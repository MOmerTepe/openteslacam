//! Minimal streaming MP4 muxer: ftyp, mdat (64-bit size patched at the end), then moov.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

pub struct Muxer {
    f: BufWriter<File>,
    /// Offset of the mdat box header.
    mdat: u64,
    pos: u64,
    timescale: u32,
    width: u16,
    height: u16,
    avcc: Vec<u8>,
    offsets: Vec<u64>,
    sizes: Vec<u32>,
    /// stts runs: (count, delta).
    stts: Vec<(u32, u32)>,
    /// ctts runs: (count, pts - dts).
    ctts: Vec<(u32, i32)>,
    /// 1-based sync sample numbers.
    keys: Vec<u32>,
}

fn invalid(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, m)
}

fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend((8 + body.len() as u32).to_be_bytes());
    v.extend(typ);
    v.extend(body);
    v
}

/// Full box; `flags` holds the version in its top byte and the 24-bit flags below it.
fn full(typ: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    bx(typ, &[&flags.to_be_bytes()[..], body].concat())
}

const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|m| m.to_be_bytes()).collect()
}

impl Muxer {
    pub fn create(path: &Path, timescale: u32, width: u32, height: u32, avcc: &[u8]) -> io::Result<Self> {
        let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height)) else {
            return Err(invalid("dimensions exceed 65535"));
        };
        let mut f = BufWriter::new(File::create(path)?);
        let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");
        f.write_all(&ftyp)?;
        let mdat = ftyp.len() as u64;
        // size==1 means a 64-bit largesize follows; patched in finish().
        f.write_all(&[0, 0, 0, 1, b'm', b'd', b'a', b't', 0, 0, 0, 0, 0, 0, 0, 0])?;
        Ok(Muxer {
            f,
            mdat,
            pos: mdat + 16,
            timescale,
            width,
            height,
            avcc: avcc.to_vec(),
            offsets: Vec::new(),
            sizes: Vec::new(),
            stts: Vec::new(),
            ctts: Vec::new(),
            keys: Vec::new(),
        })
    }

    /// `data` is one sample: AVCC length-prefixed NALs. `cts` is pts - dts (non-zero with B-frames).
    pub fn push(&mut self, data: &[u8], dur: u32, cts: i32, key: bool) -> io::Result<()> {
        let size = u32::try_from(data.len()).map_err(|_| invalid("sample too large"))?;
        self.f.write_all(data)?;
        self.offsets.push(self.pos);
        self.sizes.push(size);
        self.pos += size as u64;
        match self.stts.last_mut() {
            Some((c, d)) if *d == dur => *c += 1,
            _ => self.stts.push((1, dur)),
        }
        match self.ctts.last_mut() {
            Some((c, o)) if *o == cts => *c += 1,
            _ => self.ctts.push((1, cts)),
        }
        if key {
            self.keys.push(self.sizes.len() as u32);
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        let duration: u64 = self.stts.iter().map(|&(c, d)| c as u64 * d as u64).sum();
        let duration = u32::try_from(duration).map_err(|_| invalid("duration too long for v0 boxes"))?;
        let moov = self.moov(duration);
        self.f.write_all(&moov)?;
        self.f.seek(SeekFrom::Start(self.mdat + 8))?;
        self.f.write_all(&(self.pos - self.mdat).to_be_bytes())?;
        self.f.flush()
    }

    fn moov(&self, duration: u32) -> Vec<u8> {
        let ts = self.timescale.to_be_bytes();
        let dur = duration.to_be_bytes();
        let n = self.sizes.len() as u32;

        let mvhd = full(
            b"mvhd",
            0,
            &[
                &[0; 8][..], // creation/modification time
                &ts,
                &dur,
                &0x10000u32.to_be_bytes(), // rate 1.0
                &0x100u16.to_be_bytes(),   // volume 1.0
                &[0; 10],
                &matrix(),
                &[0; 24],
                &2u32.to_be_bytes(), // next_track_ID
            ]
            .concat(),
        );
        let tkhd = full(
            b"tkhd",
            3, // enabled | in_movie
            &[
                &[0; 8][..],
                &1u32.to_be_bytes(), // track_ID
                &[0; 4],
                &dur,
                &[0; 8],
                &[0; 8], // layer, alternate_group, volume (0 for video), reserved
                &matrix(),
                &((self.width as u32) << 16).to_be_bytes(),
                &((self.height as u32) << 16).to_be_bytes(),
            ]
            .concat(),
        );
        let mdhd = full(
            b"mdhd",
            0,
            &[&[0; 8][..], &ts, &dur, &0x55C4u16.to_be_bytes() /* "und" */, &[0; 2]].concat(),
        );
        let hdlr = full(b"hdlr", 0, &[&[0; 4][..], b"vide", &[0; 12], b"VideoHandler\0"].concat());
        let vmhd = full(b"vmhd", 1, &[0; 8]);
        let dinf = bx(
            b"dinf",
            &full(b"dref", 0, &[&1u32.to_be_bytes()[..], &full(b"url ", 1, &[])].concat()),
        );

        let avc1 = bx(
            b"avc1",
            &[
                &[0; 6][..],
                &1u16.to_be_bytes(), // data_reference_index
                &[0; 16],
                &self.width.to_be_bytes(),
                &self.height.to_be_bytes(),
                &0x48_0000u32.to_be_bytes(), // 72 dpi
                &0x48_0000u32.to_be_bytes(),
                &[0; 4],
                &1u16.to_be_bytes(), // frame_count
                &[0; 32],            // compressorname
                &0x18u16.to_be_bytes(),
                &0xFFFFu16.to_be_bytes(), // pre_defined = -1
                &bx(b"avcC", &self.avcc),
            ]
            .concat(),
        );
        let stsd = full(b"stsd", 0, &[&1u32.to_be_bytes()[..], &avc1].concat());
        let table = |count: usize, rows: Vec<u8>| [&(count as u32).to_be_bytes()[..], &rows].concat();
        let stts = full(
            b"stts",
            0,
            &table(self.stts.len(), self.stts.iter().flat_map(|&(c, d)| [c, d]).flat_map(u32::to_be_bytes).collect()),
        );
        // Version 1: signed offsets, so pts can be presentation time without an edit list.
        let ctts = if self.ctts.iter().any(|&(_, o)| o != 0) {
            full(b"ctts", 1 << 24, &table(self.ctts.len(), self.ctts.iter().flat_map(|&(c, o)| [c.to_be_bytes(), o.to_be_bytes()]).flatten().collect()))
        } else {
            Vec::new()
        };
        let stss = full(b"stss", 0, &table(self.keys.len(), self.keys.iter().flat_map(|k| k.to_be_bytes()).collect()));
        let stsz = full(
            b"stsz",
            0,
            &[&[0; 4][..], &table(self.sizes.len(), self.sizes.iter().flat_map(|s| s.to_be_bytes()).collect())]
                .concat(),
        );
        // One sample per chunk: a single stsc run covers every chunk.
        let stsc = full(b"stsc", 0, &table(1, [1u32, 1, 1].iter().flat_map(|x| x.to_be_bytes()).collect()));
        let co64 = full(b"co64", 0, &table(n as usize, self.offsets.iter().flat_map(|o| o.to_be_bytes()).collect()));

        let stbl = bx(b"stbl", &[stsd, stts, ctts, stss, stsz, stsc, co64].concat());
        let minf = bx(b"minf", &[vmhd, dinf, stbl].concat());
        let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
        let trak = bx(b"trak", &[tkhd, mdia].concat());
        bx(b"moov", &[mvhd, trak].concat())
    }
}
