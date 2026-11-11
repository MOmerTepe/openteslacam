//! Phase 4: the trim range and the export worker. Lossless copies H.264 samples into new MP4s;
//! composited decodes the layout's cameras, draws them with the vehicle band into one frame and
//! encodes that. Everything is written as `<name>.part` and renamed once the whole export succeeded.

use crate::decode::{self, CamDecoder, Loaded, key_before, pts_at, target_sample};
use crate::fixture;
use crate::library::{CAMERAS, Event};
use crate::mp4::{self, Track};
use crate::mux::Muxer;
use crate::player::{Player, SegTime};
use crate::telemetry::{self, Store};
use crate::ui::export::{BAND_H, Band, CAPTION_H, band, caption};
use crate::ui::{grid, theme};
use eframe::egui::epaint::{Mesh, Primitive, Vertex};
use eframe::egui::{Color32, Context, Pos2, RawInput, Rect, TextureId, Ui, pos2, vec2};
use openh264::encoder::FrameType;
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

/// What `run` returns when `cancel` was set.
pub const CANCELLED: &str = "cancelled";
/// Filenames have 1 s resolution, so clips this close to touching (seconds) count as one recording.
const CONTIGUOUS: f64 = 1.0;
const BPS_720: u32 = 8_000_000;
const BPS_1080: u32 = 16_000_000;
const BPS_4K: u32 = 50_000_000;
/// (label, width, height, bits per second).
pub const RESOLUTIONS: [(&str, u32, u32, u32); 3] = [("720p", 1280, 720, BPS_720), ("1080p", 1920, 1080, BPS_1080), ("4k", 3840, 2160, BPS_4K)];
/// The composited frame is laid out in points on a canvas this tall, whatever its pixels.
const CANVAS_H: f32 = 720.0;
/// Timescale of composited output: fine enough to keep the source's frame spacing.
const TIMESCALE: u32 = 90_000;
/// Frames further apart than this (seconds) have a gap between them that the video skips.
const MAX_FRAME_GAP: f64 = 0.5;

/// In and Out on the event's timeline. An unset end means the event's end.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Trim {
    pub start: Option<f64>,
    pub end: Option<f64>,
}

fn same_time(a: Option<f64>, t: f64) -> bool {
    a.is_some_and(|a| (a - t).abs() < 1e-3)
}

impl Trim {
    /// Pressing it at the same time again clears it; an Out at or before the new In goes.
    pub fn set_in(&mut self, t: f64) {
        self.start = (!same_time(self.start, t)).then_some(t);
        if self.end.is_some_and(|e| e <= t) {
            self.end = None;
        }
    }

    /// Pressing it at the same time again clears it; an In at or after the new Out goes.
    pub fn set_out(&mut self, t: f64) {
        self.end = (!same_time(self.end, t)).then_some(t);
        if self.start.is_some_and(|s| s >= t) {
            self.start = None;
        }
    }

    pub fn is_set(&self) -> bool {
        self.start.is_some() || self.end.is_some()
    }

    /// (In, Out) inside the event of `dur` seconds, always ordered.
    pub fn range(&self, dur: f64) -> (f64, f64) {
        let dur = dur.max(0.0);
        let from = self.start.unwrap_or(0.0).clamp(0.0, dur);
        (from, self.end.unwrap_or(dur).clamp(from, dur))
    }
}

fn overlaps(s: &SegTime, from: f64, to: f64) -> bool {
    s.dur > 0.0 && s.start <= to && s.start + s.dur > from
}

/// Seconds of video between `from` and `to`, leaving out gaps.
pub fn footage(segs: &[SegTime], from: f64, to: f64) -> f64 {
    segs.iter().filter(|s| s.dur > 0.0).map(|s| ((s.start + s.dur).min(to) - s.start.max(from)).max(0.0)).sum()
}

/// Composited output settings.
pub struct Composite {
    pub width: u32,
    pub height: u32,
    pub bps: u32,
    pub hud: bool,
    pub map: bool,
    pub metric: bool,
    /// (camera, grid cell), bottom first.
    pub cells: Vec<(usize, grid::Cell)>,
    /// Encode on the GPU when it can; the CPU otherwise.
    pub gpu: bool,
}

/// The GPU's H.264 encoder, when this machine has one export can use.
pub fn gpu_encoder() -> Option<&'static str> {
    #[cfg(windows)]
    return crate::mf::hardware_encoder();
    #[cfg(not(windows))]
    None
}

/// An encoded picture: AVCC sample, and whether it's a keyframe.
type Picture = (Vec<u8>, bool);

/// What turns composited frames into H.264.
enum Encoder {
    Cpu(openh264::encoder::Encoder),
    /// With its NV12 buffer.
    #[cfg(windows)]
    Gpu(crate::mf::MfEncoder, Vec<u8>),
}

impl Encoder {
    /// The GPU's encoder if `gpu` and it takes these settings, else openh264 and, if the GPU was
    /// wanted, a note on why not.
    fn new(gpu: bool, w: usize, h: usize, bps: u32) -> Result<(Encoder, Option<String>), String> {
        let mut note = None;
        #[cfg(windows)]
        if gpu {
            match crate::mf::MfEncoder::new(w as u32, h as u32, bps, fixture::FPS) {
                Ok(e) => return Ok((Encoder::Gpu(e, Vec::new()), None)),
                Err(e) => note = Some(format!("GPU encoder not available ({}), encoded on the CPU", e.message())),
            }
        }
        let _ = gpu;
        let cpu = fixture::new_encoder(bps).map_err(|e| format!("encoder {w}x{h}: {e}"))?;
        Ok((Encoder::Cpu(cpu), note))
    }

    /// Sends one frame, at `time` for `dur` in 100 ns units. Returns the pictures that came out, in
    /// order; parameter sets go to `sps` and `pps`.
    fn encode(&mut self, yuv: &YUVBuffer, time: i64, dur: i64, sps: &mut Vec<u8>, pps: &mut Vec<u8>) -> Result<Vec<Picture>, String> {
        match self {
            Encoder::Cpu(enc) => {
                let (w, h) = yuv.dimensions();
                let bs = enc.encode(yuv).map_err(|e| format!("encoder {w}x{h}: {e}"))?;
                let mut sample = Vec::new();
                fixture::take_nals(&bs, &mut sample, sps, pps);
                if sample.is_empty() {
                    return Err(format!("encoder {w}x{h} produced no picture"));
                }
                Ok(vec![(sample, bs.frame_type() == FrameType::IDR)])
            }
            #[cfg(windows)]
            Encoder::Gpu(enc, nv12) => {
                // openh264's colour conversion, then its U and V planes interleaved.
                nv12.clear();
                nv12.extend_from_slice(yuv.y());
                nv12.extend(yuv.u().iter().zip(yuv.v()).flat_map(|(&u, &v)| [u, v]));
                let units = enc.encode(nv12, time, dur).map_err(|e| format!("GPU encoder: {}", e.message()))?;
                Ok(units.iter().map(|u| picture(u, sps, pps)).collect())
            }
        }
    }

    /// Pictures still in the encoder; call after the last frame.
    fn finish(&mut self, sps: &mut Vec<u8>, pps: &mut Vec<u8>) -> Result<Vec<Picture>, String> {
        match self {
            Encoder::Cpu(_) => Ok(Vec::new()),
            #[cfg(windows)]
            Encoder::Gpu(enc, _) => Ok(enc.finish().map_err(|e| format!("GPU encoder: {}", e.message()))?.iter().map(|u| picture(u, sps, pps)).collect()),
        }
    }
}

/// An Annex-B access unit as a sample; keyframes are the ones with an IDR slice. Delimiters go: MP4
/// doesn't need them, and openh264 refuses one after the SPS and PPS decoders put before keyframes.
#[cfg_attr(not(windows), allow(dead_code))]
fn picture(unit: &[u8], sps: &mut Vec<u8>, pps: &mut Vec<u8>) -> Picture {
    let mut sample = Vec::new();
    let mut key = false;
    for nal in fixture::annex_b_nals(unit).into_iter().filter(|n| n[0] & 0x1F != 9) {
        key |= nal[0] & 0x1F == 5;
        fixture::put_nal(nal, &mut sample, sps, pps);
    }
    (sample, key)
}

pub enum Mode {
    /// Indexed like `CAMERAS`.
    Lossless([bool; 6]),
    Composite(Composite),
}

pub struct Job {
    pub event: Event,
    /// `Player::segs` of `event`.
    pub segs: Vec<SegTime>,
    pub from: f64,
    pub to: f64,
    /// Path without extension: files are `<base>.mp4`, `<base>-front.mp4`, `<base>-telemetry.csv`.
    pub base: PathBuf,
    pub mode: Mode,
    pub csv: bool,
}

#[derive(Debug)]
pub struct Summary {
    pub files: Vec<PathBuf>,
    /// What's missing from them, in words.
    pub notes: Vec<String>,
}

fn named(base: &Path, suffix: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// `p`, or `p (2)`, … if that's taken: a second export of one range must not eat the first.
fn free_name(p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    (2..)
        .map(|n| {
            let mut name = p.file_stem().unwrap_or_default().to_owned();
            name.push(format!(" ({n})"));
            if let Some(ext) = p.extension() {
                name.push(".");
                name.push(ext);
            }
            p.with_file_name(name)
        })
        .find(|c| !c.exists())
        .expect("some number is free")
}

/// The `.part` files of this export. Dropping it deletes those not committed.
#[derive(Default)]
struct Parts(Vec<(PathBuf, PathBuf)>);

impl Parts {
    /// Where to write what will become `target`.
    fn add(&mut self, target: PathBuf) -> PathBuf {
        let part = target.with_added_extension("part");
        self.0.push((part.clone(), target));
        part
    }

    fn commit(&mut self) -> io::Result<Vec<PathBuf>> {
        let mut done = Vec::new();
        while let Some((part, target)) = self.0.first().cloned() {
            let target = free_name(target);
            if let Err(e) = fs::rename(&part, &target) {
                // All or nothing: `free_name` made these names ours, so nothing older goes with them.
                for f in &done {
                    let _ = fs::remove_file(f);
                }
                return Err(e);
            }
            self.0.remove(0);
            done.push(target);
        }
        Ok(done)
    }
}

impl Drop for Parts {
    fn drop(&mut self) {
        for (part, _) in &self.0 {
            let _ = fs::remove_file(part);
        }
    }
}

fn io_err(p: &Path) -> impl Fn(io::Error) -> String + '_ {
    move |e| format!("{}: {e}", p.display())
}

/// Runs on the worker thread. `progress` gets 0..=1. Nothing keeps its final name unless everything
/// succeeded: on an error or cancel all `.part` files are gone again.
pub fn run(job: &Job, cancel: &AtomicBool, mut progress: impl FnMut(f32)) -> Result<Summary, String> {
    let mut parts = Parts::default();
    let mut notes = Vec::new();
    let wants_store = job.csv || matches!(&job.mode, Mode::Composite(c) if c.hud || c.map);
    let store = if wants_store { load_store(job, cancel) } else { Store::default() };
    match &job.mode {
        Mode::Lossless(cams) => lossless(job, *cams, &mut parts, &mut notes, cancel, &mut progress)?,
        Mode::Composite(c) => composite(job, c, &store, &mut parts, &mut notes, cancel, &mut progress)?,
    }
    if job.csv && !store.records.iter().any(|r| (job.from..=job.to).contains(&r.t)) {
        notes.push("no vehicle data in this range".to_owned());
    } else if job.csv {
        let part = parts.add(named(&job.base, "-telemetry.csv"));
        let write = || -> io::Result<()> {
            let mut f = BufWriter::new(File::create(&part)?);
            store.write_csv(&mut f, job.from, job.to)?;
            f.flush()
        };
        write().map_err(io_err(&part))?;
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLED.to_owned());
    }
    let files = parts.commit().map_err(|e| format!("{}: {e}", job.base.display()))?;
    Ok(Summary { files, notes })
}

/// The vehicle data of the segments in range only: a long RecentClips would take minutes.
fn load_store(job: &Job, cancel: &AtomicBool) -> Store {
    let jobs = telemetry::jobs(&job.event, &job.segs).into_iter().zip(&job.segs).filter(|(_, s)| overlaps(s, job.from, job.to)).map(|(j, _)| j).collect();
    let mut last = Store::default();
    telemetry::load(jobs, |s| {
        last = s;
        !cancel.load(Ordering::Relaxed)
    });
    last
}

/// A stretch of one segment's samples, in decode order.
#[derive(Debug, Clone, PartialEq)]
struct Piece {
    seg: usize,
    samples: Range<usize>,
}

/// One camera's samples for `from..=to`, split into runs that each become one file. A new run starts
/// at a gap (time isn't invented), a missing clip, or where the stream's parameters change. Each run
/// starts at a keyframe: the one at or before In in the first run, the clip's first sample after that.
fn plan(segs: &[SegTime], tracks: &[Option<Track>], from: f64, to: f64) -> Vec<Vec<Piece>> {
    let mut runs: Vec<Vec<Piece>> = Vec::new();
    // Where the open run stopped: (time, its last segment).
    let mut open: Option<(f64, usize)> = None;
    for (i, (s, track)) in segs.iter().zip(tracks).enumerate() {
        if !overlaps(s, from, to) {
            continue;
        }
        let Some(t) = track.as_ref().filter(|t| !t.samples.is_empty()) else {
            open = None;
            continue;
        };
        let out = pts_at(to - s.start, t.timescale);
        let Some(last) = t.samples.iter().rposition(|x| x.pts <= out) else { continue };
        let joins = t.samples[0].key
            && open.is_some_and(|(end, k)| {
                let same = tracks[k].as_ref().is_some_and(|p| p.avcc == t.avcc && p.timescale == t.timescale && (p.width, p.height) == (t.width, t.height));
                same && (s.start - end).abs() <= CONTIGUOUS
            });
        let first = if joins {
            0
        } else {
            let k = key_before(&t.samples, target_sample(&t.samples, pts_at(from - s.start, t.timescale)));
            // A clip that doesn't start on a keyframe can't be decoded before its first one.
            match if t.samples[k].key { Some(k) } else { t.samples.iter().position(|x| x.key) } {
                Some(k) if k <= last => k,
                _ => {
                    open = None;
                    continue;
                }
            }
        };
        // With B-frames a kept reference can display after samples decoded later than `last`: without
        // them, presentation would have a hole before that reference.
        let shown_to = t.samples[first..=last].iter().map(|x| x.pts).max().expect("first <= last");
        let last = t.samples.iter().rposition(|x| x.pts <= shown_to).expect("at least the one above");
        let piece = Piece { seg: i, samples: first..last + 1 };
        match runs.last_mut().filter(|_| joins) {
            Some(run) => run.push(piece),
            None => runs.push(vec![piece]),
        }
        open = Some((s.start + t.duration as f64 / t.timescale as f64, i));
    }
    runs
}

fn lossless(job: &Job, cams: [bool; 6], parts: &mut Parts, notes: &mut Vec<String>, cancel: &AtomicBool, progress: &mut impl FnMut(f32)) -> Result<(), String> {
    let mut plans = Vec::new();
    // Clips in range that can't be read: notes, or the reason when nothing at all can be copied.
    let mut unreadable = Vec::new();
    for cam in (0..6).filter(|&c| cams[c]) {
        let before = unreadable.len();
        let mut missing = Vec::new();
        let read = |(i, (s, t)): (usize, (&crate::library::Segment, &SegTime))| {
            if !overlaps(t, job.from, job.to) {
                return None;
            }
            let Some(p) = &s.files[cam] else {
                missing.push((i + 1).to_string());
                return None;
            };
            match mp4::read_track(p) {
                Ok(t) if !t.samples.is_empty() => Some(t),
                Ok(_) => {
                    unreadable.push(notes_line(cam, p, &"no frames"));
                    None
                }
                Err(e) => {
                    unreadable.push(notes_line(cam, p, &e));
                    None
                }
            }
        };
        let tracks: Vec<Option<Track>> = job.event.segments.iter().zip(&job.segs).enumerate().map(read).collect();
        let runs = plan(&job.segs, &tracks, job.from, job.to);
        if runs.is_empty() {
            if unreadable.len() == before {
                notes.push(format!("{}: no footage in this range", CAMERAS[cam]));
            }
        } else if !missing.is_empty() {
            notes.push(format!("{}: no clip for segment {}", CAMERAS[cam], missing.join(", ")));
        }
        plans.push((cam, tracks, runs));
    }
    let total: usize = plans.iter().flat_map(|p| p.2.iter().flatten()).map(|x| x.samples.len()).sum();
    if total == 0 {
        unreadable.insert(0, "no footage in this range".to_owned());
        return Err(unreadable.join("\n"));
    }
    notes.extend(unreadable);
    let mut done = 0;
    for (cam, tracks, runs) in &plans {
        for (k, run) in runs.iter().enumerate() {
            let suffix = if k == 0 { format!("-{}.mp4", CAMERAS[*cam]) } else { format!("-{}-{}.mp4", CAMERAS[*cam], k + 1) };
            let part = parts.add(named(&job.base, &suffix));
            let t0 = tracks[run[0].seg].as_ref().expect("planned from it");
            let err = io_err(&part);
            let mut m = Muxer::create(&part, t0.timescale, t0.width, t0.height, &t0.avcc).map_err(&err)?;
            for piece in run {
                let t = tracks[piece.seg].as_ref().expect("planned from it");
                let src = job.event.segments[piece.seg].files[*cam].as_ref().expect("planned from it");
                let mut f = File::open(src).map_err(io_err(src))?;
                for s in &t.samples[piece.samples.clone()] {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(CANCELLED.to_owned());
                    }
                    let data = mp4::read_sample(&mut f, s).map_err(io_err(src))?;
                    m.push(&data, s.dur, (s.pts as i64 - s.dts as i64) as i32, s.key).map_err(&err)?;
                    done += 1;
                    progress(done as f32 / total as f32);
                }
            }
            m.finish().map_err(&err)?;
        }
    }
    Ok(())
}

fn notes_line(cam: usize, p: &Path, e: &dyn std::fmt::Display) -> String {
    format!("{}: {}: {e}", CAMERAS[cam], p.display())
}

/// Reference-camera frame times (seconds from the event start) from the frame showing `from` through
/// `to`. Where segments overlap the later one owns the time, as on the player's clock.
fn frame_times(clock: &Player, from: f64, to: f64) -> Vec<f64> {
    let mut all = Vec::new();
    for (i, s) in clock.segs.iter().enumerate() {
        all.extend(s.frames.iter().map(|f| s.start + f).filter(|&t| clock.locate(t).is_some_and(|l| l.0 == i)));
    }
    let first = all.partition_point(|&t| t <= from).saturating_sub(1);
    let end = all.partition_point(|&t| t <= to);
    all.get(first..end.max(first + 1).min(all.len())).unwrap_or_default().to_vec()
}

/// Each frame lasts until the next one, in `TIMESCALE`; across a gap the video just cuts.
fn durations(times: &[f64]) -> Vec<u32> {
    let nominal = TIMESCALE / fixture::FPS;
    let mut out: Vec<u32> = times.windows(2).map(|w| if w[1] - w[0] > MAX_FRAME_GAP { nominal } else { ((w[1] - w[0]) * TIMESCALE as f64).round() as u32 }).collect();
    out.push(out.last().copied().unwrap_or(nominal));
    out
}

/// Tightly packed RGBA pixels.
#[derive(Default)]
struct Bitmap {
    w: usize,
    h: usize,
    px: Vec<[u8; 4]>,
}

/// Clipped to the canvas on the right and bottom.
fn blit(canvas: &mut [[u8; 4]], canvas_w: usize, b: &Bitmap, x: usize, y: usize) {
    let w = b.w.min(canvas_w.saturating_sub(x));
    for (r, row) in b.px.chunks_exact(b.w.max(1)).enumerate() {
        let at = (y + r) * canvas_w + x;
        let Some(dst) = canvas.get_mut(at..at + w) else { break };
        dst.copy_from_slice(&row[..w]);
    }
}

/// Box filter when shrinking, nearest when growing.
fn scale(src: &[u8], (sw, sh): (usize, usize), out: &mut Bitmap, (dw, dh): (usize, usize)) {
    out.px.clear();
    (out.w, out.h) = (dw, dh);
    for y in 0..dh {
        let (y0, y1) = (y * sh / dh, ((y + 1) * sh / dh).max(y * sh / dh + 1));
        for x in 0..dw {
            let (x0, x1) = (x * sw / dw, ((x + 1) * sw / dw).max(x * sw / dw + 1));
            let mut sum = [0u32; 3];
            for row in y0..y1 {
                for px in src[(row * sw + x0) * 4..(row * sw + x1) * 4].as_chunks::<4>().0 {
                    sum[0] += px[0] as u32;
                    sum[1] += px[1] as u32;
                    sum[2] += px[2] as u32;
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            out.px.push([(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, 255]);
        }
    }
}

/// The app's own widgets, painted on the worker thread: a private egui with a CPU rasterizer behind
/// it, so the exported vehicle strip is the one on screen and no new dependency is needed.
struct Chrome {
    ctx: Context,
    zoom: f32,
    atlas: Vec<Color32>,
    atlas_size: [usize; 2],
}

impl Chrome {
    fn new(zoom: f32) -> Chrome {
        let ctx = Context::default();
        theme::install(&ctx);
        ctx.set_pixels_per_point(zoom);
        let mut c = Chrome { ctx, zoom, atlas: Vec::new(), atlas_size: [0, 0] };
        // Fonts and the zoom take effect on the pass after they're set.
        c.paint(1, 1, |_| {});
        c
    }

    fn paint(&mut self, w: usize, h: usize, mut add: impl FnMut(&Ui)) -> Bitmap {
        let size = vec2(w as f32, h as f32) / self.zoom;
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)), ..Default::default() };
        let mut out = self.ctx.run_ui(input, |ui| add(ui));
        for delta in out.textures_delta.set.drain().filter(|d| d.0 == TextureId::default()).flat_map(|d| d.1) {
            self.patch_atlas(delta);
        }
        out.textures_delta.clear();
        let mut canvas = Bitmap { w, h, px: vec![theme::BG.to_array(); w * h] };
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        // Horizontal stripes, one per core: each draws every mesh in order, clipped to its rows, so
        // the picture is the same as drawn on one thread. At 4K this was a third of export time.
        let threads = thread::available_parallelism().map_or(1, |n| n.get());
        let rows = h.div_ceil(threads).max(16);
        let this = &*self;
        thread::scope(|s| {
            for (k, stripe) in canvas.px.chunks_mut(rows * w.max(1)).enumerate() {
                let prims = &prims;
                s.spawn(move || {
                    for prim in prims {
                        if let Primitive::Mesh(mesh) = &prim.primitive {
                            this.fill(stripe, w, k * rows, mesh, prim.clip_rect, out.pixels_per_point);
                        }
                    }
                });
            }
        });
        canvas
    }

    fn patch_atlas(&mut self, delta: eframe::egui::epaint::ImageDelta) {
        let eframe::egui::ImageData::Color(image) = &delta.image;
        let [x0, y0] = delta.pos.unwrap_or([0, 0]);
        if delta.pos.is_none() {
            self.atlas_size = image.size;
            self.atlas = vec![Color32::TRANSPARENT; image.size[0] * image.size[1]];
        }
        for (r, row) in image.pixels.chunks_exact(image.size[0].max(1)).enumerate() {
            let at = (y0 + r) * self.atlas_size[0] + x0;
            self.atlas[at..at + row.len()].copy_from_slice(row);
        }
    }

    /// Draws `mesh` into `px`: rows `top..` of a `w` wide canvas, nothing outside them.
    fn fill(&self, px: &mut [[u8; 4]], w: usize, top: usize, mesh: &Mesh, clip: Rect, ppp: f32) {
        let [tw, th] = self.atlas_size;
        let bottom = top + px.len() / w.max(1);
        let (cx0, cy0) = ((clip.min.x * ppp).floor().max(0.0) as usize, ((clip.min.y * ppp).floor().max(0.0) as usize).max(top));
        let (cx1, cy1) = (((clip.max.x * ppp).ceil().max(0.0) as usize).min(w), ((clip.max.y * ppp).ceil().max(0.0) as usize).min(bottom));
        let at = |v: &Vertex| pos2(v.pos.x * ppp, v.pos.y * ppp);
        let edge = |a: Pos2, b: Pos2, c: Pos2| (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
        for tri in mesh.indices.as_chunks::<3>().0 {
            let [va, vb, vc] = tri.map(|i| &mesh.vertices[i as usize]);
            let (a, b, c) = (at(va), at(vb), at(vc));
            let area = edge(a, b, c);
            if area.abs() < 1e-6 {
                continue;
            }
            let (x0, x1) = (a.x.min(b.x).min(c.x).floor().max(0.0) as usize, a.x.max(b.x).max(c.x).ceil().max(0.0) as usize);
            let (y0, y1) = (a.y.min(b.y).min(c.y).floor().max(0.0) as usize, a.y.max(b.y).max(c.y).ceil().max(0.0) as usize);
            for y in y0.max(cy0)..y1.min(cy1) {
                for x in x0.max(cx0)..x1.min(cx1) {
                    let p = pos2(x as f32 + 0.5, y as f32 + 0.5);
                    let (wa, wb) = (edge(b, c, p) / area, edge(c, a, p) / area);
                    let wc = 1.0 - wa - wb;
                    if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                        continue;
                    }
                    let (u, v) = (wa * va.uv.x + wb * vb.uv.x + wc * vc.uv.x, wa * va.uv.y + wb * vb.uv.y + wc * vc.uv.y);
                    let tex = self.atlas.get(((v * th as f32) as usize).min(th.saturating_sub(1)) * tw + ((u * tw as f32) as usize).min(tw.saturating_sub(1))).copied().unwrap_or(Color32::WHITE);
                    let channel = |i: usize| (wa * va.color[i] as f32 + wb * vb.color[i] as f32 + wc * vc.color[i] as f32) * tex[i] as f32 / 255.0;
                    let src = [channel(0), channel(1), channel(2), channel(3)];
                    let dst = &mut px[(y - top) * w + x];
                    for i in 0..4 {
                        dst[i] = (src[i] + dst[i] as f32 * (1.0 - src[3] / 255.0)).round().min(255.0) as u8;
                    }
                }
            }
        }
    }
}

/// One tile of the composited frame.
#[derive(Debug, PartialEq)]
struct Place {
    cam: usize,
    caption: Rect,
    video: Rect,
}

/// In pixels; `band_h` is taken off the bottom, so tiles can't reach it (design rule 1).
fn places(w: usize, h: usize, band_h: f32, zoom: f32, cells: &[(usize, grid::Cell)]) -> Vec<Place> {
    let area = Rect::from_min_size(Pos2::ZERO, vec2(w as f32, h as f32 - band_h));
    cells
        .iter()
        .map(|&(cam, cell)| {
            let (caption, video) = {
                let r = grid::cell_rect(area, cell);
                r.split_top_bottom_at_y((r.min.y + (CAPTION_H * zoom).round()).min(r.max.y))
            };
            Place { cam, caption, video }
        })
        .collect()
}

/// One camera following increasing times: decodes forward, restarts at each new segment.
struct Stream {
    cam: usize,
    files: Vec<Option<PathBuf>>,
    cur: Option<(usize, Result<Loaded, String>)>,
    dec: Option<CamDecoder>,
    next: usize,
    /// pts of samples sent without a picture out yet.
    pending: Vec<u64>,
    /// pts of the picture in `rgba`.
    shown: Option<u64>,
    rgba: Vec<u8>,
    size: (usize, usize),
    /// `rgba` scaled to fit the tile.
    fitted: Bitmap,
    /// The first thing that went wrong: the tile stays empty or holds its last picture.
    note: Option<String>,
}

impl Stream {
    fn new(cam: usize, event: &Event) -> Stream {
        let files = event.segments.iter().map(|s| s.files[cam].clone()).collect();
        Stream { cam, files, cur: None, dec: None, next: 0, pending: Vec::new(), shown: None, rgba: Vec::new(), size: (0, 0), fitted: Bitmap::default(), note: None }
    }

    /// Brings `rgba` to the latest picture with pts <= `local` seconds into `seg`.
    fn advance(&mut self, seg: usize, local: f64) {
        if self.cur.as_ref().is_none_or(|c| c.0 != seg) {
            let loaded = decode::load(&self.files, seg);
            if let Err(e) = &loaded {
                self.note.get_or_insert_with(|| e.clone());
            }
            self.cur = Some((seg, loaded));
            (self.dec, self.shown, self.size) = (None, None, (0, 0));
            self.pending.clear();
        }
        let Some((_, Ok(l))) = &mut self.cur else { return };
        let samples = &l.track.samples;
        let target = target_sample(samples, pts_at(local, l.track.timescale));
        if self.dec.is_none() {
            match CamDecoder::new(&l.track) {
                Ok(d) => self.dec = Some(d),
                Err(e) => {
                    self.note.get_or_insert(e);
                    return;
                }
            }
            self.next = key_before(samples, target);
        }
        let dec = self.dec.as_mut().expect("just made");
        while self.shown.is_none_or(|p| p < samples[target].pts) {
            let Some(s) = samples.get(self.next) else {
                if self.next == samples.len() {
                    // Reordering pictures still held at the end of the clip.
                    for yuv in dec.flush().unwrap_or_default() {
                        let (w, h) = decode::to_rgba(&yuv, &mut self.rgba);
                        self.size = (w as usize, h as usize);
                        self.shown = pop_min(&mut self.pending).or(self.shown);
                    }
                    self.next += 1;
                }
                break;
            };
            self.next += 1;
            let data = match mp4::read_sample(&mut l.file, s) {
                Ok(d) => d,
                Err(e) => {
                    self.note.get_or_insert_with(|| e.to_string());
                    continue;
                }
            };
            self.pending.push(s.pts);
            match dec.decode(&data, s.key) {
                Ok(Some(yuv)) => {
                    let (w, h) = decode::to_rgba(&yuv, &mut self.rgba);
                    self.size = (w as usize, h as usize);
                    self.shown = pop_min(&mut self.pending).or(self.shown);
                }
                Ok(None) => {}
                Err(e) => {
                    self.note.get_or_insert(e);
                }
            }
        }
    }

    /// Scales the picture to fit `video`; empty when there is none.
    fn fit_to(&mut self, video: Rect) {
        let (sw, sh) = self.size;
        let r = grid::fit(video, vec2(sw as f32, sh as f32));
        let (dw, dh) = (r.width().round() as usize, r.height().round() as usize);
        if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
            self.fitted = Bitmap::default();
            return;
        }
        scale(&self.rgba, (sw, sh), &mut self.fitted, (dw, dh));
    }
}

fn pop_min(v: &mut Vec<u64>) -> Option<u64> {
    let i = (0..v.len()).min_by_key(|&i| v[i])?;
    Some(v.swap_remove(i))
}

fn composite(job: &Job, c: &Composite, store: &Store, parts: &mut Parts, notes: &mut Vec<String>, cancel: &AtomicBool, progress: &mut impl FnMut(f32)) -> Result<(), String> {
    let clock = Player::new(job.segs.clone(), 0);
    let times = frame_times(&clock, job.from, job.to);
    // `frame_times` always offers the frame before In, even for a range with no video in it.
    if times.is_empty() || !job.segs.iter().any(|s| overlaps(s, job.from, job.to)) {
        return Err("no footage in this range".to_owned());
    }
    let durs = durations(&times);
    // H.264 wants even dimensions.
    let (w, h) = ((c.width & !1).max(2) as usize, (c.height & !1).max(2) as usize);
    let zoom = h as f32 / CANVAS_H;
    let band_h = if c.hud || c.map { (BAND_H * zoom).round() } else { 0.0 };
    let places = places(w, h, band_h, zoom, &c.cells);

    let mut chrome = Chrome::new(zoom);
    let captions: Vec<Bitmap> = places
        .iter()
        .map(|t| {
            let (cw, ch) = (t.caption.width() as usize, t.caption.height() as usize);
            chrome.paint(cw, ch, |ui| caption(ui, Rect::from_min_size(Pos2::ZERO, vec2(cw as f32, ch as f32) / zoom), t.cam))
        })
        .collect();
    let letterbox: Vec<Bitmap> = places
        .iter()
        .map(|t| {
            let (vw, vh) = (t.video.width() as usize, t.video.height() as usize);
            Bitmap { w: vw, h: vh, px: vec![theme::VIDEO_BG.to_array(); vw * vh] }
        })
        .collect();
    let mut streams: Vec<Stream> = places.iter().map(|t| Stream::new(t.cam, &job.event)).collect();

    let (mut enc, note) = Encoder::new(c.gpu, w, h, c.bps)?;
    notes.extend(note);
    let mut yuv = YUVBuffer::new(w, h);
    let mut canvas = vec![theme::BG.to_array(); w * h];
    let part = parts.add(named(&job.base, ".mp4"));
    let err = io_err(&part);
    let mut mux: Option<Muxer> = None;
    let (mut sps, mut pps) = (Vec::new(), Vec::new());
    // Durations of frames sent and not muxed yet: a GPU encoder gives pictures back later.
    let mut waiting = std::collections::VecDeque::new();
    let mut put = |pics: Vec<Picture>, sps: &[u8], pps: &[u8], waiting: &mut std::collections::VecDeque<u32>| -> Result<(), String> {
        for (sample, key) in pics {
            if mux.is_none() {
                if sps.len() < 4 || pps.is_empty() {
                    return Err(format!("encoder {w}x{h} produced no SPS/PPS"));
                }
                mux = Some(Muxer::create(&part, TIMESCALE, w as u32, h as u32, &fixture::avcc_of(sps, pps)).map_err(&err)?);
            }
            let dur = waiting.pop_front().ok_or_else(|| format!("encoder {w}x{h} returned more pictures than it was given"))?;
            mux.as_mut().expect("just made").push(&sample, dur, 0, key).map_err(&err)?;
        }
        Ok(())
    };
    // 100 ns units, as Media Foundation counts.
    let (mut ticks, hns) = (0u64, |t: u64| (t * 10_000_000 / TIMESCALE as u64) as i64);
    let wall0 = job.event.segments[0].start as f64;

    for (i, &t) in times.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_owned());
        }
        let Some((seg, local)) = clock.locate(t) else { continue };
        // One thread per camera per frame: decode dominates, and a spawn costs little next to it.
        thread::scope(|sc| {
            for (st, p) in streams.iter_mut().zip(&places) {
                sc.spawn(move || {
                    st.advance(seg, local);
                    st.fit_to(p.video);
                });
            }
        });
        canvas.fill(theme::BG.to_array());
        // Back to front, each tile whole: custom tiles can overlap.
        for (k, p) in places.iter().enumerate() {
            blit(&mut canvas, w, &captions[k], p.caption.min.x as usize, p.caption.min.y as usize);
            blit(&mut canvas, w, &letterbox[k], p.video.min.x as usize, p.video.min.y as usize);
            let f = &streams[k].fitted;
            let x = p.video.min.x as usize + (letterbox[k].w.saturating_sub(f.w)) / 2;
            let y = p.video.min.y as usize + (letterbox[k].h.saturating_sub(f.h)) / 2;
            blit(&mut canvas, w, f, x, y);
        }
        if band_h > 0.0 {
            let data = Band { store, t, wall: wall0 + t, hud: c.hud, map: c.map, metric: c.metric };
            let bmp = chrome.paint(w, band_h as usize, |ui| band(ui, Rect::from_min_size(Pos2::ZERO, vec2(w as f32, band_h) / zoom), &data));
            blit(&mut canvas, w, &bmp, 0, h - band_h as usize);
        }
        yuv.read_rgba8(RgbaSliceU8::new(canvas.as_flattened(), (w, h)));
        let pics = enc.encode(&yuv, hns(ticks), hns(ticks + durs[i] as u64) - hns(ticks), &mut sps, &mut pps)?;
        ticks += durs[i] as u64;
        waiting.push_back(durs[i]);
        put(pics, &sps, &pps, &mut waiting)?;
        progress((i + 1) as f32 / times.len() as f32);
    }
    let pics = enc.finish(&mut sps, &mut pps)?;
    put(pics, &sps, &pps, &mut waiting)?;
    if !waiting.is_empty() {
        return Err(format!("encoder {w}x{h} lost {} pictures", waiting.len()));
    }
    mux.ok_or("no footage in this range")?.finish().map_err(&err)?;
    notes.extend(streams.into_iter().filter_map(|s| Some(format!("{}: {}", CAMERAS[s.cam], s.note?))));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::to_rgba;
    use crate::library::{Kind, scan};
    use crate::mp4::Sample;
    use crate::sei;
    use eframe::egui::Align2;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("otc-export-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// (fixture with 2 s segments, output folder).
    fn fixture_dirs(name: &str) -> (PathBuf, PathBuf) {
        let d = temp(name);
        fixture::make_fixture(&d.join("in"), 2, 64, 48).unwrap();
        let out = d.join("out");
        fs::create_dir(&out).unwrap();
        (d, out)
    }

    fn event(root: &Path, kind: Kind) -> Event {
        scan(&root.join("in")).unwrap().into_iter().find(|e| e.kind == kind).unwrap()
    }

    fn job(ev: Event, from: f64, to: f64, base: PathBuf, mode: Mode, csv: bool) -> Job {
        Job { segs: Player::open(&ev).segs, event: ev, from, to, base, mode, csv }
    }

    fn only(cam: usize) -> Mode {
        let mut c = [false; 6];
        c[cam] = true;
        Mode::Lossless(c)
    }

    fn list(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn trim_stays_ordered_and_inside_the_event() {
        let mut t = Trim::default();
        assert_eq!(t.range(60.0), (0.0, 60.0));
        assert!(!t.is_set());
        t.set_in(10.0);
        assert_eq!(t.range(60.0), (10.0, 60.0));
        t.set_out(20.0);
        assert_eq!(t.range(60.0), (10.0, 20.0));
        // Out before In drops In; In after Out drops Out. Never a backwards range.
        t.set_out(5.0);
        assert_eq!((t.start, t.end), (None, Some(5.0)));
        t.set_in(30.0);
        assert_eq!((t.start, t.end), (Some(30.0), None));
        t.set_out(30.0);
        assert_eq!((t.start, t.end), (None, Some(30.0)), "In at the very same time as Out isn't a range");
        // The same point again clears only that point.
        t.set_in(10.0);
        t.set_in(10.0);
        assert_eq!((t.start, t.end), (None, Some(30.0)));
        t.set_out(30.0);
        assert_eq!(t, Trim::default());
        // A range outside a shorter event is clamped.
        let t = Trim { start: Some(70.0), end: Some(80.0) };
        assert_eq!(t.range(60.0), (60.0, 60.0));
        let t = Trim { start: Some(-5.0), end: Some(30.0) };
        assert_eq!(t.range(60.0), (0.0, 30.0));
        assert_eq!(Trim::default().range(-1.0), (0.0, 0.0));
    }

    #[test]
    fn footage_leaves_out_gaps() {
        let s = |start: f64, dur: f64| SegTime { start, dur, frames: vec![] };
        let segs = [s(0.0, 2.0), s(2.0, 2.0), s(49.0, 2.0), s(60.0, 0.0)];
        assert_eq!(footage(&segs, 0.0, 51.0), 6.0);
        assert_eq!(footage(&segs, 1.0, 49.5), 3.5);
        assert_eq!(footage(&segs, 10.0, 20.0), 0.0);
    }

    /// `n` samples, a keyframe every `gop`, 1000 timescale units apart at 36000/s.
    fn track(n: usize, gop: usize, avcc: &[u8]) -> Option<Track> {
        let samples = (0..n as u64).map(|i| Sample { offset: 0, size: 1, dts: i * 1000, pts: i * 1000, dur: 1000, key: i % gop as u64 == 0 }).collect();
        Some(Track { timescale: 36000, width: 64, height: 48, avcc: avcc.to_vec(), sps: vec![], pps: vec![], nal_len: 4, samples, duration: n as u64 * 1000 })
    }

    fn seg(start: f64, dur: f64) -> SegTime {
        SegTime { start, dur, frames: vec![] }
    }

    fn piece(seg: usize, samples: Range<usize>) -> Piece {
        Piece { seg, samples }
    }

    #[test]
    fn the_plan_starts_at_a_keyframe_and_covers_out() {
        let segs = [seg(0.0, 2.0)];
        let tracks = [track(72, 36, b"a")];
        assert_eq!(plan(&segs, &tracks, 1.5, 1.75), [[piece(0, 36..64)]]);
        // In on a keyframe; In right before one still needs the earlier one.
        assert_eq!(plan(&segs, &tracks, 1.0, 1.5), [[piece(0, 36..55)]]);
        assert_eq!(plan(&segs, &tracks, 0.99, 1.5), [[piece(0, 0..55)]]);
        assert_eq!(plan(&segs, &tracks, 0.0, 9.0), [[piece(0, 0..72)]], "Out past the end");
    }

    #[test]
    fn the_plan_splits_where_time_or_format_breaks() {
        let (a, b) = (track(72, 36, b"a"), track(72, 36, b"b"));
        // Touching, and within the filename's 1 s resolution: one run.
        let t = [a.clone(), a.clone()];
        assert_eq!(plan(&[seg(0.0, 2.0), seg(2.0, 2.0)], &t, 1.5, 2.5), [[piece(0, 36..72), piece(1, 0..19)]]);
        assert_eq!(plan(&[seg(0.0, 2.0), seg(2.9, 2.0)], &t, 1.5, 3.0).len(), 1);
        // A real gap: two runs, the second from its clip's start.
        assert_eq!(plan(&[seg(0.0, 2.0), seg(47.0, 2.0)], &t, 1.5, 47.5), [[piece(0, 36..72)], [piece(1, 0..19)]]);
        // Another avcC: two runs.
        let t2 = [a.clone(), b];
        assert_eq!(plan(&[seg(0.0, 2.0), seg(2.0, 2.0)], &t2, 1.5, 2.5).len(), 2);
        // Another timescale: durations wouldn't mean the same.
        let mut other = a.clone().unwrap();
        other.timescale = 90000;
        other.duration = 72000;
        assert_eq!(plan(&[seg(0.0, 2.0), seg(2.0, 2.0)], &[a.clone(), Some(other)], 1.5, 2.5).len(), 2);
        // A clip missing in the middle ends the run, one outside the range doesn't.
        let t3 = [a.clone(), None, a.clone()];
        let segs = [seg(0.0, 2.0), seg(2.0, 2.0), seg(4.0, 2.0)];
        assert_eq!(plan(&segs, &t3, 1.0, 5.0), [[piece(0, 36..72)], [piece(2, 0..37)]]);
        assert_eq!(plan(&segs, &t3, 4.5, 5.0), [[piece(2, 0..37)]]);
        assert!(plan(&segs, &[None, None, None], 0.0, 6.0).is_empty());
        // A clip that doesn't start on a keyframe can't continue a run or be cut before its first one.
        let mut late = a.clone().unwrap();
        late.samples.iter_mut().for_each(|s| s.key = s.pts == 36000);
        assert_eq!(plan(&segs[..2], &[a, Some(late.clone())], 1.5, 3.5), [[piece(0, 36..72)], [piece(1, 36..55)]]);
        assert_eq!(plan(&segs[..1], &[Some(late)], 0.0, 0.5), Vec::<Vec<Piece>>::new());
    }

    #[test]
    fn a_range_ending_on_a_boundary_takes_the_next_clips_first_frame() {
        let t = [track(72, 36, b"a"), track(72, 36, b"a")];
        let segs = [seg(0.0, 2.0), seg(2.0, 2.0)];
        assert_eq!(plan(&segs, &t, 1.0, 2.0), [[piece(0, 36..72), piece(1, 0..1)]]);
        assert_eq!(plan(&segs, &t, 2.0, 2.5), [[piece(1, 0..19)]], "In on the boundary skips the old clip");
    }

    #[test]
    fn the_plan_leaves_no_hole_in_presentation_order() {
        // Decode order 0, 4, 2, 1, 3, then the next group. Out at 2.5 alone would end on 1 and lose 3,
        // which displays before the 4 that's already in.
        let mut t = track(7, 7, b"a").unwrap();
        for (s, pts) in t.samples.iter_mut().zip([0, 4000, 2000, 1000, 3000, 6000, 5000]) {
            s.pts = pts;
        }
        let segs = [seg(0.0, 7000.0 / 36000.0)];
        let t = [Some(t)];
        assert_eq!(plan(&segs, &t, 0.0, 2500.0 / 36000.0), [[piece(0, 0..5)]]);
        // Nothing to add when the kept samples already run up to Out.
        assert_eq!(plan(&segs, &t, 0.0, 4000.0 / 36000.0), [[piece(0, 0..5)]]);
        assert_eq!(plan(&segs, &t, 0.0, 500.0 / 36000.0), [[piece(0, 0..1)]]);
    }

    #[test]
    fn a_failed_commit_leaves_no_final_file() {
        let dir = temp("commit");
        let mut parts = Parts::default();
        let (a, b) = (dir.join("a.mp4"), dir.join("b.mp4"));
        fs::write(parts.add(a.clone()), b"a").unwrap();
        // The second rename fails: its .part isn't there.
        let _ = parts.add(b);
        let c = parts.add(dir.join("c.mp4"));
        fs::write(c, b"c").unwrap();
        assert!(parts.commit().is_err());
        drop(parts);
        assert!(list(&dir).is_empty(), "{:?}", list(&dir));
        // What was there before survives, under its own name.
        fs::write(&a, b"old").unwrap();
        let mut parts = Parts::default();
        fs::write(parts.add(a.clone()), b"new").unwrap();
        let _ = parts.add(dir.join("b.mp4"));
        assert!(parts.commit().is_err());
        drop(parts);
        assert_eq!(list(&dir), ["a.mp4"]);
        assert_eq!(fs::read(&a).unwrap(), b"old");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lossless_says_what_is_missing_inside_the_range() {
        let (root, out) = fixture_dirs("holes");
        let ev = event(&root, Kind::Recent);
        let middle = ev.segments[1].files[0].clone().unwrap();
        let go = |ev: Event, from: f64, to: f64, name: &str| run(&job(ev, from, to, out.join(name), only(0), false), &AtomicBool::new(false), |_| {});

        // The clip is gone: other cameras still give the segment a length.
        let mut gone = ev.clone();
        gone.segments[1].files[0] = None;
        let done = go(gone, 0.0, 51.0, "gone").unwrap();
        assert_eq!(done.files.len(), 2);
        assert_eq!(done.notes, ["front: no clip for segment 2"]);

        // The clip is cut short: the note names it and says why.
        File::options().write(true).open(&middle).unwrap().set_len(40).unwrap();
        let done = go(ev.clone(), 0.0, 51.0, "cut").unwrap();
        assert_eq!(done.files.len(), 2);
        assert_eq!(done.notes.len(), 1, "{:?}", done.notes);
        assert!(done.notes[0].starts_with("front: ") && done.notes[0].contains(&*middle.to_string_lossy()), "{:?}", done.notes);

        // Nothing but that clip in range: the error gives the real cause.
        let e = go(ev, 2.5, 3.5, "only").unwrap_err();
        assert!(e.starts_with("no footage in this range\nfront: ") && e.contains(&*middle.to_string_lossy()), "{e}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_range_past_the_footage_is_refused_in_both_modes() {
        let (root, out) = fixture_dirs("past");
        let ev = event(&root, Kind::Saved);
        let dur = Player::open(&ev).duration();
        for mode in [only(0), composite_mode(320, 180, false)] {
            let e = run(&job(ev.clone(), dur, dur, out.join("n"), mode, false), &AtomicBool::new(false), |_| {}).unwrap_err();
            assert_eq!(e, "no footage in this range");
        }
        assert!(list(&out).is_empty(), "{:?}", list(&out));
        // A zero-length range inside a clip still gives its one frame.
        let done = run(&job(ev, 1.0, 1.0, out.join("one"), composite_mode(320, 180, false), false), &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(mp4::read_track(&done.files[0]).unwrap().samples.len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lossless_is_byte_identical_across_a_boundary() {
        let (root, out) = fixture_dirs("lossless");
        let ev = event(&root, Kind::Saved);
        let src: Vec<PathBuf> = ev.segments.iter().map(|s| s.files[0].clone().unwrap()).collect();
        let (from, to) = (1.5, 2.5);
        let base = out.join("clip");
        let done = run(&job(ev, from, to, base.clone(), only(0), true), &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(done.files, [named(&base, "-front.mp4"), named(&base, "-telemetry.csv")]);
        assert!(done.notes.is_empty());
        assert_eq!(list(&out), ["clip-front.mp4", "clip-telemetry.csv"], "no .part left");

        let got = mp4::read_track(&done.files[0]).unwrap();
        let a = mp4::read_track(&src[0]).unwrap();
        let b = mp4::read_track(&src[1]).unwrap();
        assert_eq!((got.avcc.as_slice(), got.timescale, got.width), (a.avcc.as_slice(), a.timescale, a.width));
        // The keyframe before In (t = 1.0) through the frame showing Out, in source order.
        let wanted: Vec<(&Path, &Sample)> = a.samples[36..].iter().map(|s| (src[0].as_path(), s)).chain(b.samples[..19].iter().map(|s| (src[1].as_path(), s))).collect();
        assert_eq!(got.samples.len(), wanted.len());
        assert!(got.samples[0].key);
        let start_at = a.samples[36].pts as f64 / a.timescale as f64;
        assert!(start_at <= from);
        let last = got.samples.last().unwrap();
        assert!(start_at + (last.pts + last.dur as u64) as f64 / got.timescale as f64 >= to, "the last sample covers Out");

        let mut f = File::open(&done.files[0]).unwrap();
        for (i, (s, (path, w))) in got.samples.iter().zip(&wanted).enumerate() {
            let want = mp4::read_sample(&mut File::open(path).unwrap(), w).unwrap();
            assert_eq!(mp4::read_sample(&mut f, s).unwrap(), want, "sample {i}");
            assert_eq!((s.dur, s.key), (w.dur, w.key), "sample {i}");
            assert_eq!(s.pts, i as u64 * 1000, "sample {i}: no jump at the boundary");
        }

        // SEI rows, what --dump-sei prints, match the source's for that range.
        let rows = |p: &Path| sei::read_telemetry(p).unwrap().1.iter().map(|f| sei::csv_row(&f.t)).collect::<Vec<_>>();
        let mut want = rows(&src[0])[36..].to_vec();
        want.extend_from_slice(&rows(&src[1])[..19]);
        assert_eq!(rows(&done.files[0]), want);

        // The CSV holds the records from In through Out.
        let csv = fs::read_to_string(&done.files[1]).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert!(lines[0].starts_with("event_time_s,"));
        assert_eq!(lines.len() - 1, 37, "t = 1.5 to 2.5 at 36 fps");
        assert!(lines[1].starts_with("1.500,"), "{}", lines[1]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lossless_splits_at_a_gap_and_notes_a_missing_camera() {
        let (root, out) = fixture_dirs("gap");
        let mut ev = event(&root, Kind::Recent);
        ev.segments.iter_mut().for_each(|s| s.files[1] = None);
        let base = out.join("r");
        let mut cams = [false; 6];
        (cams[0], cams[1]) = (true, true);
        // Segments at 0, 2 and 49 s.
        let done = run(&job(ev.clone(), 1.0, 49.5, base.clone(), Mode::Lossless(cams), false), &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(done.files, [named(&base, "-front.mp4"), named(&base, "-front-2.mp4")]);
        assert_eq!(done.notes, ["back: no footage in this range"]);
        let (a, b) = (mp4::read_track(&done.files[0]).unwrap(), mp4::read_track(&done.files[1]).unwrap());
        assert_eq!(a.samples.len(), 36 + 72, "1 s of the first clip and all of the second, no time invented");
        assert_eq!(b.samples.len(), 19);
        assert!(a.samples[0].key && b.samples[0].key);
        assert_eq!(b.samples[0].pts, 0);

        // Nothing at all to copy is an error, not an empty file.
        let e = run(&job(ev, 1.0, 2.0, out.join("x"), only(1), false), &AtomicBool::new(false), |_| {}).unwrap_err();
        assert!(e.contains("no footage"), "{e}");
        assert!(!list(&out).iter().any(|f| f.starts_with('x')));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn no_vehicle_data_writes_no_csv_and_says_so() {
        let (root, out) = fixture_dirs("nocsv");
        let ev = event(&root, Kind::Sentry);
        let base = out.join("s");
        let done = run(&job(ev, 0.0, 1.0, base.clone(), only(0), true), &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(done.files, [named(&base, "-front.mp4")]);
        assert_eq!(done.notes, ["no vehicle data in this range"]);
        assert_eq!(list(&out), ["s-front.mp4"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_and_failure_leave_no_file() {
        let (root, out) = fixture_dirs("cancel");
        let ev = event(&root, Kind::Saved);
        let cancel = AtomicBool::new(false);
        let mut seen = None;
        let j = job(ev.clone(), 0.0, 4.0, out.join("c"), Mode::Lossless([true; 6]), true);
        let r = run(&j, &cancel, |f| {
            if f > 0.2 && seen.is_none() {
                seen = Some(list(&out));
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(r.unwrap_err(), CANCELLED);
        let seen = seen.expect("got that far");
        assert!(!seen.is_empty() && seen.iter().all(|f| f.ends_with(".part")), "{seen:?}");
        assert!(list(&out).is_empty(), "{:?}", list(&out));
        // A destination that can't be written reports why.
        let bad = job(ev.clone(), 0.0, 1.0, out.join("missing").join("c"), only(0), false);
        assert!(run(&bad, &AtomicBool::new(false), |_| {}).is_err());
        assert!(list(&out).is_empty());
        // A repeat doesn't overwrite what's there.
        let j = job(ev, 0.0, 1.0, out.join("c"), only(0), false);
        let first = run(&j, &AtomicBool::new(false), |_| {}).unwrap();
        let second = run(&j, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(second.files, [out.join("c-front (2).mp4")]);
        assert!(first.files[0].exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// Times computed as start + pts / timescale must land on that pts, not the frame before it.
    #[test]
    fn composited_frames_are_the_exact_ones_at_any_segment_start() {
        let (root, _) = fixture_dirs("exact");
        let ev = event(&root, Kind::Recent);
        let clock = Player::open(&ev);
        assert_eq!(clock.segs.iter().map(|s| s.start).collect::<Vec<_>>(), [0.0, 2.0, 49.0]);
        for seg in 1..3 {
            let s = &clock.segs[seg];
            let mut pts: Vec<u64> = mp4::read_track(ev.segments[seg].files[0].as_ref().unwrap()).unwrap().samples.iter().map(|x| x.pts).collect();
            pts.sort();
            let mut st = Stream::new(0, &ev);
            for (k, f) in s.frames.iter().enumerate() {
                let (at, local) = clock.locate(s.start + f).unwrap();
                st.advance(at, local);
                assert_eq!(st.shown, Some(pts[k]), "segment {seg}, frame {k}");
            }
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn frame_times_follow_the_clock() {
        let s = |start: f64, n: usize| SegTime { start, dur: n as f64 * 0.25, frames: (0..n).map(|k| k as f64 * 0.25).collect() };
        let p = Player::new(vec![s(0.0, 8), s(2.0, 8), s(10.0, 4)], 0);
        assert_eq!(frame_times(&p, 0.5, 1.0), [0.5, 0.75, 1.0]);
        assert_eq!(frame_times(&p, 0.6, 1.0), [0.5, 0.75, 1.0], "starts with the frame showing In");
        assert_eq!(frame_times(&p, 0.6, 0.6), [0.5], "a range between frames still shows one");
        assert_eq!(frame_times(&p, 3.5, 10.25), [3.5, 3.75, 10.0, 10.25], "the gap is skipped");
        assert_eq!(frame_times(&p, 100.0, 200.0), [10.75]);
        assert!(frame_times(&Player::new(vec![seg(0.0, 0.0)], 0), 0.0, 1.0).is_empty());
        // Overlap: the later segment owns it.
        let p = Player::new(vec![s(0.0, 8), s(1.5, 4)], 0);
        assert_eq!(frame_times(&p, 1.0, 2.0), [1.0, 1.25, 1.5, 1.75, 2.0]);
    }

    #[test]
    fn durations_keep_spacing_and_cut_at_gaps() {
        assert_eq!(durations(&[0.0, 0.25, 0.5]), [22500, 22500, 22500]);
        assert_eq!(durations(&[3.75, 10.0, 10.1]), [2500, 9000, 9000], "no time spent in the gap");
        assert_eq!(durations(&[1.0]), [2500]);
    }

    #[test]
    fn scaling_averages_down_and_repeats_up() {
        let src = [0, 0, 0, 255, 100, 100, 100, 255, 20, 20, 20, 255, 40, 40, 40, 255];
        let mut b = Bitmap::default();
        scale(&src, (2, 2), &mut b, (1, 1));
        assert_eq!(b.px, [[40, 40, 40, 255]]);
        scale(&src, (2, 2), &mut b, (4, 2));
        assert_eq!(b.px[..4], [[0, 0, 0, 255], [0, 0, 0, 255], [100, 100, 100, 255], [100, 100, 100, 255]]);
        assert_eq!((b.w, b.h, b.px.len()), (4, 2, 8));
    }

    #[test]
    fn blit_clips_at_the_edges() {
        let mut canvas = vec![[0u8; 4]; 12];
        let b = Bitmap { w: 2, h: 2, px: vec![[9; 4]; 4] };
        blit(&mut canvas, 4, &b, 3, 2);
        assert_eq!(canvas.iter().filter(|p| p[0] == 9).count(), 1);
        assert_eq!(canvas[11], [9; 4]);
    }

    #[test]
    fn chrome_rasterizes_shapes_and_text() {
        let mut c = Chrome::new(1.5);
        let b = c.paint(150, 60, |ui| {
            ui.painter().rect_filled(Rect::from_min_size(Pos2::ZERO, vec2(40.0, 30.0)), 0.0, theme::TEXT);
            ui.painter().text(pos2(50.0, 20.0), Align2::LEFT_CENTER, "88", theme::mono(16.0), theme::TEXT);
        });
        let at = |x: usize, y: usize| b.px[y * b.w + x];
        assert_eq!((b.w, b.h), (150, 60));
        assert_eq!(at(10, 10), theme::TEXT.to_array());
        // Across the 16-row stripes drawn on separate threads, with no seam. (Row 0 has a few edge pixels off, as it always had.)
        assert!((1..45).all(|y| (0..60).all(|x| at(x, y) == theme::TEXT.to_array())), "the rect is whole");
        assert_eq!(at(58, 10), theme::TEXT.to_array(), "40 points is 60 pixels");
        assert_eq!(at(62, 10), theme::BG.to_array());
        assert_eq!(at(10, 50), theme::BG.to_array());
        let lit = (75..110).flat_map(|x| (10..50).map(move |y| (x, y))).filter(|&(x, y)| at(x, y)[0] > 200).count();
        assert!(lit > 20, "the glyphs came from the atlas: {lit}");
    }

    #[test]
    fn nothing_is_placed_over_the_band() {
        let cells = [(0, [0.0, 0.0, 8.0, 8.0]), (1, [8.0, 4.0, 4.0, 4.0])];
        for (w, h) in [(1280, 720), (1920, 1080), (3840, 2160)] {
            let zoom = h as f32 / CANVAS_H;
            let band = (BAND_H * zoom).round();
            for p in places(w, h as usize, band, zoom, &cells) {
                assert!(p.video.max.y <= h as f32 - band);
                assert!(p.caption.max.y <= p.video.min.y);
                assert!(p.video.max.x <= w as f32);
            }
        }
    }

    #[test]
    fn annex_b_splits_into_nals() {
        let au = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x65, 0, 0, 3, 0, 9, 0, 0];
        let nals = fixture::annex_b_nals(&au);
        assert_eq!(nals, [&[0x67, 1, 2][..], &[0x68, 3], &[0x09, 0xf0], &[0x65, 0, 0, 3, 0, 9]]);
        let (mut sps, mut pps) = (Vec::new(), Vec::new());
        let (sample, key) = picture(&au, &mut sps, &mut pps);
        assert!(key);
        assert_eq!((sps.as_slice(), pps.as_slice()), (&[0x67, 1, 2][..], &[0x68, 3][..]));
        assert_eq!(sample, [0, 0, 0, 6, 0x65, 0, 0, 3, 0, 9]);
        assert!(!picture(&[0, 0, 1, 0x41, 7], &mut sps, &mut pps).1, "a P slice isn't a keyframe");
    }

    /// The GPU path where the machine has an encoder, the CPU with a note where it hasn't.
    #[test]
    fn gpu_export_decodes_back_or_falls_back() {
        let (root, out) = fixture_dirs("gpu");
        let ev = event(&root, Kind::Saved);
        for (w, h) in [(640, 360), (1920, 1080)] {
            let Mode::Composite(c) = composite_mode(w, h, true) else { unreachable!() };
            let done = run(&job(ev.clone(), 0.0, 2.5, out.join(format!("g{w}")), Mode::Composite(Composite { gpu: true, ..c }), false), &AtomicBool::new(false), |_| {}).unwrap();
            if gpu_encoder().is_some() {
                assert!(done.notes.is_empty(), "{w}x{h}: {:?}", done.notes);
            } else {
                assert!(done.notes[0].starts_with("GPU encoder not available"), "{:?}", done.notes);
            }
            let (n, top, bottom, t) = decode_all(&done.files[0]);
            assert_eq!((t.width, t.height), (w, h));
            assert_eq!((t.samples.len(), n), (91, 91), "every frame, each decodes");
            let keys: Vec<usize> = (0..91).filter(|&i| t.samples[i].key).collect();
            assert_eq!(keys[0], 0);
            assert!(keys.windows(2).all(|k| k[1] - k[0] <= fixture::FPS as usize), "a keyframe at least every second: {keys:?}");
            assert!(top > 60 && bottom > 150, "cameras {top}, strip {bottom}");
            let total = t.samples.iter().map(|s| s.dur as u64).sum::<u64>() as f64 / t.timescale as f64;
            assert!((total - 91.0 / 36.0).abs() < 0.01, "plays in source time: {total}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    fn composite_mode(width: u32, height: u32, hud: bool) -> Mode {
        let cells = (0..6).map(|c| (c, [(c % 3) as f32 * 4.0, (c / 3) as f32 * 4.0, 4.0, 4.0])).collect();
        Mode::Composite(Composite { width, height, bps: 4_000_000, hud, map: hud, metric: true, cells, gpu: false })
    }

    /// (frames decoded, brightest pixel in the picture's top part, brightest in the bottom 5 %).
    fn decode_all(path: &Path) -> (usize, u8, u8, Track) {
        let t = mp4::read_track(path).unwrap();
        let mut f = File::open(path).unwrap();
        let mut d = CamDecoder::new(&t).unwrap();
        let (mut n, mut top, mut bottom, mut rgba) = (0, 0, 0, Vec::new());
        for s in &t.samples {
            let data = mp4::read_sample(&mut f, s).unwrap();
            if let Some(yuv) = d.decode(&data, s.key).unwrap() {
                let (w, h) = to_rgba(&yuv, &mut rgba);
                let (w, h) = (w as usize, h as usize);
                n += 1;
                top = top.max(rgba[..w * h / 2 * 4].iter().step_by(4).copied().max().unwrap());
                bottom = bottom.max(rgba[w * (h - h / 20) * 4..].iter().step_by(4).copied().max().unwrap());
            }
        }
        n += d.flush().unwrap().len();
        (n, top, bottom, t)
    }

    #[test]
    fn composited_export_decodes_back() {
        let (root, out) = fixture_dirs("composite");
        let ev = event(&root, Kind::Saved);
        let base = out.join("v");
        let mut last = 0.0;
        // Odd sizes come out even; 19 frames from t = 1.5 to t = 2.0, crossing the segment boundary at the end.
        let done = run(&job(ev.clone(), 1.5, 2.0, base.clone(), composite_mode(641, 361, true), true), &AtomicBool::new(false), |p| last = p).unwrap();
        assert_eq!(done.files, [named(&base, ".mp4"), named(&base, "-telemetry.csv")]);
        assert_eq!(last, 1.0);
        assert!(done.notes.is_empty(), "{:?}", done.notes);
        let (n, top, bottom, t) = decode_all(&done.files[0]);
        assert_eq!((t.width, t.height), (640, 360));
        assert_eq!(t.samples.len(), 19);
        assert_eq!(n, 19, "every sample decodes");
        assert!(t.samples[0].key);
        assert!(top > 60, "the cameras aren't black: {top}");
        assert!(bottom > 150, "the strip's text is there: {bottom}");
        let total = t.samples.iter().map(|s| s.dur as u64).sum::<u64>() as f64 / t.timescale as f64;
        assert!((total - 19.0 / 36.0).abs() < 0.01, "plays in source time: {total}");

        // The real sizes encode, and a missing camera leaves a note rather than failing.
        let mut ev2 = ev.clone();
        ev2.segments.iter_mut().for_each(|s| s.files[4] = None);
        let (_, w, h, bps) = RESOLUTIONS[2];
        let Mode::Composite(c) = composite_mode(w, h, false) else { unreachable!() };
        let done = run(&job(ev2, 1.0, 1.0, out.join("k"), Mode::Composite(Composite { bps, ..c }), false), &AtomicBool::new(false), |_| {}).unwrap();
        let t = mp4::read_track(&done.files[0]).unwrap();
        assert_eq!((t.width, t.height, t.samples.len()), (3840, 2160, 1));
        assert_eq!(done.notes, ["left_pillar: no clip"]);

        // Cancelled: nothing stays.
        let cancel = AtomicBool::new(true);
        let before = list(&out);
        let r = run(&job(ev, 0.0, 1.0, out.join("z"), composite_mode(320, 180, false), false), &cancel, |_| {});
        assert_eq!(r.unwrap_err(), CANCELLED);
        assert_eq!(list(&out), before);
        let _ = fs::remove_dir_all(&root);
    }
}
