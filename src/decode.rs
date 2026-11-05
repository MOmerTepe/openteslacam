//! openh264 wrapper: AVCC samples from `mp4::Track` in, I420 frames out.

use crate::mp4::Track;
use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;

const START: [u8; 4] = [0, 0, 0, 1];

pub struct CamDecoder {
    dec: Decoder,
    /// SPS and PPS in Annex-B form, sent before every key frame so decoding can start at any of them.
    params: Vec<u8>,
    nal_len: usize,
    /// Reused Annex-B buffer for the sample being decoded.
    buf: Vec<u8>,
}

impl CamDecoder {
    pub fn new(track: &Track) -> Result<Self, String> {
        let mut params = Vec::new();
        for p in track.sps.iter().chain(&track.pps) {
            params.extend(START);
            params.extend(p);
        }
        // The default auto-flush after a packet with no picture corrupts reordering (B-frame)
        // streams; pictures still buffered come out of flush() at end of stream instead.
        let cfg = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        let dec = Decoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| format!("openh264: {e}"))?;
        Ok(CamDecoder { dec, params, nal_len: track.nal_len as usize, buf: Vec::new() })
    }

    /// Decodes one sample; `None` means the decoder has no picture ready yet.
    pub fn decode(&mut self, sample: &[u8], key: bool) -> Result<Option<DecodedYUV<'_>>, String> {
        self.buf.clear();
        if key {
            self.buf.extend(&self.params);
        }
        let n = self.nal_len;
        let mut pos = 0;
        while pos + n <= sample.len() {
            let len = sample[pos..pos + n].iter().fold(0usize, |a, &b| a << 8 | b as usize);
            pos += n;
            let nal = sample.get(pos..pos + len).ok_or("NAL length runs past the sample")?;
            pos += len;
            self.buf.extend(START);
            self.buf.extend(nal);
        }
        self.dec.decode(&self.buf).map_err(|e| format!("openh264: {e}"))
    }

    /// Pictures still held by the decoder; call after the last sample.
    pub fn flush(&mut self) -> Result<Vec<DecodedYUV<'_>>, String> {
        self.dec.flush_remaining().map_err(|e| format!("openh264: {e}"))
    }
}

/// Writes the picture into `out` as RGBA (resized as needed) and returns its (width, height).
pub fn to_rgba(yuv: &DecodedYUV, out: &mut Vec<u8>) -> (u32, u32) {
    let (w, h) = yuv.dimensions();
    out.resize(w * h * 4, 0);
    yuv.write_rgba8(out);
    (w as u32, h as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4;
    use crate::mux::Muxer;
    use std::process::Command;

    fn ffmpeg(args: &[&str]) -> Option<Vec<u8>> {
        let out = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).output().ok()?;
        assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        Some(out.stdout)
    }

    /// x264 High profile with B-frames (openh264's own encoder only makes Baseline). Needs ffmpeg
    /// on PATH; skipped otherwise.
    #[test]
    fn b_frames_decode_and_remux() {
        let dir = std::env::temp_dir();
        let src = dir.join(format!("otc-bf-{}.mp4", std::process::id()));
        let out = dir.join(format!("otc-bf-remux-{}.mp4", std::process::id()));
        let (s, o) = (src.to_str().unwrap(), out.to_str().unwrap());
        let input = ["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30", "-frames:v", "60"];
        let x264 = ["-c:v", "libx264", "-profile:v", "high", "-bf", "2", "-g", "30", s];
        if ffmpeg(&[&input[..], &x264].concat()).is_none() {
            println!("skipped: ffmpeg not on PATH");
            return;
        }
        let t = mp4::read_track(&src).unwrap();
        assert!(t.samples.iter().any(|s| s.pts != s.dts), "fixture has no B-frames");

        // Edit list applied: presentation times are exactly 0, d, 2d, ...
        let mut pts: Vec<u64> = t.samples.iter().map(|s| s.pts).collect();
        pts.sort();
        let d = t.samples[0].dur as u64;
        assert_eq!(pts, (0..60).map(|i| i * d).collect::<Vec<_>>());

        let mut f = std::fs::File::open(&src).unwrap();
        let mut dec = CamDecoder::new(&t).unwrap();
        let mut m = Muxer::create(&out, t.timescale, t.width, t.height, &t.avcc).unwrap();
        let mut frames = 0;
        for s in &t.samples {
            let data = mp4::read_sample(&mut f, s).unwrap();
            frames += dec.decode(&data, s.key).unwrap().is_some() as usize;
            m.push(&data, s.dur, (s.pts as i64 - s.dts as i64) as i32, s.key).unwrap();
        }
        frames += dec.flush().unwrap().len();
        m.finish().unwrap();
        assert_eq!(frames, 60);

        let back = mp4::read_track(&out).unwrap();
        let key = |s: &mp4::Sample| (s.size, s.dts, s.pts, s.dur, s.key);
        assert!(back.samples.iter().map(key).eq(t.samples.iter().map(key)));
        // ffmpeg agrees: same decoded pictures with the same presentation times.
        let md5 = |p: &str| ffmpeg(&["-i", p, "-f", "framemd5", "-"]).unwrap();
        assert_eq!(md5(o), md5(s));
        std::fs::remove_file(&src).unwrap();
        std::fs::remove_file(&out).unwrap();
    }
}
