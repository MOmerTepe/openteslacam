//! Hand-rolled demuxer for Tesla's plain (non-fragmented) single-video-track H.264 MP4s.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Track timescale. dts is raw decode time; pts is presentation time:
    /// dts + ctts offset - edit list media_time.
    pub dts: u64,
    pub pts: u64,
    pub dur: u32,
    pub key: bool,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub timescale: u32,
    pub width: u32,
    pub height: u32,
    /// Raw avcC payload (AVCDecoderConfigurationRecord).
    pub avcc: Vec<u8>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
    /// Bytes in each NAL length prefix (usually 4).
    pub nal_len: u8,
    pub samples: Vec<Sample>,
    /// Sum of stts, in timescale units.
    pub duration: u64,
}

#[derive(Debug)]
pub enum Mp4Error {
    Io(io::Error),
    Encrypted,
    NoVideo,
    Malformed(String),
}

impl fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Mp4Error::Io(e) => write!(f, "I/O error: {e}"),
            Mp4Error::Encrypted => f.write_str("not a readable MP4 (encrypted clip?)"),
            Mp4Error::NoVideo => f.write_str("no video track"),
            Mp4Error::Malformed(m) => write!(f, "malformed MP4: {m}"),
        }
    }
}

impl std::error::Error for Mp4Error {}

impl From<io::Error> for Mp4Error {
    fn from(e: io::Error) -> Self {
        Mp4Error::Io(e)
    }
}

fn bad(m: impl Into<String>) -> Mp4Error {
    Mp4Error::Malformed(m.into())
}

/// Bounds-checked big-endian reader over a box payload.
struct R<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    fn new(d: &'a [u8]) -> Self {
        R { d, p: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Mp4Error> {
        if n > self.d.len() - self.p {
            return Err(bad("box too short"));
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, Mp4Error> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, Mp4Error> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, Mp4Error> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, Mp4Error> {
        Ok((self.u32()? as u64) << 32 | self.u32()? as u64)
    }
    /// Table of `count` entries of `w` bytes; checks the data exists before anyone allocates.
    fn table(&mut self, count: u32, w: usize) -> Result<R<'a>, Mp4Error> {
        Ok(R::new(self.take(count as usize * w)?))
    }
}

type Boxes<'a> = Vec<([u8; 4], &'a [u8])>;

/// Splits a container payload into (type, payload) children.
fn children(mut d: &[u8]) -> Result<Boxes<'_>, Mp4Error> {
    let mut out = Vec::new();
    // Trailing <8 bytes (e.g. a zero terminator some muxers add) are not a box.
    while d.len() >= 8 {
        let mut r = R::new(d);
        let mut size = r.u32()? as u64;
        let typ = r.u32()?.to_be_bytes();
        if size == 1 {
            size = r.u64()?;
        } else if size == 0 {
            size = d.len() as u64;
        }
        if size < r.p as u64 || size > d.len() as u64 {
            return Err(bad(format!("bad size for '{}' box", String::from_utf8_lossy(&typ))));
        }
        out.push((typ, &d[r.p..size as usize]));
        d = &d[size as usize..];
    }
    Ok(out)
}

fn find<'a>(d: &'a [u8], t: &[u8; 4]) -> Result<Option<&'a [u8]>, Mp4Error> {
    Ok(children(d)?.into_iter().find(|(k, _)| k == t).map(|(_, v)| v))
}

fn child<'a>(d: &'a [u8], t: &[u8; 4]) -> Result<&'a [u8], Mp4Error> {
    find(d, t)?.ok_or_else(|| bad(format!("missing '{}' box", String::from_utf8_lossy(t))))
}

pub fn read_track(path: &Path) -> Result<Track, Mp4Error> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let moov = read_moov(&mut f, len)?;
    for (typ, trak) in children(&moov)? {
        if &typ != b"trak" {
            continue;
        }
        let hdlr = child(child(trak, b"mdia")?, b"hdlr")?;
        if hdlr.get(8..12) == Some(b"vide") {
            return parse_video(trak, len);
        }
    }
    Err(Mp4Error::NoVideo)
}

/// Walks top-level boxes by seeking (mdat can be huge) and returns the moov payload.
fn read_moov(f: &mut File, len: u64) -> Result<Vec<u8>, Mp4Error> {
    let mut pos = 0u64;
    while len - pos >= 8 {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 8];
        f.read_exact(&mut h)?;
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let typ = &h[4..8];
        if pos == 0 && typ != b"ftyp" {
            return Err(Mp4Error::Encrypted);
        }
        let mut hl = 8;
        if size == 1 {
            let mut l = [0u8; 8];
            f.read_exact(&mut l)?;
            size = u64::from_be_bytes(l);
            hl = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < hl || size > len - pos {
            return Err(bad("top-level box runs past end of file (truncated?)"));
        }
        if typ == b"moov" {
            // A one-minute clip's moov is tens of KB; don't let a bogus size allocate gigabytes.
            if size - hl > MAX_MOOV {
                return Err(bad("moov box too large"));
            }
            let mut m = vec![0u8; (size - hl) as usize];
            f.read_exact(&mut m)?;
            return Ok(m);
        }
        pos += size;
    }
    // Tesla writes moov last, so a recording cut short (power loss) starts with ftyp but has none.
    Err(bad("no moov box (truncated recording?)"))
}

const MAX_MOOV: u64 = 256 << 20;
/// About 129 h at 36 fps; caps what a tiny crafted moov can make us allocate (~55 B/sample).
const MAX_SAMPLES: u32 = 1 << 24;

fn parse_video(trak: &[u8], file_len: u64) -> Result<Track, Mp4Error> {
    let mdia = child(trak, b"mdia")?;
    let mut r = R::new(child(mdia, b"mdhd")?);
    let version = r.u8()?;
    r.take(3)?;
    let timescale = if version == 1 {
        r.take(16)?;
        r.u32()?
    } else {
        r.take(8)?;
        r.u32()?
    };
    if timescale == 0 {
        return Err(bad("zero timescale"));
    }

    let stbl = child(child(mdia, b"minf")?, b"stbl")?;

    // stsd -> avc1 -> avcC
    let mut r = R::new(child(stbl, b"stsd")?);
    r.take(8)?; // version/flags, entry_count
    let entries = children(&r.d[r.p..])?;
    let (typ, avc1) = entries.first().ok_or_else(|| bad("empty stsd"))?;
    if typ != b"avc1" {
        return Err(bad(format!("unsupported codec '{}'", String::from_utf8_lossy(typ))));
    }
    let mut r = R::new(avc1);
    r.take(24)?; // reserved, data_reference_index, pre_defined/reserved
    let width = r.u16()? as u32;
    let height = r.u16()? as u32;
    r.take(50)?; // resolutions, frame_count, compressorname, depth, pre_defined
    let avcc = child(&avc1[r.p..], b"avcC")?.to_vec();
    let mut r = R::new(&avcc);
    r.take(4)?; // version, profile, compat, level
    let nal_len = (r.u8()? & 3) + 1;
    let mut sps = Vec::new();
    for _ in 0..r.u8()? & 0x1F {
        let n = r.u16()? as usize;
        sps.push(r.take(n)?.to_vec());
    }
    let mut pps = Vec::new();
    for _ in 0..r.u8()? {
        let n = r.u16()? as usize;
        pps.push(r.take(n)?.to_vec());
    }

    // stsz
    let mut r = R::new(child(stbl, b"stsz")?);
    r.take(4)?;
    let uniform = r.u32()?;
    let count = r.u32()?;
    if count > MAX_SAMPLES {
        return Err(bad("too many samples"));
    }
    let sizes: Vec<u32> = if uniform == 0 {
        let mut t = r.table(count, 4)?;
        (0..count).map(|_| t.u32()).collect::<Result<_, _>>()?
    } else {
        if count as u64 * uniform as u64 > file_len {
            return Err(bad("stsz larger than file"));
        }
        vec![uniform; count as usize]
    };
    let n = sizes.len();

    // stts -> per-sample durations
    let mut r = R::new(child(stbl, b"stts")?);
    r.take(4)?;
    let entries = r.u32()?;
    let mut t = r.table(entries, 8)?;
    let mut durs = Vec::with_capacity(n);
    for _ in 0..entries {
        let (c, d) = (t.u32()?, t.u32()?);
        let c = (c as usize).min(n - durs.len());
        durs.extend(std::iter::repeat_n(d, c));
    }
    if durs.len() < n {
        return Err(bad("stts covers fewer samples than stsz"));
    }

    // ctts (optional): composition offsets, treated as signed like most demuxers do.
    let mut cts = vec![0i64; n];
    if let Some(ctts) = find(stbl, b"ctts")? {
        let mut r = R::new(ctts);
        r.take(4)?;
        let entries = r.u32()?;
        let mut t = r.table(entries, 8)?;
        let mut i = 0;
        for _ in 0..entries {
            let (c, off) = (t.u32()?, t.u32()? as i32 as i64);
            for slot in cts.iter_mut().skip(i).take(c as usize) {
                *slot = off;
            }
            i = i.saturating_add(c as usize);
        }
    }

    // edts/elst (optional): media time shown at presentation 0. B-frame encoders pair ctts with
    // media_time = first offset so the first frame lands at 0.
    // shortcut: only the first non-empty edit is honoured (no start delays, splices or rates).
    let mut shift = 0i64;
    let elst = match find(trak, b"edts")? {
        Some(edts) => find(edts, b"elst")?,
        None => None,
    };
    if let Some(elst) = elst {
        let mut r = R::new(elst);
        let v1 = r.u8()? == 1;
        r.take(3)?;
        let entries = r.u32()?;
        let mut t = r.table(entries, if v1 { 20 } else { 12 })?;
        for _ in 0..entries {
            let media_time = if v1 {
                t.u64()?;
                t.u64()? as i64
            } else {
                t.u32()?;
                t.u32()? as i32 as i64
            };
            t.u32()?; // media_rate
            if media_time != -1 {
                shift = media_time;
                break;
            }
        }
    }

    // stss (optional): absent means every sample is a sync sample.
    let mut keys = vec![true; n];
    if let Some(stss) = find(stbl, b"stss")? {
        keys.fill(false);
        let mut r = R::new(stss);
        r.take(4)?;
        let entries = r.u32()?;
        let mut t = r.table(entries, 4)?;
        for _ in 0..entries {
            if let Some(k) = (t.u32()? as usize).checked_sub(1).and_then(|i| keys.get_mut(i)) {
                *k = true;
            }
        }
    }

    // stco/co64 chunk offsets
    let chunks: Vec<u64> = if let Some(stco) = find(stbl, b"stco")? {
        let mut r = R::new(stco);
        r.take(4)?;
        let c = r.u32()?;
        let mut t = r.table(c, 4)?;
        (0..c).map(|_| t.u32().map(u64::from)).collect::<Result<_, _>>()?
    } else {
        let mut r = R::new(child(stbl, b"co64")?);
        r.take(4)?;
        let c = r.u32()?;
        let mut t = r.table(c, 8)?;
        (0..c).map(|_| t.u64()).collect::<Result<_, _>>()?
    };

    // stsc: (first_chunk 1-based, samples_per_chunk, desc index)
    let mut r = R::new(child(stbl, b"stsc")?);
    r.take(4)?;
    let entries = r.u32()?;
    let mut t = r.table(entries, 12)?;
    let mut stsc = Vec::with_capacity(entries as usize);
    for _ in 0..entries {
        let (first_chunk, per_chunk) = (t.u32()?, t.u32()?);
        t.u32()?;
        stsc.push((first_chunk, per_chunk));
    }

    let mut samples = Vec::with_capacity(n);
    let mut dts = 0u64;
    let mut e = 0;
    'chunks: for (ci, &chunk_off) in chunks.iter().enumerate() {
        let chunk_no = ci as u64 + 1;
        while e + 1 < stsc.len() && stsc[e + 1].0 as u64 <= chunk_no {
            e += 1;
        }
        let spc = stsc.get(e).ok_or_else(|| bad("empty stsc"))?.1;
        let mut off = chunk_off;
        for _ in 0..spc {
            let i = samples.len();
            if i == n {
                break 'chunks;
            }
            let size = sizes[i];
            let end = off.checked_add(size as u64).filter(|&end| end <= file_len);
            let Some(end) = end else {
                return Err(bad("sample data past end of file"));
            };
            let dur = durs[i];
            let pts = (dts as i64).saturating_add(cts[i]).saturating_sub(shift).max(0) as u64;
            samples.push(Sample { offset: off, size, dts, pts, dur, key: keys[i] });
            dts += dur as u64;
            off = end;
        }
    }
    if samples.len() < n {
        return Err(bad("chunks hold fewer samples than stsz"));
    }

    Ok(Track { timescale, width, height, avcc, sps, pps, nal_len, samples, duration: dts })
}

pub fn read_sample(f: &mut File, s: &Sample) -> io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(s.offset))?;
    let mut buf = vec![0u8; s.size as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::Muxer;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("otc-mp4-{}-{name}.mp4", std::process::id()))
    }

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x50];
    const PPS: &[u8] = &[0x68, 0xEB, 0xE3, 0xCB];

    fn avcc() -> Vec<u8> {
        let mut v = vec![1, 0x64, 0x00, 0x28, 0xFF, 0xE1];
        v.extend((SPS.len() as u16).to_be_bytes());
        v.extend(SPS);
        v.push(1);
        v.extend((PPS.len() as u16).to_be_bytes());
        v.extend(PPS);
        v
    }

    /// Length-prefixed NALs: an SEI-ish NAL plus a slice of varying size.
    fn sample(i: usize, key: bool) -> Vec<u8> {
        let slice: Vec<u8> = std::iter::once(if key { 0x65 } else { 0x41 })
            .chain((0..(10 + i * 7)).map(|b| (b * 31 + i) as u8))
            .collect();
        let sei = [0x06, 0x05, i as u8, 0x80];
        let mut v = Vec::new();
        for nal in [&sei[..], &slice[..]] {
            v.extend((nal.len() as u32).to_be_bytes());
            v.extend(nal);
        }
        v
    }

    // (dur, pts - dts, key) per sample: mixed durations exercise stts runs, the offsets a
    // B-frame pattern (I P B with presentation-time pts, so one offset is negative).
    const PLAN: &[(u32, i32, bool)] = &[
        (3000, 0, true),
        (3000, 3000, false),
        (3000, -3000, false),
        (3003, 0, false),
        (3000, 0, true),
        (2997, 0, false),
    ];

    fn write_clip(path: &Path) -> Vec<Vec<u8>> {
        let mut m = Muxer::create(path, 90000, 1280, 960, &avcc()).unwrap();
        let data: Vec<Vec<u8>> = PLAN.iter().enumerate().map(|(i, &(_, _, k))| sample(i, k)).collect();
        for (d, &(dur, cts, key)) in data.iter().zip(PLAN) {
            m.push(d, dur, cts, key).unwrap();
        }
        m.finish().unwrap();
        data
    }

    #[test]
    fn roundtrip() {
        let path = tmp("roundtrip");
        let data = write_clip(&path);
        let t = read_track(&path).unwrap();
        assert_eq!((t.timescale, t.width, t.height, t.nal_len), (90000, 1280, 960, 4));
        assert_eq!(t.avcc, avcc());
        assert_eq!(t.sps, vec![SPS.to_vec()]);
        assert_eq!(t.pps, vec![PPS.to_vec()]);
        assert_eq!(t.samples.len(), PLAN.len());
        assert_eq!(t.duration, PLAN.iter().map(|p| p.0 as u64).sum::<u64>());

        // ftyp (32 bytes) + 16-byte mdat header, then samples back to back.
        let mut off = 32 + 16;
        let mut dts = 0;
        let mut f = File::open(&path).unwrap();
        for ((s, d), &(dur, cts, key)) in t.samples.iter().zip(&data).zip(PLAN) {
            let pts = (dts as i64 + cts as i64) as u64;
            let want = Sample { offset: off, size: d.len() as u32, dts, pts, dur, key };
            assert_eq!(*s, want);
            assert_eq!(read_sample(&mut f, s).unwrap(), *d);
            off += d.len() as u64;
            dts += dur as u64;
        }
        std::fs::remove_file(&path).unwrap();
    }

    /// Inserts `edts` (one elst v0 entry) after tkhd, growing the trak and moov sizes to match.
    fn add_edit_list(file: &[u8], media_time: i32) -> Vec<u8> {
        let elst = [&0u32.to_be_bytes()[..], &1u32.to_be_bytes(), &[0; 4], &media_time.to_be_bytes(), &0x10000u32.to_be_bytes()]
            .concat();
        let bx = |t: &[u8], b: &[u8]| [&(8 + b.len() as u32).to_be_bytes()[..], t, b].concat();
        let edts = bx(b"edts", &bx(b"elst", &elst));
        let at = |t: &[u8]| file.windows(4).position(|w| w == t).unwrap() - 4;
        let size = |p: usize| u32::from_be_bytes(file[p..p + 4].try_into().unwrap());
        let tkhd_end = at(b"tkhd") + size(at(b"tkhd")) as usize;
        let mut out = [&file[..tkhd_end], &edts, &file[tkhd_end..]].concat();
        for p in [at(b"moov"), at(b"trak")] {
            out[p..p + 4].copy_from_slice(&(size(p) + edts.len() as u32).to_be_bytes());
        }
        out
    }

    #[test]
    fn edit_list_shifts_pts() {
        let path = tmp("elst");
        write_clip(&path);
        let plain = read_track(&path).unwrap();
        std::fs::write(&path, add_edit_list(&std::fs::read(&path).unwrap(), 3000)).unwrap();
        let t = read_track(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        for (a, b) in plain.samples.iter().zip(&t.samples) {
            assert_eq!(b.pts, a.pts.saturating_sub(3000));
            assert_eq!((b.dts, b.offset), (a.dts, a.offset));
        }
    }

    #[test]
    fn huge_sample_count_is_rejected() {
        // A uniform stsz with a giant count used to expand to ~55 bytes per sample in memory.
        let path = tmp("huge");
        write_clip(&path);
        let mut b = std::fs::read(&path).unwrap();
        let stsz = b.windows(4).position(|w| w == b"stsz").unwrap();
        b[stsz + 8..stsz + 16].copy_from_slice(&[0, 0, 0, 1, 0x01, 0, 0, 1]); // size 1, 2^24 + 1 samples
        std::fs::write(&path, &b).unwrap();
        File::options().write(true).open(&path).unwrap().set_len((1 << 24) + 4096).unwrap();
        let r = read_track(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(r, Err(Mp4Error::Malformed(m)) if m == "too many samples"));
    }

    #[test]
    fn no_moov_is_truncated_not_encrypted() {
        let path = tmp("nomoov");
        write_clip(&path);
        let b = std::fs::read(&path).unwrap();
        let moov = b.windows(4).position(|w| w == b"moov").unwrap() - 4;
        std::fs::write(&path, &b[..moov]).unwrap();
        let r = read_track(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(r, Err(Mp4Error::Malformed(m)) if m.contains("truncated")));
    }

    #[test]
    fn garbage_is_encrypted() {
        let path = tmp("garbage");
        let mut x = 0x2545F4914F6CDD1Du64;
        let bytes: Vec<u8> = (0..100_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(read_track(&path), Err(Mp4Error::Encrypted)));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn truncated_and_corrupt_never_panic() {
        let path = tmp("trunc");
        write_clip(&path);
        let full = std::fs::read(&path).unwrap();
        for cut in 0..full.len() {
            std::fs::write(&path, &full[..cut]).unwrap();
            assert!(read_track(&path).is_err(), "cut at {cut} parsed");
        }
        // Corrupt each moov byte in turn; any result is fine as long as nothing panics.
        let moov = full.windows(4).position(|w| w == b"moov").unwrap() - 4;
        for i in moov..full.len() {
            let mut b = full.clone();
            b[i] ^= 0xFF;
            std::fs::write(&path, &b).unwrap();
            let _ = read_track(&path);
        }
        std::fs::remove_file(&path).unwrap();
    }
}
