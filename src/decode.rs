//! openh264 wrapper (AVCC samples from `mp4::Track` in, I420 frames out) and the per-camera
//! decoder thread that follows the player's clock.

use crate::mp4::{self, Sample, Track};
use eframe::egui::{Color32, ColorImage, Context};
use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use std::cell::Cell;
use std::fs::File;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const START: [u8; 4] = [0, 0, 0, 1];

/// SPS and PPS in Annex-B form.
pub(crate) fn annex_b_params(track: &Track) -> Vec<u8> {
    let mut params = Vec::new();
    for p in track.sps.iter().chain(&track.pps) {
        params.extend(START);
        params.extend(p);
    }
    params
}

/// Rewrites an AVCC sample as Annex-B into `buf`, after `prefix`.
pub(crate) fn annex_b(buf: &mut Vec<u8>, prefix: &[u8], nal_len: usize, sample: &[u8]) -> Result<(), String> {
    buf.clear();
    buf.extend(prefix);
    let mut pos = 0;
    while pos + nal_len <= sample.len() {
        let len = sample[pos..pos + nal_len].iter().fold(0usize, |a, &b| a << 8 | b as usize);
        pos += nal_len;
        let nal = sample.get(pos..pos + len).ok_or("NAL length runs past the sample")?;
        pos += len;
        buf.extend(START);
        buf.extend(nal);
    }
    Ok(())
}

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
        let params = annex_b_params(track);
        // The default auto-flush after a packet with no picture corrupts reordering (B-frame)
        // streams; pictures still buffered come out of flush() at end of stream instead.
        let cfg = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        let dec = Decoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| format!("openh264: {e}"))?;
        Ok(CamDecoder { dec, params, nal_len: track.nal_len as usize, buf: Vec::new() })
    }

    /// Decodes one sample; `None` means the decoder has no picture ready yet.
    pub fn decode(&mut self, sample: &[u8], key: bool) -> Result<Option<DecodedYUV<'_>>, String> {
        annex_b(&mut self.buf, if key { &self.params } else { &[] }, self.nal_len, sample)?;
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

/// A decoded picture on its way to the screen.
pub enum Pic<'a> {
    Yuv(DecodedYUV<'a>),
    #[cfg(windows)]
    Mf(&'a crate::mf::Frame),
}

impl Pic<'_> {
    /// The picture as an egui image. The conversion's output is opaque, which is premultiplied
    /// as is: going through `from_rgba_unmultiplied` costs an HW4 front frame about 6 ms. A GPU
    /// picture comes back to memory here, so frames nobody shows don't pay for that.
    pub fn image(&self) -> Result<ColorImage, String> {
        let [w, h] = match self {
            Pic::Yuv(y) => [y.dimensions().0, y.dimensions().1],
            #[cfg(windows)]
            Pic::Mf(f) => f.size(),
        };
        // Zeroed u32s, unlike Color32s, cost nothing until the conversion touches them.
        let mut px = vec![0u32; w * h];
        match self {
            Pic::Yuv(y) => y.write_rgba8(bytemuck::cast_slice_mut(&mut px)),
            #[cfg(windows)]
            Pic::Mf(f) => f.write_rgba(&mut px).map_err(|e| format!("media foundation: {e}"))?,
        }
        Ok(ColorImage::new([w, h], bytemuck::cast_vec::<u32, Color32>(px)))
    }

    /// The picture's presentation time, when the decoder reports one.
    fn pts(&self) -> Option<u64> {
        match self {
            Pic::Yuv(_) => None,
            #[cfg(windows)]
            Pic::Mf(f) => Some(f.pts),
        }
    }
}

/// What decodes a clip for the viewer, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// Media Foundation on the GPU.
    Gpu,
    /// Media Foundation on the CPU.
    Software,
    OpenH264,
}

impl Engine {
    /// Where a clip starts. OPENTESLACAM_DECODER=software or openh264 is there in case a machine's
    /// GPU or decoder misbehaves.
    pub fn best() -> Engine {
        match std::env::var("OPENTESLACAM_DECODER").as_deref() {
            Ok("software") => Engine::Software,
            Ok("openh264") => Engine::OpenH264,
            _ => Engine::Gpu,
        }
    }

    fn next(self) -> Engine {
        match self {
            Engine::Gpu => Engine::Software,
            _ => Engine::OpenH264,
        }
    }
}

/// The viewer's decoder. Export stays on `CamDecoder`, which needs no realtime speed.
pub enum ViewDec {
    Soft(CamDecoder),
    #[cfg(windows)]
    Mf(crate::mf::MfDecoder),
}

impl ViewDec {
    /// `engine`, or the next one down if it can't be set up here.
    pub fn new(track: &Track, engine: Engine) -> Result<ViewDec, String> {
        // Media Foundation runs in low-latency mode, which only suits clips without reordering.
        // Tesla's have none; a clip with B-frames goes to openh264.
        #[cfg(windows)]
        if track.samples.windows(2).all(|p| p[0].pts < p[1].pts) {
            let tries: &[bool] = match engine {
                Engine::Gpu => &[true, false],
                Engine::Software => &[false],
                Engine::OpenH264 => &[],
            };
            if let Some(d) = tries.iter().find_map(|&gpu| crate::mf::MfDecoder::new(track, gpu).ok()) {
                return Ok(ViewDec::Mf(d));
            }
        }
        let _ = engine;
        CamDecoder::new(track).map(ViewDec::Soft)
    }

    pub fn name(&self) -> &'static str {
        match self {
            ViewDec::Soft(_) => "openh264",
            #[cfg(windows)]
            ViewDec::Mf(d) if d.on_gpu => "media foundation, gpu",
            #[cfg(windows)]
            ViewDec::Mf(_) => "media foundation",
        }
    }

    pub fn decode(&mut self, sample: &[u8], s: &Sample) -> Result<Option<Pic<'_>>, String> {
        match self {
            ViewDec::Soft(d) => Ok(d.decode(sample, s.key)?.map(Pic::Yuv)),
            #[cfg(windows)]
            ViewDec::Mf(d) => Ok(d.decode(sample, s.key, s.pts, s.dur)?.map(Pic::Mf)),
        }
    }

    pub fn flush(&mut self) -> Result<Vec<Pic<'_>>, String> {
        match self {
            ViewDec::Soft(d) => Ok(d.flush()?.into_iter().map(Pic::Yuv).collect()),
            #[cfg(windows)]
            ViewDec::Mf(d) => Ok(d.flush()?.iter().map(Pic::Mf).collect()),
        }
    }

    /// Whether the next engine should take this clip over: Media Foundation refused a sample, or
    /// swallowed the clip's start without a picture.
    fn failed(&self, was_err: bool) -> bool {
        match self {
            ViewDec::Soft(_) => false,
            #[cfg(windows)]
            ViewDec::Mf(d) => was_err || d.silent.is_some_and(|n| n > crate::mf::MAX_SILENT),
        }
    }
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

/// Latest-only hand-off to the UI, tagged with the `open` it belongs to. `Err` says why the
/// camera has no picture for the target (no clip, unreadable clip, damaged frame).
type Slot = Arc<Mutex<Option<(u64, Result<FrameOut, String>)>>>;

enum Cmd {
    Open(u64, Vec<Option<PathBuf>>),
    Seek(usize, f64, Mode),
    Shutdown,
}

/// One camera's decoder thread. Dropping it stops and joins the thread.
pub struct CamWorker {
    tx: Sender<Cmd>,
    slot: Slot,
    /// Counts `open` calls: the worker may still publish for the old event until it gets there.
    generation: Cell<u64>,
    thread: Option<JoinHandle<()>>,
}

impl CamWorker {
    /// `ctx` gets a repaint request whenever a new result is ready.
    pub fn spawn(name: &str, ctx: Context) -> CamWorker {
        let (tx, rx) = channel();
        let slot = Slot::default();
        let shared = slot.clone();
        // Built on its thread: a Media Foundation decoder isn't Send.
        let thread = thread::Builder::new().name(format!("decode-{name}")).spawn(move || Worker::new(shared, ctx).run(rx)).expect("spawn decoder thread");
        CamWorker { tx, slot, generation: Cell::new(0), thread: Some(thread) }
    }

    /// This camera's clip per segment (`None` where it didn't record). Forgets the old event.
    pub fn open(&self, files: Vec<Option<PathBuf>>) {
        self.generation.set(self.generation.get() + 1);
        let _ = self.tx.send(Cmd::Open(self.generation.get(), files));
    }

    /// Show the latest frame with pts <= `local` seconds into segment `seg`. Cheap: send it every
    /// UI frame; repeating the current target does nothing.
    pub fn seek(&self, seg: usize, local: f64, mode: Mode) {
        let _ = self.tx.send(Cmd::Seek(seg, local, mode));
    }

    /// The newest result for the current `open` since the last call, if any.
    pub fn take(&self) -> Option<Result<FrameOut, String>> {
        let (generation, r) = self.slot.lock().ok()?.take()?;
        (generation == self.generation.get()).then_some(r)
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
/// instead of decoding every frame: openh264 does an HW4 front stream at only ~1.3-1.7x realtime.
const CATCH_UP: f64 = 0.5;
/// While lagging in Play mode, convert and show a frame at most this often. Below any real decode
/// time, so a camera just behind still shows every frame; a catch-up burst doesn't convert frames
/// no screen would show.
const LAG_REFRESH: Duration = Duration::from_millis(10);

/// `seconds` in track ticks. The margin keeps a time computed as pts / timescale, often with a
/// segment start added and taken off again, from rounding below that pts. A thousandth of a tick
/// is far below any frame spacing and covers event times of many days.
pub(crate) fn pts_at(seconds: f64, timescale: u32) -> u64 {
    (seconds.max(0.0) * timescale.max(1) as f64 + 1e-3) as u64
}

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

pub(crate) struct Loaded {
    pub(crate) track: Track,
    pub(crate) file: File,
}

pub(crate) fn load(files: &[Option<PathBuf>], seg: usize) -> Result<Loaded, String> {
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
    /// The decoder skipped a sample without saying so.
    dropped: bool,
}

/// `Publisher::shown` sample once the tile was told its picture is lost.
const LOST: usize = usize::MAX;

struct Publisher {
    slot: Slot,
    generation: u64,
    ctx: Context,
    shown: Option<(usize, usize)>,
    /// A span of pts in a segment with no pictures, found out the hard way: re-decoding can't help.
    lost: Option<(usize, Range<u64>)>,
    at: Instant,
}

impl Publisher {
    fn put(&mut self, r: Result<FrameOut, String>) {
        self.shown = r.as_ref().ok().map(|f| (f.seg, f.sample));
        self.at = Instant::now();
        if let Ok(mut s) = self.slot.lock() {
            *s = Some((self.generation, r));
        }
        self.ctx.request_repaint();
    }

    fn frame(&mut self, seg: usize, sample: usize, pic: &Pic) {
        self.put(pic.image().map(|image| FrameOut { seg, sample, image }));
    }

    /// Whether `pts` is in the known lost span; if so the tile hears about it, once.
    fn in_lost(&mut self, seg: usize, pts: u64) -> bool {
        if !self.lost.as_ref().is_some_and(|(s, span)| *s == seg && span.contains(&pts)) {
            return false;
        }
        if self.shown != Some((seg, LOST)) {
            self.put(Err("damaged frame".to_owned()));
            self.shown = Some((seg, LOST));
        }
        true
    }
}

struct Target {
    seg: usize,
    sample: usize,
    pts: u64,
    mode: Mode,
}

/// Matches a decoded picture to its sample and shows it if wanted; true once the target is out or
/// known to be lost.
fn picture(pic: &Pic, feed: &mut Feed, out: &mut Publisher, t: &Target) -> bool {
    // A decoder that reports times tells us which earlier samples it dropped without a picture.
    if let Some(p) = pic.pts().filter(|p| feed.pending.iter().any(|e| e.0 == *p)) {
        feed.dropped |= feed.pending.iter().any(|e| e.0 < p);
        feed.pending.retain(|e| e.0 >= p);
    }
    let Some(i) = (0..feed.pending.len()).min_by_key(|&i| feed.pending[i].0) else { return false };
    let (pts, sample) = feed.pending.swap_remove(i);
    let prev = feed.last_out.replace(pts);
    // Pictures come out in pts order, so a later one means damage swallowed the target. It must
    // not be shown: the tile would run ahead of the other cameras.
    if pts > t.pts {
        out.lost = Some((t.seg, prev.map_or(t.pts, |p| p + 1)..pts));
        return true;
    }
    let reached = sample == t.sample;
    if reached || (t.mode == Mode::Play && out.at.elapsed() >= LAG_REFRESH) {
        out.frame(t.seg, sample, pic);
    }
    reached
}

struct Worker {
    files: Vec<Option<PathBuf>>,
    cur: Option<(usize, Result<Loaded, String>)>,
    /// The following segment, opened ahead (before going idle) so a boundary doesn't wait on disk.
    next: Option<(usize, Result<Loaded, String>)>,
    /// None: the next step restarts from a keyframe.
    dec: Option<ViewDec>,
    /// What decodes the current clip. It moves down when Media Foundation can't decode the clip, and
    /// to openh264 when it reports damage. It would carry on past a damaged frame showing pictures
    /// built on it; openh264 stops, and the tile says so until the next keyframe.
    // shortcut: damage Media Foundation conceals without a word shows as a glitch until the next keyframe, parse slices ourselves if that matters.
    engine: Engine,
    feed: Feed,
    out: Publisher,
    target: Option<(usize, f64, Mode)>,
    /// Nothing to do until the next command.
    idle: bool,
}

impl Worker {
    fn new(slot: Slot, ctx: Context) -> Worker {
        let out = Publisher { slot, generation: 0, ctx, shown: None, lost: None, at: Instant::now() };
        Worker { files: Vec::new(), cur: None, next: None, dec: None, engine: Engine::best(), feed: Feed::default(), out, target: None, idle: true }
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
                    Cmd::Open(generation, files) => {
                        self.files = files;
                        self.out.generation = generation;
                        (self.cur, self.next, self.dec, self.target, self.out.shown, self.out.lost) = (None, None, None, None, None, None);
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

    /// The clip can't be read or decoded: later targets in this segment settle without trying again.
    fn give_up(&mut self, seg: usize, msg: String) {
        self.out.put(Err(msg.clone()));
        self.cur = Some((seg, Err(msg)));
        self.dec = None;
        self.idle = true;
    }

    fn preload(&mut self, seg: usize) {
        if seg + 1 < self.files.len() && self.next.as_ref().is_none_or(|n| n.0 != seg + 1) {
            self.next = Some((seg + 1, load(&self.files, seg + 1)));
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
            (self.dec, self.engine) = (None, Engine::best());
        }
        let Some((_, Ok(l))) = &mut self.cur else {
            self.idle = true;
            return;
        };
        let samples = &l.track.samples;
        let mut sample = target_sample(samples, pts_at(local, l.track.timescale));
        if mode == Mode::Keyframes {
            sample = key_before(samples, sample);
        }
        if self.out.shown == Some((seg, sample)) || self.out.in_lost(seg, samples[sample].pts) {
            self.idle = true;
            self.preload(seg);
            return;
        }
        let t = Target { seg, sample, pts: samples[sample].pts, mode };
        let key = key_before(samples, sample);
        let fed = samples[self.feed.next.min(samples.len() - 1)].pts;
        let behind = t.pts.saturating_sub(fed) as f64 / l.track.timescale.max(1) as f64;
        if self.dec.is_none() || must_seek(mode, key, self.feed.next, t.pts, self.feed.last_out, behind) {
            match ViewDec::new(&l.track, self.engine) {
                Ok(d) => self.dec = Some(d),
                Err(e) => return self.give_up(seg, e),
            }
            self.feed = Feed { next: key, ..Feed::default() };
        }
        let Some(dec) = self.dec.as_mut() else { return };

        let mut done = false;
        if self.feed.next < samples.len() {
            let i = self.feed.next;
            self.feed.next += 1;
            let s = &samples[i];
            let data = match mp4::read_sample(&mut l.file, s) {
                Ok(d) => d,
                Err(e) => return self.give_up(seg, format!("read: {e}")),
            };
            self.feed.pending.push((s.pts, i));
            let res = dec.decode(&data, s);
            let was_err = res.is_err();
            match res {
                Ok(Some(pic)) => done = picture(&pic, &mut self.feed, &mut self.out, &t),
                Ok(None) => {}
                // A damaged frame yields no picture; forget it so later pictures still match.
                Err(_) => self.feed.pending.retain(|p| p.1 != i),
            }
            if dec.failed(was_err) || self.feed.dropped {
                // Next step: from the keyframe again, on another engine.
                self.engine = if self.feed.dropped { Engine::OpenH264 } else { self.engine.next() };
                self.dec = None;
                return;
            }
        }
        // Keyframes mode feeds nothing past the keyframe, and at the clip's end there is nothing
        // more to feed: either way the reorder buffer has to give up what it holds.
        let fed_all = self.feed.next >= samples.len() || (mode == Mode::Keyframes && self.feed.next > sample);
        if !done && fed_all {
            for pic in dec.flush().unwrap_or_default() {
                // Stop here: later pictures would overwrite the target in the slot.
                if picture(&pic, &mut self.feed, &mut self.out, &t) {
                    done = true;
                    break;
                }
            }
            if !done {
                // No picture at or after the target in this pass. At the clip's end that covers the
                // rest of the clip; a damaged keyframe only itself.
                let end = if self.feed.next >= samples.len() { u64::MAX } else { t.pts + 1 };
                self.out.lost = Some((seg, self.feed.last_out.map_or(t.pts, |p| p + 1)..end));
                done = true;
            }
            // Decoding on after a flush isn't relied on: the next step restarts at a keyframe.
            self.dec = None;
        }
        if done {
            self.out.in_lost(seg, t.pts);
            self.idle = true;
            self.preload(seg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4;
    use crate::mux::Muxer;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn a_frame_time_maps_back_to_its_pts_at_any_segment_start() {
        for start in [0.0, 2.0, 49.0, 85.0, 3600.0, 2_000_000.0] {
            for ts in [36_000u32, 18_432, 90_000] {
                for k in 0..2000u64 {
                    let pts = k * u64::from(ts) / 36;
                    let t = start + pts as f64 / f64::from(ts);
                    assert_eq!(pts_at(t - start, ts), pts, "start {start}, timescale {ts}");
                }
            }
        }
        assert_eq!(pts_at(-1.0, 36_000), 0);
        assert_eq!(pts_at(0.5, 0), 0, "a broken timescale doesn't divide anything");
    }

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

    /// A b-pyramid stream holds several pictures at its end: a cold seek to one of the last frames
    /// must show that frame, not whatever the flush returns last.
    #[test]
    fn pyramid_frames_at_the_clip_end_settle_on_the_target() {
        let path = std::env::temp_dir().join(format!("otc-pyr-{}.mp4", std::process::id()));
        let p = path.to_str().unwrap();
        let args = ["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30", "-frames:v", "60", "-c:v", "libx264", "-profile:v", "high"];
        if ffmpeg(&[&args[..], &["-bf", "3", "-x264-params", "b-pyramid=2", "-g", "30", p]].concat()).is_none() {
            println!("skipped: ffmpeg not on PATH");
            return;
        }
        let t = mp4::read_track(&path).unwrap();
        let d = t.samples[0].dur as u64;
        for k in [59u64, 58, 57, 56] {
            let w = CamWorker::spawn("pyr", Context::default());
            w.open(vec![Some(path.clone())]);
            let want = t.samples.iter().position(|s| s.pts == k * d).unwrap();
            assert_eq!(once(&w, 0, (k * d) as f64 / t.timescale as f64, Mode::Exact), Ok((0, want)), "frame {k}");
        }
        std::fs::remove_file(&path).unwrap();
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

    /// Windows' decoder gives the same pictures as openh264, at the times they were fed with. The
    /// size needs cropping, as Tesla's do (938 rows out of 944 coded).
    #[cfg(windows)]
    #[test]
    fn media_foundation_matches_openh264() {
        let path = gradient_clip_sized("mfsame", 40, 122, 90);
        let t = mp4::read_track(&path).unwrap();
        for engine in [Engine::Software, Engine::Gpu] {
            let mut f = File::open(&path).unwrap();
            let mut soft = ViewDec::new(&t, Engine::OpenH264).unwrap();
            let mut mf = ViewDec::new(&t, engine).unwrap();
            assert_eq!(soft.name(), "openh264");
            assert!(mf.name().starts_with("media foundation"), "{engine:?} on {}", mf.name());
            for s in &t.samples {
                let data = mp4::read_sample(&mut f, s).unwrap();
                let a = soft.decode(&data, s).unwrap().unwrap().image().unwrap();
                let pic = mf.decode(&data, s).unwrap().expect("low latency: a picture for every sample");
                assert_eq!(pic.pts(), Some(s.pts));
                let b = pic.image().unwrap();
                assert_eq!(a.size, b.size);
                let worst = a.pixels.iter().zip(&b.pixels).map(|(x, y)| x.r().abs_diff(y.r()).max(x.g().abs_diff(y.g())).max(x.b().abs_diff(y.b()))).max();
                assert!(worst.unwrap() <= 2, "{engine:?}, sample at {}: differs by {worst:?}", s.pts);
                assert!(b.pixels.iter().all(|p| p.a() == 255));
            }
        }
        std::fs::remove_file(&path).unwrap();
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

    /// Reissues the target for a while like the UI's repaints do, and returns everything published.
    fn settle(w: &CamWorker, seg: usize, local: f64, mode: Mode) -> Vec<Result<(usize, usize), String>> {
        let mut got = Vec::new();
        let t0 = Instant::now();
        let mut quiet = Instant::now();
        while quiet.elapsed() < Duration::from_millis(150) {
            assert!(t0.elapsed() < Duration::from_secs(30), "never settled: {} results", got.len());
            w.seek(seg, local, mode);
            if let Some(r) = w.take() {
                got.push(r.map(|f| (f.seg, f.sample)));
                quiet = Instant::now();
            }
            thread::sleep(Duration::from_millis(1));
        }
        got
    }

    /// The worker's one answer: however long it takes, and nothing may follow it.
    fn once(w: &CamWorker, seg: usize, local: f64, mode: Mode) -> Result<(usize, usize), String> {
        let r = wait(w, seg, local, mode);
        assert_eq!(settle(w, seg, local, mode), [], "more answers after {r:?}");
        r
    }

    fn damaged() -> Result<(usize, usize), String> {
        Err("damaged frame".to_owned())
    }

    fn gradient_clip(tag: &str, frames: usize) -> PathBuf {
        gradient_clip_sized(tag, frames, 128, 96)
    }

    fn gradient_clip_sized(tag: &str, frames: usize, w: usize, h: usize) -> PathBuf {
        use crate::fixture::encode_clip;
        use crate::sei;
        let path = std::env::temp_dir().join(format!("otc-{tag}-{}.mp4", std::process::id()));
        // Moving gradient, with a Tesla-style SEI NAL in every sample like the front camera.
        encode_clip(&path, w, h, 200_000, frames, |t, yuv| {
            yuv.extend((0..w * h).map(|i| (i % w + t * 3) as u8));
            yuv.extend(std::iter::repeat_n(128, w * h / 2));
            Some(sei::sei_nal(&sei::encode_metadata(&sei::Telemetry { frame_seq_no: t as u64 + 1, ..Default::default() })))
        })
        .unwrap();
        path
    }

    /// Overwrites these samples with noise in the file.
    fn damage(path: &Path, samples: &[&Sample]) {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = File::options().write(true).open(path).unwrap();
        let mut seed = 7;
        for s in samples {
            let noise: Vec<u8> = (0..s.size).map(|_| crate::fixture::rng(&mut seed) as u8).collect();
            f.seek(SeekFrom::Start(s.offset)).unwrap();
            f.write_all(&noise).unwrap();
        }
    }

    fn at(k: f64) -> f64 {
        k / crate::fixture::FPS as f64
    }

    #[test]
    fn a_damaged_frame_settles_and_never_shows_the_future() {
        let path = gradient_clip("dmg", 108);
        let t = mp4::read_track(&path).unwrap();
        damage(&path, &[&t.samples[50]]);
        let w = CamWorker::spawn("dmg", Context::default());
        w.open(vec![Some(path.clone())]);
        assert_eq!(once(&w, 0, at(49.0), Mode::Exact), Ok((0, 49)));
        assert_eq!(once(&w, 0, at(50.0), Mode::Exact), damaged());
        // The same hole: already told, and nothing is decoded again.
        assert_eq!(settle(&w, 0, at(55.0), Mode::Exact), []);
        assert_eq!(settle(&w, 0, at(71.0), Mode::Play), []);
        assert_eq!(once(&w, 0, at(72.0), Mode::Exact), Ok((0, 72)), "the next keyframe heals it");
        assert_eq!(once(&w, 0, at(60.0), Mode::Exact), damaged(), "back in the hole: told again");
        assert_eq!(once(&w, 0, at(49.0), Mode::Exact), Ok((0, 49)));
        drop(w);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_damaged_keyframe_settles_in_every_mode() {
        let path = gradient_clip("dmgkey", 108);
        let t = mp4::read_track(&path).unwrap();
        damage(&path, &[&t.samples[36]]);
        let w = CamWorker::spawn("dmgkey", Context::default());
        w.open(vec![Some(path.clone())]);
        assert_eq!(once(&w, 0, at(30.0), Mode::Exact), Ok((0, 30)));
        assert_eq!(once(&w, 0, at(40.0), Mode::Keyframes), damaged());
        assert_eq!(settle(&w, 0, at(40.0), Mode::Exact), [], "the tile already says so");
        assert_eq!(settle(&w, 0, at(36.0), Mode::Exact), []);
        assert_eq!(once(&w, 0, at(72.0), Mode::Exact), Ok((0, 72)));
        drop(w);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_undecodable_clip_reports_once_and_playing_through_it_is_quiet() {
        let path = gradient_clip("garbage", 80);
        let t = mp4::read_track(&path).unwrap();
        damage(&path, &t.samples.iter().collect::<Vec<_>>());
        let w = CamWorker::spawn("garbage", Context::default());
        w.open(vec![Some(path.clone())]);
        assert_eq!(once(&w, 0, at(10.0), Mode::Exact), damaged());
        assert_eq!(settle(&w, 0, at(20.0), Mode::Play), []);
        assert_eq!(settle(&w, 0, at(60.0), Mode::Play), []);
        drop(w);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_read_failure_reports_once() {
        let path = gradient_clip("short", 80);
        let w = CamWorker::spawn("short", Context::default());
        w.open(vec![Some(path.clone())]);
        assert_eq!(once(&w, 0, 0.0, Mode::Exact), Ok((0, 0)));
        // The file shrinks under the open clip, as with a pulled drive.
        File::options().write(true).open(&path).unwrap().set_len(64).unwrap();
        assert!(once(&w, 0, at(70.0), Mode::Exact).unwrap_err().starts_with("read:"));
        assert_eq!(settle(&w, 0, at(75.0), Mode::Exact), []);
        drop(w);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn take_never_returns_the_old_event_after_open() {
        let path = gradient_clip("gen", 40);
        let w = CamWorker::spawn("gen", Context::default());
        w.open(vec![Some(path.clone())]);
        w.seek(0, 0.0, Mode::Exact);
        let t0 = Instant::now();
        while w.slot.lock().unwrap().is_none() {
            assert!(t0.elapsed() < Duration::from_secs(30));
            thread::sleep(Duration::from_millis(1));
        }
        // The worker hasn't seen this open yet; its result for the old event is still in the slot.
        w.open(vec![Some(path.clone())]);
        assert!(w.take().is_none());
        assert_eq!(wait(&w, 0, 0.0, Mode::Exact), Ok((0, 0)));
        drop(w);
        std::fs::remove_file(&path).unwrap();
    }

    /// Entering a segment must not read the next one's moov before its first picture.
    #[test]
    fn preloading_waits_until_the_target_is_out() {
        let path = gradient_clip("pre", 12);
        let mut w = Worker::new(Slot::default(), Context::default());
        w.files = vec![Some(path.clone()), Some(path.clone())];
        w.target = Some((0, at(5.0), Mode::Exact));
        w.idle = false;
        w.step();
        assert!(w.cur.is_some() && w.next.is_none());
        while !w.idle {
            w.step();
        }
        assert_eq!(w.next.as_ref().map(|n| n.0), Some(1));
        // The last segment has nothing to open.
        w.target = Some((1, 0.0, Mode::Exact));
        w.idle = false;
        while !w.idle {
            w.step();
        }
        assert!(w.next.is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn worker_follows_targets() {
        let path = gradient_clip("worker", 80);
        let track = mp4::read_track(&path).unwrap();
        let keys: Vec<usize> = (0..80).filter(|&i| track.samples[i].key).collect();
        assert_eq!(keys, [0, 36, 72]);

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
