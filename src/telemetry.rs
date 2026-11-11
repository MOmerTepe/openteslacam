//! One event's vehicle data on the event's timeline: lookup at a time, and everything the timeline
//! lanes draw. Built off the UI thread.
//!
//! Sign conventions (the proto documents none; measured on real HW4 footage in November 2026 against
//! heading, speed and yaw): `steering_deg` > 0 is right. `accel_y` is longitudinal and negative when
//! speeding up, so forward acceleration is `-accel_y` and braking is negative g. `accel_x` is lateral
//! (> 0 in right turns) and unused.
// shortcut: checked on one car only, other models or firmware may differ; flip `steer_y`, `steer_text`, `wheel` and `smooth_g` if a clip from one disagrees.

use crate::library::Event;
use crate::mp4;
use crate::player::SegTime;
use crate::sei::{self, AP_NONE, Telemetry};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Hard brake or accel above this. Tesla's own Safety Score counts braking above 0.3 g as hard, and
/// the mockup fills the strip's g-force at the same value. An ordinary pull-away stays under it.
pub const HARD_G: f32 = 0.3;
const STANDARD_G: f64 = 9.80665;
/// g is averaged over this many seconds: the IMU sees every pothole, a stop lasts longer than that.
const G_WINDOW: f64 = 0.25;
/// Hard moments of the same sign this close (seconds) are one stop: drivers pump and ease off.
const HARD_MERGE: f64 = 3.0;
/// Samples further apart than this (seconds) have a hole between them; a sample stands in for the
/// clock up to this far from it. Far above the ~28 ms spacing, so a frame without SEI doesn't flicker.
pub const HOLD: f64 = 0.5;
/// A blinker that reads on without a break gets a chevron this often, one flash of a real lamp.
const FLASH: f64 = 2.0 / 3.0;
/// Frames of a non-front clip checked for SEI before reading all of it.
const PROBE_SAMPLES: usize = 36;

#[derive(Clone, Copy, Debug)]
pub struct Record {
    /// Seconds from the event start.
    pub t: f64,
    /// Smoothed longitudinal g, signed.
    pub g: f32,
    pub tel: Telemetry,
}

/// An unbroken autopilot state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub t0: f64,
    pub t1: f64,
    pub state: i32,
}

/// One merged hard stop (`g` < 0) or hard acceleration, at its peak.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HardEvent {
    pub t: f64,
    pub g: f32,
}

/// One pixel column of the lanes: extremes win, so peaks and short brake taps survive downsampling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Column {
    /// m/s, ≥ 0.
    pub speed: f32,
    /// Accelerator, 0–100 %.
    pub gas: f32,
    pub brake: bool,
    pub steer_lo: f32,
    pub steer_hi: f32,
}

impl Column {
    fn of(r: &Record) -> Column {
        let t = &r.tel;
        Column { speed: t.speed_mps.abs(), gas: t.accel_pct.clamp(0.0, 100.0), brake: t.brake, steer_lo: t.steering_deg, steer_hi: t.steering_deg }
    }

    fn merge(&mut self, o: Column) {
        self.speed = self.speed.max(o.speed);
        self.gas = self.gas.max(o.gas);
        self.brake |= o.brake;
        self.steer_lo = self.steer_lo.min(o.steer_lo);
        self.steer_hi = self.steer_hi.max(o.steer_hi);
    }
}

#[derive(Default)]
pub struct Store {
    /// Sorted by `t`. Everything else derives from these.
    pub records: Vec<Record>,
    /// Chevron times, one per flash. Hazards simply appear in both.
    pub left: Vec<f64>,
    pub right: Vec<f64>,
    pub bands: Vec<Band>,
    pub hard: Vec<HardEvent>,
    /// Fastest of the event, m/s: the speed lane's scale.
    pub speed_max: f32,
}

impl Store {
    /// `g` of the records is filled in here.
    pub fn build(mut records: Vec<Record>) -> Store {
        records.sort_by(|a, b| a.t.total_cmp(&b.t));
        smooth_g(&mut records);
        Store {
            left: flashes(&records, |t| t.blink_left),
            right: flashes(&records, |t| t.blink_right),
            bands: bands(&records),
            hard: hard_events(&records),
            speed_max: records.iter().map(|r| r.tel.speed_mps.abs()).fold(0.0, f32::max),
            records,
        }
    }

    /// The sample that stands for time `t`, or None where there is no data (a gap, or a segment
    /// without SEI).
    pub fn at(&self, t: f64) -> Option<&Record> {
        let i = self.records.partition_point(|r| r.t <= t);
        let near = |r: &&Record| (r.t - t).abs() <= HOLD;
        i.checked_sub(1).map(|k| &self.records[k]).filter(near).or_else(|| self.records.get(i).filter(near))
    }

    /// Downsamples to `n` columns across `dur` seconds, O(records). None where there is no data.
    pub fn columns(&self, dur: f64, n: usize) -> Vec<Option<Column>> {
        let mut out = vec![None; n];
        if dur <= 0.0 {
            return out;
        }
        let r = &self.records;
        let mut i = 0;
        for (c, slot) in out.iter_mut().enumerate() {
            let end = (c + 1) as f64 * dur / n as f64;
            let mut col: Option<Column> = None;
            while i < r.len() && r[i].t < end {
                let o = Column::of(&r[i]);
                col.get_or_insert(o).merge(o);
                i += 1;
            }
            // Zoomed in past the sample spacing: hold the last sample, unless this is a real hole.
            *slot = col.or_else(|| {
                let (a, b) = (r.get(i.checked_sub(1)?)?, r.get(i)?);
                (b.t - a.t <= HOLD).then(|| Column::of(a))
            });
        }
        out
    }

    /// `sei::csv_row` columns after the event time, for records with `t` in `from..=to`.
    pub fn write_csv(&self, out: &mut impl Write, from: f64, to: f64) -> io::Result<()> {
        writeln!(out, "event_time_s,{}", sei::csv_header())?;
        for r in self.records.iter().filter(|r| (from..=to).contains(&r.t)) {
            writeln!(out, "{:.3},{}", r.t, sei::csv_row(&r.tel))?;
        }
        Ok(())
    }
}

fn smooth_g(r: &mut [Record]) {
    let (mut lo, mut hi, mut sum) = (0, 0, 0.0);
    for i in 0..r.len() {
        let t = r[i].t;
        while hi < r.len() && r[hi].t <= t + G_WINDOW / 2.0 {
            sum -= r[hi].tel.accel_y;
            hi += 1;
        }
        while r[lo].t < t - G_WINDOW / 2.0 {
            sum += r[lo].tel.accel_y;
            lo += 1;
        }
        r[i].g = (sum / (hi - lo) as f64 / STANDARD_G) as f32;
    }
}

fn flashes(r: &[Record], on: impl Fn(&Telemetry) -> bool) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    let mut prev: Option<&Record> = None;
    for rec in r {
        if on(&rec.tel) {
            let still_lit = prev.is_some_and(|p| on(&p.tel) && rec.t - p.t <= HOLD);
            if !still_lit || out.last().is_none_or(|l| rec.t - l >= FLASH) {
                out.push(rec.t);
            }
        }
        prev = Some(rec);
    }
    out
}

fn bands(r: &[Record]) -> Vec<Band> {
    let mut out: Vec<Band> = Vec::new();
    for rec in r.iter().filter(|rec| rec.tel.autopilot != AP_NONE) {
        let state = rec.tel.autopilot;
        match out.last_mut() {
            Some(b) if b.state == state && rec.t - b.t1 <= HOLD => b.t1 = rec.t,
            _ => out.push(Band { t0: rec.t, t1: rec.t, state }),
        }
    }
    out
}

fn hard_events(r: &[Record]) -> Vec<HardEvent> {
    let mut out: Vec<HardEvent> = Vec::new();
    let mut last_over = f64::NEG_INFINITY;
    for rec in r.iter().filter(|rec| rec.g.abs() >= HARD_G) {
        match out.last_mut() {
            Some(h) if h.g.signum() == rec.g.signum() && rec.t - last_over <= HARD_MERGE => {
                if rec.g.abs() > h.g.abs() {
                    *h = HardEvent { t: rec.t, g: rec.g };
                }
            }
            _ => out.push(HardEvent { t: rec.t, g: rec.g }),
        }
        last_over = rec.t;
    }
    out
}

/// Speed in the chosen units. Absolute: reverse may report negative speed (the legacy app guards it).
pub fn speed_in(mps: f32, metric: bool) -> f32 {
    mps.abs() * if metric { 3.6 } else { 2.236_936 }
}

/// What the loader reads for one segment.
pub struct Job {
    start: f64,
    /// The next readable segment takes over here (as in `Player::locate`).
    limit: f64,
    files: [Option<PathBuf>; 6],
}

/// `segs` are `Player::segs` of `ev`: each segment keeps its own start, so one without SEI can't shift the rest.
pub fn jobs(ev: &Event, segs: &[SegTime]) -> Vec<Job> {
    let limit = |i: usize| segs[i + 1..].iter().find(|s| s.dur > 0.0).map_or(f64::INFINITY, |s| s.start);
    ev.segments.iter().zip(segs).enumerate().map(|(i, (s, t))| Job { start: t.start, limit: limit(i), files: s.files.clone() }).collect()
}

/// Segments around time `t` first: Saved and Sentry events open near their end, and the data at the
/// playhead shouldn't arrive last. `Store::build` sorts, so the order is free.
pub fn nearest_first(jobs: &mut [Job], t: f64) {
    let away = |j: &Job| if t < j.start { j.start - t } else { (t - j.limit).max(0.0) };
    jobs.sort_by(|a, b| away(a).total_cmp(&away(b)));
}

/// Reads the jobs in order and hands over a store after each segment, so lanes grow in while a long
/// event loads. Stops when `progress` returns false: nobody wants this event any more.
// shortcut: read_telemetry reads every video sample on one thread, so each clip costs its full size off USB (the first one included) and an hour of RecentClips takes minutes; read only each sample's first KB once a real clip confirms SEI comes before the slices.
pub fn load(jobs: Vec<Job>, mut progress: impl FnMut(Store) -> bool) {
    let mut records = Vec::new();
    for job in &jobs {
        records.extend(read_segment(job));
        if !progress(Store::build(records.clone())) {
            return;
        }
    }
}

/// The first clip with SEI, front first. Other cameras get a quick look before a full read: when a
/// parked Sentry clip has no SEI, reading all six off a USB stick would take minutes.
fn read_segment(job: &Job) -> Vec<Record> {
    let clip = job.files.iter().flatten().enumerate().find_map(|(k, p)| {
        if k > 0 && !has_early_sei(p) {
            return None;
        }
        sei::read_telemetry(p).ok().filter(|(_, frames)| !frames.is_empty())
    });
    let Some((track, frames)) = clip else { return Vec::new() };
    let ts = track.timescale.max(1) as f64;
    frames
        .into_iter()
        // Corrupt numbers must not reach the lanes; a finite record is all the UI assumes.
        .filter(|f| [f.t.speed_mps, f.t.accel_pct, f.t.steering_deg].iter().all(|v| v.is_finite()) && f.t.accel_y.is_finite())
        .map(|f| Record { t: job.start + f.pts as f64 / ts, g: 0.0, tel: f.t })
        .filter(|r| r.t < job.limit)
        .collect()
}

fn has_early_sei(path: &Path) -> bool {
    let (Ok(track), Ok(mut f)) = (mp4::read_track(path), File::open(path)) else { return false };
    track.samples.iter().take(PROBE_SAMPLES).any(|s| {
        mp4::read_sample(&mut f, s).is_ok_and(|d| sei::sei_payloads(&d, track.nal_len).iter().any(|p| sei::decode_metadata(p).is_some()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::sei::{AP_AUTOSTEER, AP_TACC};

    fn rec(t: f64, f: impl FnOnce(&mut Telemetry)) -> Record {
        let mut tel = Telemetry::default();
        f(&mut tel);
        Record { t, g: 0.0, tel }
    }

    /// 36 Hz samples over `from..to` (seconds).
    fn span(from: f64, to: f64, f: impl Fn(f64, &mut Telemetry)) -> Vec<Record> {
        let n = ((to - from) * 36.0) as usize;
        (0..n)
            .map(|i| {
                let t = from + i as f64 / 36.0;
                rec(t, |s| f(t, s))
            })
            .collect()
    }

    fn speeds(store: &Store, t: f64) -> Option<f32> {
        store.at(t).map(|r| r.tel.speed_mps)
    }

    #[test]
    fn lookup_at_and_between_samples_across_segments_and_gaps() {
        let mut r: Vec<Record> = [0.0, 0.1, 0.2].iter().map(|&t| rec(t, |s| s.speed_mps = t as f32)).collect();
        r.extend([60.0, 60.1].iter().map(|&t| rec(t, |s| s.speed_mps = t as f32)));
        let store = Store::build(r);
        assert_eq!(speeds(&store, 0.0), Some(0.0), "at a sample");
        assert_eq!(speeds(&store, 0.1), Some(0.1));
        assert_eq!(speeds(&store, 0.15), Some(0.1), "between samples: the latest one");
        assert_eq!(speeds(&store, 0.6), Some(0.2), "held briefly");
        assert_eq!(speeds(&store, 30.0), None, "in a gap");
        assert_eq!(speeds(&store, 59.8), Some(60.0), "a segment whose first frame has no SEI doesn't flicker");
        assert_eq!(speeds(&store, 60.05), Some(60.0), "in the next segment");
        assert_eq!(speeds(&store, 99.0), None, "past the end");
        assert_eq!(speeds(&store, -5.0), None);
        assert!(Store::default().at(0.0).is_none());
    }

    #[test]
    fn blinker_flashes_become_chevrons() {
        let flash = |t: f64| (t * 1.5).fract() < 0.5;
        // Three flashes in 2 s, left only.
        let store = Store::build(span(0.0, 2.0, |t, s| s.blink_left = flash(t)));
        assert_eq!(store.left.len(), 3, "{:?}", store.left);
        assert!((store.left[1] - 2.0 / 3.0).abs() < 0.04);
        assert!(store.right.is_empty());

        // Hazards: both sides at the same moments.
        let store = Store::build(span(0.0, 2.0, |t, s| (s.blink_left, s.blink_right) = (flash(t), flash(t))));
        assert_eq!(store.left, store.right);
        assert_eq!(store.right.len(), 3);

        // A signal that reads on without a break still counts flashes, not one chevron.
        let store = Store::build(span(0.0, 2.0, |_, s| s.blink_right = true));
        assert_eq!(store.right.len(), 3, "{:?}", store.right);

        // Off one frame is a new flash; across a gap too, but not across touching segments.
        let on = |t: f64| rec(t, |s| s.blink_left = true);
        let store = Store::build(vec![on(0.0), rec(0.03, |_| ()), on(0.06)]);
        assert_eq!(store.left, [0.0, 0.06]);
        let store = Store::build(vec![on(0.0), on(60.0)]);
        assert_eq!(store.left, [0.0, 60.0]);
        let store = Store::build(vec![on(0.0), on(0.03)]);
        assert_eq!(store.left, [0.0]);
    }

    #[test]
    fn hard_moments_merge_into_one_marker_at_the_peak() {
        // `ms2` is forward acceleration; the car reports it as -accel_y.
        let pulse = |a: f64, b: f64, ms2: f64, t: f64, s: &mut Telemetry| {
            if (a..b).contains(&t) {
                s.accel_y = -ms2;
            }
        };
        // Two pulses of one stop, 1 s apart, the second stronger.
        let store = Store::build(span(0.0, 10.0, |t, s| {
            pulse(2.0, 2.5, -4.0, t, s);
            pulse(3.5, 4.0, -8.0, t, s);
        }));
        assert_eq!(store.hard.len(), 1, "{:?}", store.hard);
        let h = store.hard[0];
        assert!((3.5..4.0).contains(&h.t) && (h.g + 8.0 / 9.80665).abs() < 0.01, "{h:?}");

        // Far apart: two. Opposite sign: its own marker.
        let store = Store::build(span(0.0, 20.0, |t, s| {
            pulse(2.0, 2.5, -8.0, t, s);
            pulse(10.0, 10.5, -8.0, t, s);
            pulse(11.0, 11.5, 5.0, t, s);
        }));
        let signs: Vec<f32> = store.hard.iter().map(|h| h.g.signum()).collect();
        assert_eq!(signs, [-1.0, -1.0, 1.0]);

        // A single jolt and normal driving are not hard moments.
        let store = Store::build(span(0.0, 10.0, |t, s| {
            pulse(2.0, 2.02, -20.0, t, s);
            pulse(5.0, 9.0, 2.5, t, s);
        }));
        assert!(store.hard.is_empty(), "{:?}", store.hard);
        // Smoothing doesn't reach across a gap.
        // Positive accel_y is braking: negative g.
        let mut r = span(0.0, 1.0, |_, s| s.accel_y = 8.0);
        r.extend(span(100.0, 101.0, |_, _| ()));
        let store = Store::build(r);
        assert!(store.at(0.5).unwrap().g < -0.5);
        assert_eq!(store.at(100.2).unwrap().g, 0.0);
        assert_eq!(store.hard.len(), 1);
    }

    #[test]
    fn downsampling_keeps_peaks_and_brake_blocks_and_gaps() {
        // An hour at 36 Hz into 100 columns, with one-sample events and a gap in the middle.
        let mut r = span(0.0, 1800.0, |_, s| s.speed_mps = 10.0);
        r.extend(span(2400.0, 3600.0, |_, s| s.speed_mps = 10.0));
        let spike = r.partition_point(|x| x.t < 100.0);
        r[spike].tel.speed_mps = 40.0;
        r[spike + 1].tel.accel_pct = 90.0;
        r[spike + 2].tel.steering_deg = -200.0;
        r[spike + 3].tel.brake = true;
        r[spike + 4].tel.steering_deg = 150.0;
        let store = Store::build(r);
        assert_eq!(store.speed_max, 40.0);
        let c = store.columns(3600.0, 100);
        assert_eq!(c.len(), 100);
        let hit = c[2].unwrap();
        assert_eq!((hit.speed, hit.gas, hit.brake, hit.steer_lo, hit.steer_hi), (40.0, 90.0, true, -200.0, 150.0));
        let other = c[1].unwrap();
        assert_eq!((other.speed, other.gas, other.brake), (10.0, 0.0, false));
        assert!(c[50..66].iter().all(Option::is_none), "the gap stays empty");
        assert!(c[49].is_some() && c[66].is_some());

        // Zoomed in past the sample spacing: no holes between samples, none invented past the data.
        let store = Store::build(span(10.0, 14.0, |_, s| s.speed_mps = 5.0));
        let c = store.columns(20.0, 2000);
        assert!(c[1000..1395].iter().all(Option::is_some));
        assert!(c[..999].iter().all(Option::is_none) && c[1401..].iter().all(Option::is_none));

        assert_eq!(Store::default().columns(10.0, 4), [None; 4]);
        assert_eq!(store.columns(0.0, 2), [None; 2]);
        assert!(store.columns(10.0, 0).is_empty());
    }

    #[test]
    fn autopilot_bands_split_on_state_and_gaps() {
        let mut r = span(0.0, 2.0, |_, s| s.autopilot = AP_TACC);
        r.extend(span(2.0, 4.0, |_, s| s.autopilot = AP_AUTOSTEER));
        r.extend(span(4.0, 6.0, |_, _| ()));
        r.extend(span(6.0, 7.0, |_, s| s.autopilot = AP_AUTOSTEER));
        r.extend(span(60.0, 61.0, |_, s| s.autopilot = AP_AUTOSTEER));
        let b = Store::build(r).bands;
        let states: Vec<(i32, f64)> = b.iter().map(|b| (b.state, b.t0.round())).collect();
        assert_eq!(states, [(AP_TACC, 0.0), (AP_AUTOSTEER, 2.0), (AP_AUTOSTEER, 6.0), (AP_AUTOSTEER, 60.0)]);
        assert!(b[0].t1 > 1.9 && b[0].t1 < 2.0);
    }

    #[test]
    fn the_segments_at_the_playhead_load_first() {
        let job = |start: f64, limit: f64| Job { start, limit, files: Default::default() };
        let order = |t: f64| {
            let mut jobs = vec![job(0.0, 60.0), job(60.0, 120.0), job(120.0, 180.0), job(180.0, f64::INFINITY)];
            nearest_first(&mut jobs, t);
            jobs.iter().map(|j| j.start).collect::<Vec<_>>()
        };
        assert_eq!(order(190.0), [180.0, 120.0, 60.0, 0.0], "a Saved event opens in its last segment");
        assert_eq!(order(0.0), [0.0, 60.0, 120.0, 180.0]);
        assert_eq!(order(125.0), [120.0, 60.0, 180.0, 0.0]);
        // Arriving out of order doesn't matter to the store.
        let store = Store::build(vec![rec(60.0, |_| ()), rec(0.0, |_| ())]);
        assert_eq!((store.records[0].t, store.records[1].t), (0.0, 60.0));
    }

    #[test]
    fn units() {
        assert!((speed_in(10.0, true) - 36.0).abs() < 1e-3);
        assert!((speed_in(10.0, false) - 22.369).abs() < 1e-3);
        assert_eq!(speed_in(-10.0, true), 36.0);
    }

    #[test]
    fn csv_has_the_event_time_and_the_sei_columns() {
        let store = Store::build(vec![rec(1.0, |s| s.speed_mps = 2.5), rec(2.5, |s| s.speed_mps = 3.5), rec(9.0, |_| ())]);
        let mut out = Vec::new();
        store.write_csv(&mut out, 1.0, 2.5).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("event_time_s,{}", sei::csv_header()));
        assert_eq!(lines[1], format!("1.000,{}", sei::csv_row(&store.records[0].tel)));
        assert_eq!(lines.len(), 3, "only records in range");
        assert_eq!(lines[2].split(',').count(), lines[0].split(',').count());
    }

    /// Writes a tiny clip; frames carry SEI with `speed` when it is given.
    fn clip(path: &Path, frames: usize, speed: Option<f32>) {
        fixture::encode_clip(path, 64, 48, 100_000, frames, |_, yuv| {
            yuv.resize(64 * 48 * 3 / 2, 128);
            speed.map(|v| sei::sei_nal(&sei::encode_metadata(&Telemetry { version: 1, speed_mps: v, ..Default::default() })))
        })
        .unwrap();
    }

    #[test]
    fn a_segment_without_sei_does_not_shift_the_next_and_other_cameras_can_stand_in() {
        let dir = std::env::temp_dir().join(format!("otc-telemetry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = |n: &str| dir.join(n);
        clip(&path("none.mp4"), 18, None);
        clip(&path("a.mp4"), 18, Some(1.0));
        clip(&path("b.mp4"), 18, Some(2.0));

        let cams = |front: &str, back: Option<&str>| {
            let mut f: [Option<PathBuf>; 6] = Default::default();
            f[0] = Some(path(front));
            f[1] = back.map(path);
            f
        };
        let jobs = vec![
            Job { start: 0.0, limit: 60.0, files: cams("none.mp4", None) },
            Job { start: 60.0, limit: 120.0, files: cams("a.mp4", None) },
            Job { start: 120.0, limit: 180.0, files: cams("none.mp4", Some("b.mp4")) },
            // Overlaps the segment after it, which owns the shared time.
            Job { start: 180.0, limit: 180.25, files: cams("a.mp4", None) },
            Job { start: 900.0, limit: f64::INFINITY, files: [const { None }; 6] },
        ];
        let mut seen = Vec::new();
        load(jobs, |s| {
            seen.push(s);
            true
        });
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(seen.len(), 5, "a store after every segment, even an empty one");
        assert!(seen[0].records.is_empty());
        let store = seen.pop().unwrap();
        assert_eq!(store.records.len(), 18 + 18 + 9);
        assert_eq!(speeds(&store, 0.0), None, "no SEI");
        assert_eq!(speeds(&store, 60.0), Some(1.0), "at its own start");
        assert_eq!(store.records[0].t, 60.0);
        assert_eq!(speeds(&store, 120.25), Some(2.0), "the back camera stands in");
        assert!(store.records.iter().all(|r| r.t < 180.25));
        assert_eq!(speeds(&store, 900.0), None);

        // Abandoned load: stops at once.
        let mut calls = 0;
        load((0..2).map(|_| Job { start: 0.0, limit: 1.0, files: Default::default() }).collect(), |_| {
            calls += 1;
            false
        });
        assert_eq!(calls, 1);
    }
}
