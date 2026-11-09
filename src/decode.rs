//! openh264 wrapper (AVCC samples from `mp4::Track` in, I420 frames out) and the per-camera
//! decoder thread that follows the player's clock.

use crate::mp4::{self, Sample, Track};
use eframe::egui::{ColorImage, Context};
use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use std::fs::File;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Paused, seeks, frame steps: only the frame at the target is converted and shown.
    Exact,
    /// Playing: like Exact while the decoder keeps up. When it lags, the newest decoded frame is
    /// shown about once per display frame, and it skips to a keyframe once too far behind.
    Play,
    /// Scrubber drag: the keyframe at or before the target, nothing decoded after it.
    Keyframes,
}

pub struct FrameOut {
    pub seg: usize,
    /// Index into that segment's `Track::samples` (decode order).
    pub sample: usize,
    pub image: ColorImage,
}

/// Latest-only hand-off to the UI. `Err` says why the camera has no picture for the target
/// segment (no clip, unreadable clip).
pub type Slot = Arc<Mutex<Option<Result<FrameOut, String>>>>;

enum Cmd {
    Open(Vec<Option<PathBuf>>),
    Seek(usize, f64, Mode),
    Shutdown,
}

/// One camera's decoder thread. Dropping it stops and joins the thread.
pub struct CamWorker {
    tx: Sender<Cmd>,
    slot: Slot,
    thread: Option<JoinHandle<()>>,
}

impl CamWorker {
    /// `ctx` gets a repaint request whenever a new result is ready.
    pub fn spawn(name: &str, ctx: Context) -> CamWorker {
        let (tx, rx) = channel();
        let slot = Slot::default();
        let w = Worker::new(slot.clone(), ctx);
        let thread = thread::Builder::new().name(format!("decode-{name}")).spawn(move || w.run(rx)).expect("spawn decoder thread");
        CamWorker { tx, slot, thread: Some(thread) }
    }

    /// This camera's clip per segment (`None` where it didn't record). Forgets the old event.
    pub fn open(&self, files: Vec<Option<PathBuf>>) {
        let _ = self.tx.send(Cmd::Open(files));
    }

    /// Show the latest frame with pts <= `local` seconds into segment `seg`. Cheap: send it every
    /// UI frame; repeating the current target does nothing.
    pub fn seek(&self, seg: usize, local: f64, mode: Mode) {
        let _ = self.tx.send(Cmd::Seek(seg, local, mode));
    }

    /// The newest result since the last call, if any.
    pub fn take(&self) -> Option<Result<FrameOut, String>> {
        self.slot.lock().ok()?.take()
    }
}

impl Drop for CamWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A Play-mode camera further behind than this (seconds) jumps to the latest keyframe <= target
/// instead of decoding every frame: an HW4 front stream decodes at only ~1.7x realtime.
// shortcut: frames get dropped at 2x/4x on slow CPUs, upgrade with hardware decode.
const CATCH_UP: f64 = 0.5;
/// While lagging in Play mode, convert and show a frame at most this often.
const LAG_REFRESH: Duration = Duration::from_millis(25);

/// Display-order target: the sample with the largest pts <= `pts`, else the earliest one.
pub(crate) fn target_sample(samples: &[Sample], pts: u64) -> usize {
    let mut best: Option<usize> = None;
    let mut first = 0;
    for (i, s) in samples.iter().enumerate() {
        if s.pts <= pts && best.is_none_or(|b| s.pts > samples[b].pts) {
            best = Some(i);
        }
        if s.pts < samples[first].pts {
            first = i;
        }
    }
    best.unwrap_or(first)
}

/// The keyframe that decoding sample `i` has to start from (decode order).
pub(crate) fn key_before(samples: &[Sample], i: usize) -> usize {
    (0..=i).rev().find(|&k| samples[k].key).unwrap_or(0)
}

/// Whether to restart decoding at keyframe `key` rather than keep feeding from `next_feed`.
/// `last_out`: pts of the decoder's last picture; `behind`: seconds from the decoder's position
/// to the target.
pub(crate) fn must_seek(mode: Mode, key: usize, next_feed: usize, tgt_pts: u64, last_out: Option<u64>, behind: f64) -> bool {
    // The target is behind the decoder: only its keyframe leads there.
    if last_out.is_some_and(|p| p >= tgt_pts) {
        return true;
    }
    // Frames before a later keyframe are worth decoding only if they get shown.
    key > next_feed && (mode != Mode::Play || behind > CATCH_UP)
}

struct Loaded {
    track: Track,
    file: File,
}

fn load(files: &[Option<PathBuf>], seg: usize) -> Result<Loaded, String> {
    let p = files.get(seg).and_then(Option::as_ref).ok_or("no clip")?;
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", p.display());
    let track = mp4::read_track(p).map_err(|e| err(&e))?;
    if track.samples.is_empty() {
        return Err(err(&"no frames"));
    }
    Ok(Loaded { file: File::open(p).map_err(|e| err(&e))?, track })
}

/// Decoder progress within the loaded segment.
#[derive(Default)]
struct Feed {
    /// Next sample (decode order) to send to the decoder.
    next: usize,
    /// Sent but not yet out of the decoder, as (pts, sample). Pictures come out in pts order.
    pending: Vec<(u64, usize)>,
    last_out: Option<u64>,
}

struct Publisher {
    slot: Slot,
    ctx: Context,
    rgba: Vec<u8>,
    shown: Option<(usize, usize)>,
    at: Instant,
}

impl Publisher {
    fn put(&mut self, r: Result<FrameOut, String>) {
        self.shown = r.as_ref().ok().map(|f| (f.seg, f.sample));
        self.at = Instant::now();
        if let Ok(mut s) = self.slot.lock() {
            *s = Some(r);
        }
        self.ctx.request_repaint();
    }

    fn frame(&mut self, seg: usize, sample: usize, yuv: &DecodedYUV) {
        let (w, h) = to_rgba(yuv, &mut self.rgba);
        let image = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &self.rgba);
        self.put(Ok(FrameOut { seg, sample, image }));
    }
}

struct Target {
    seg: usize,
    sample: usize,
    pts: u64,
    mode: Mode,
}

/// Matches a decoded picture to its sample and shows it if wanted; true once the target is out.
fn picture(yuv: &DecodedYUV, feed: &mut Feed, out: &mut Publisher, t: &Target) -> bool {
    let Some(i) = (0..feed.pending.len()).min_by_key(|&i| feed.pending[i].0) else { return false };
    let (pts, sample) = feed.pending.swap_remove(i);
    feed.last_out = Some(pts);
    // `pts >= t.pts` also ends the search when a damaged stream lost the target picture.
    let reached = sample == t.sample || pts >= t.pts;
    if reached || (t.mode == Mode::Play && out.at.elapsed() >= LAG_REFRESH) {
        out.frame(t.seg, sample, yuv);
    }
    reached
}

struct Worker {
    files: Vec<Option<PathBuf>>,
    cur: Option<(usize, Result<Loaded, String>)>,
    /// The following segment, opened ahead so a boundary doesn't wait on disk.
    next: Option<(usize, Result<Loaded, String>)>,
    /// None: the next step restarts from a keyframe.
    dec: Option<CamDecoder>,
    feed: Feed,
    out: Publisher,
    target: Option<(usize, f64, Mode)>,
    /// Nothing to do until the next command.
    idle: bool,
}

impl Worker {
    fn new(slot: Slot, ctx: Context) -> Worker {
        let out = Publisher { slot, ctx, rgba: Vec::new(), shown: None, at: Instant::now() };
        Worker { files: Vec::new(), cur: None, next: None, dec: None, feed: Feed::default(), out, target: None, idle: true }
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            let first = if self.idle {
                match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                }
            } else {
                None
            };
            for cmd in first.into_iter().chain(rx.try_iter()) {
                match cmd {
                    Cmd::Shutdown => return,
                    Cmd::Open(files) => {
                        self.files = files;
                        (self.cur, self.next, self.dec, self.target, self.out.shown) = (None, None, None, None, None);
                        if let Ok(mut s) = self.out.slot.lock() {
                            *s = None;
                        }
                        self.idle = true;
                    }
                    Cmd::Seek(seg, local, mode) => {
                        self.target = Some((seg, local, mode));
                        self.idle = false;
                    }
                }
            }
            if !self.idle {
                self.step();
            }
        }
    }

    /// At most one sample's worth of decoding towards the target, so new commands are picked up
    /// between samples.
    fn step(&mut self) {
        let Some((seg, local, mode)) = self.target else {
            self.idle = true;
            return;
        };
        if self.cur.as_ref().is_none_or(|c| c.0 != seg) {
            let l = match self.next.take() {
                Some((s, l)) if s == seg => l,
                _ => load(&self.files, seg),
            };
            if let Err(e) = &l {
                self.out.put(Err(e.clone()));
            }
            self.cur = Some((seg, l));
            self.dec = None;
            self.next = (seg + 1 < self.files.len()).then(|| (seg + 1, load(&self.files, seg + 1)));
        }
        let Some((_, Ok(l))) = &mut self.cur else {
            self.idle = true;
            return;
        };
        let samples = &l.track.samples;
        let ts = l.track.timescale.max(1) as f64;
        // The epsilon keeps a time computed as pts / timescale from rounding below that pts.
        let mut sample = target_sample(samples, (local.max(0.0) * ts + 1e-6) as u64);
        if mode == Mode::Keyframes {
            sample = key_before(samples, sample);
        }
        if self.out.shown == Some((seg, sample)) {
            self.idle = true;
            return;
        }
        let t = Target { seg, sample, pts: samples[sample].pts, mode };
        let key = key_before(samples, sample);
        let fed = samples[self.feed.next.min(samples.len() - 1)].pts;
        let behind = t.pts.saturating_sub(fed) as f64 / ts;
        if self.dec.is_none() || must_seek(mode, key, self.feed.next, t.pts, self.feed.last_out, behind) {
            match CamDecoder::new(&l.track) {
                Ok(d) => self.dec = Some(d),
                Err(e) => {
                    self.out.put(Err(e));
                    self.idle = true;
                    return;
                }
            }
            self.feed = Feed { next: key, ..Feed::default() };
        }
        let Some(dec) = self.dec.as_mut() else { return };

        let mut reached = false;
        if self.feed.next < samples.len() {
            let i = self.feed.next;
            self.feed.next += 1;
            let s = &samples[i];
            let data = match mp4::read_sample(&mut l.file, s) {
                Ok(d) => d,
                Err(e) => {
                    self.out.put(Err(format!("read: {e}")));
                    self.idle = true;
                    return;
                }
            };
            self.feed.pending.push((s.pts, i));
            match dec.decode(&data, s.key) {
                Ok(Some(yuv)) => reached = picture(&yuv, &mut self.feed, &mut self.out, &t),
                Ok(None) => {}
                // A damaged frame yields no picture; forget it so later pictures still match.
                Err(_) => self.feed.pending.retain(|p| p.1 != i),
            }
        }
        // Keyframes mode feeds nothing past the keyframe, and at the clip's end there is nothing
        // more to feed: either way the reorder buffer has to give up what it holds.
        let fed_all = self.feed.next >= samples.len() || (mode == Mode::Keyframes && self.feed.next > sample);
        if !reached && fed_all {
            for yuv in dec.flush().unwrap_or_default() {
                reached |= picture(&yuv, &mut self.feed, &mut self.out, &t);
            }
            // Decoding on after a flush isn't relied on: the next step restarts at a keyframe.
            self.dec = None;
            // Not reached even so (damaged clip): wait for a new target instead of spinning.
            self.idle = true;
        }
        if reached {
            self.idle = true;
        }
    }
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

        // The worker maps reordered pictures back to their samples (pts order, not decode order).
        let w = CamWorker::spawn("bf", Context::default());
        w.open(vec![Some(src.clone())]);
        for k in [7u64, 3, 45, 59] {
            let want = t.samples.iter().position(|s| s.pts == k * d).unwrap();
            assert_eq!(wait(&w, 0, (k * d) as f64 / t.timescale as f64, Mode::Exact), Ok((0, want)), "frame {k}");
        }
        drop(w);
        std::fs::remove_file(&src).unwrap();
        std::fs::remove_file(&out).unwrap();
    }

    fn sample(pts: u64, key: bool) -> Sample {
        Sample { offset: 0, size: 0, dts: 0, pts, dur: 1, key }
    }

    #[test]
    fn target_and_keyframe_selection() {
        // Decode order I P B B I with pts 0 3 1 2 4: the B-frames show before the P they follow.
        let s = [sample(0, true), sample(3, false), sample(1, false), sample(2, false), sample(4, true)];
        let got: Vec<_> = (0..6).map(|p| target_sample(&s, p)).collect();
        assert_eq!(got, [0, 2, 3, 1, 4, 4]);
        assert_eq!((key_before(&s, 3), key_before(&s, 4)), (0, 4));
        // Before the first picture: the earliest one.
        let late = [sample(9, true), sample(5, false)];
        assert_eq!(target_sample(&late, 2), 1);
        // No keyframe flag at all: start at the beginning.
        assert_eq!(key_before(&[sample(0, false), sample(1, false)], 1), 0);
    }

    #[test]
    fn catch_up_decision() {
        // Target behind the decoder's output: restart, whatever the mode.
        assert!(must_seek(Mode::Play, 0, 10, 5, Some(9), 0.0));
        assert!(must_seek(Mode::Exact, 0, 10, 9, Some(9), 0.0));
        // Same GOP ahead: keep decoding.
        assert!(!must_seek(Mode::Exact, 0, 5, 9, Some(4), 0.1));
        assert!(!must_seek(Mode::Play, 0, 5, 30, Some(4), 2.0));
        // Target past a later keyframe: Exact and Keyframes jump at once...
        assert!(must_seek(Mode::Exact, 36, 5, 40, Some(4), 0.2));
        assert!(must_seek(Mode::Keyframes, 36, 5, 40, Some(4), 0.2));
        // ...Play keeps every frame unless it is more than CATCH_UP behind.
        assert!(!must_seek(Mode::Play, 36, 30, 40, Some(29), 0.3));
        assert!(must_seek(Mode::Play, 36, 5, 60, Some(4), 1.5));
        // The keyframe is next anyway.
        assert!(!must_seek(Mode::Play, 36, 36, 40, Some(35), 1.5));
    }

    /// Sends one target and waits for the worker's answer as (segment, sample).
    fn wait(w: &CamWorker, seg: usize, local: f64, mode: Mode) -> Result<(usize, usize), String> {
        while w.take().is_some() {}
        w.seek(seg, local, mode);
        let t0 = Instant::now();
        loop {
            if let Some(r) = w.take() {
                return r.map(|f| (f.seg, f.sample));
            }
            assert!(t0.elapsed() < Duration::from_secs(30), "worker timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn worker_follows_targets() {
        use crate::fixture::{FPS, encode_clip};
        use crate::sei;
        let path = std::env::temp_dir().join(format!("otc-worker-{}.mp4", std::process::id()));
        let (w, h) = (128, 96);
        // Moving gradient, with a Tesla-style SEI NAL in every sample like the front camera.
        encode_clip(&path, w, h, 200_000, 80, |t, yuv| {
            yuv.extend((0..w * h).map(|i| (i % w + t * 3) as u8));
            yuv.extend(std::iter::repeat_n(128, w * h / 2));
            Some(sei::sei_nal(&sei::encode_metadata(&sei::Telemetry { frame_seq_no: t as u64 + 1, ..Default::default() })))
        })
        .unwrap();
        let track = mp4::read_track(&path).unwrap();
        let keys: Vec<usize> = (0..80).filter(|&i| track.samples[i].key).collect();
        assert_eq!(keys, [0, 36, 72]);
        let at = |k: f64| k / FPS as f64;

        let worker = CamWorker::spawn("test", Context::default());
        worker.open(vec![Some(path.clone()), None, Some(path.clone())]);
        let w = &worker;
        assert_eq!(wait(w, 0, 0.0, Mode::Exact), Ok((0, 0)));
        assert_eq!(wait(w, 0, at(50.5), Mode::Exact), Ok((0, 50)), "forward, through keyframe 36");
        assert_eq!(wait(w, 0, at(51.0), Mode::Exact), Ok((0, 51)), "next frame");
        assert_eq!(wait(w, 0, at(10.0), Mode::Exact), Ok((0, 10)), "backward, exactly on a pts");
        assert_eq!(wait(w, 0, at(70.9), Mode::Keyframes), Ok((0, 36)));
        assert_eq!(wait(w, 0, at(75.0), Mode::Keyframes), Ok((0, 72)));
        assert_eq!(wait(w, 0, at(73.0), Mode::Exact), Ok((0, 73)), "exact after scrubbing");
        assert_eq!(wait(w, 0, 99.0, Mode::Exact), Ok((0, 79)), "past the end: last frame");
        assert!(wait(w, 1, 0.0, Mode::Exact).unwrap_err().contains("no clip"));
        assert_eq!(wait(w, 2, at(3.0), Mode::Exact), Ok((2, 3)), "preloaded next segment");
        // The picture is the decoded frame at full size.
        w.seek(2, at(4.0), Mode::Exact);
        let t0 = Instant::now();
        let f = loop {
            if let Some(f) = w.take() {
                break f.unwrap();
            }
            assert!(t0.elapsed() < Duration::from_secs(30));
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!((f.sample, f.image.size), (4, [128, 96]));
        drop(worker);
        std::fs::remove_file(&path).unwrap();
    }
}
