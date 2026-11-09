//! The master clock: one position on the event's wall-clock timeline, mapped to
//! (segment, seconds into that segment) for the camera decoders.

use crate::library::Event;
use crate::mp4;
use std::time::Instant;

pub const RATES: [f64; 5] = [0.25, 0.5, 1.0, 2.0, 4.0];
/// Tesla records at about 36 fps; used when a segment has no readable track.
const FRAME: f64 = 1.0 / 36.0;
/// Positions set by frame steps sit this far past the frame's pts so float rounding can't land
/// them on the previous frame.
const EPS: f64 = 1e-4;

#[derive(Debug, Clone, PartialEq)]
pub struct SegTime {
    /// Seconds from the event start (filename times).
    pub start: f64,
    /// From the track's stts; 0 when no camera of the segment could be read.
    pub dur: f64,
    /// Sorted presentation times (seconds into the segment) of the reference camera's frames.
    pub frames: Vec<f64>,
}

pub struct Player {
    pub segs: Vec<SegTime>,
    /// Wall clock of position 0, in `library::stamp` seconds.
    pub start: i64,
    pub rate: f64,
    pub playing: bool,
    /// Seconds from the event start. Never inside a gap between segments.
    pub position: f64,
    last: Option<Instant>,
}

impl Player {
    /// Reads each segment's front track (or the first camera that reads) for its length and frame
    /// times; only the moov is read.
    // shortcut: runs on the caller's thread, ~1 ms per segment; move to a thread if hour-long
    // RecentClips sets make opening stutter.
    pub fn open(ev: &Event) -> Player {
        let t0 = ev.segments[0].start;
        let segs = ev
            .segments
            .iter()
            .map(|s| {
                let track = s.files.iter().flatten().find_map(|p| mp4::read_track(p).ok());
                let (dur, frames) = track.map_or((0.0, Vec::new()), |t| {
                    let ts = t.timescale.max(1) as f64;
                    let mut f: Vec<f64> = t.samples.iter().map(|s| s.pts as f64 / ts).collect();
                    f.sort_by(f64::total_cmp);
                    (t.duration as f64 / ts, f)
                });
                SegTime { start: (s.start - t0) as f64, dur, frames }
            })
            .collect();
        Player::new(segs, t0)
    }

    pub fn new(segs: Vec<SegTime>, start: i64) -> Player {
        Player { segs, start, rate: 1.0, playing: false, position: 0.0, last: None }
    }

    /// End of the last segment.
    pub fn duration(&self) -> f64 {
        self.segs.iter().map(|s| s.start + s.dur).fold(0.0, f64::max)
    }

    /// Segment that starts at or before `t`. A later segment wins where two overlap.
    pub fn seg_at(&self, t: f64) -> usize {
        self.segs.partition_point(|s| s.start <= t).saturating_sub(1)
    }

    /// (segment, seconds into it), or None in a gap or outside the event.
    pub fn locate(&self, t: f64) -> Option<(usize, f64)> {
        let i = self.segs.partition_point(|s| s.start <= t).checked_sub(1)?;
        let local = t - self.segs[i].start;
        (local <= self.segs[i].dur).then_some((i, local))
    }

    /// Where the decoders should be now.
    pub fn target(&self) -> Option<(usize, f64)> {
        self.locate(self.position)
    }

    pub fn segment(&self) -> usize {
        self.seg_at(self.position)
    }

    pub fn wall_clock(&self) -> f64 {
        self.start as f64 + self.position
    }

    /// Clamps to the event and moves a time in a gap to the next segment's start (`forward`) or
    /// the previous one's end.
    fn snap(&self, t: f64, forward: bool) -> f64 {
        let end = self.duration();
        let t = t.clamp(0.0, end);
        if self.locate(t).is_some() {
            return t;
        }
        let i = self.seg_at(t);
        match self.segs.get(i + 1) {
            Some(next) if forward => next.start,
            _ if forward => end,
            _ => self.segs.get(i).map_or(0.0, |s| s.start + s.dur),
        }
    }

    /// Advances the clock while playing. Call once per UI frame.
    ///
    /// Playing into a gap jumps to the next segment (as the legacy app did): RecentClips gaps
    /// last as long as the car was off, often hours, and the timeline already shows them.
    pub fn tick(&mut self, now: Instant) {
        if !self.playing {
            self.last = None;
            return;
        }
        if let Some(l) = self.last {
            let t = self.position + now.saturating_duration_since(l).as_secs_f64() * self.rate;
            self.position = self.snap(t, true);
            if self.position >= self.duration() {
                self.playing = false;
            }
        }
        self.last = Some(now);
    }

    pub fn play(&mut self) {
        if self.position >= self.duration() {
            self.position = 0.0;
        }
        self.playing = true;
        self.last = None;
    }

    pub fn pause(&mut self) {
        self.playing = false;
    }

    pub fn toggle(&mut self) {
        if self.playing { self.pause() } else { self.play() }
    }

    /// Scrubber click or drag: a time in a gap goes to the next segment.
    pub fn seek(&mut self, t: f64) {
        self.position = self.snap(t, true);
    }

    /// ±5 s / ±30 s. Going back into a gap lands on the end of the segment before it.
    pub fn seek_by(&mut self, dt: f64) {
        self.position = self.snap(self.position + dt, dt >= 0.0);
    }

    pub fn home(&mut self) {
        self.position = 0.0;
    }

    pub fn end(&mut self) {
        self.position = self.duration();
    }

    /// Start of the next segment; no-op on the last.
    pub fn next_segment(&mut self) {
        if let Some(s) = self.segs.get(self.segment() + 1) {
            self.position = s.start;
        }
    }

    /// Start of the previous segment (legacy behaviour); no-op on the first.
    pub fn prev_segment(&mut self) {
        if let Some(i) = self.segment().checked_sub(1) {
            self.position = self.segs[i].start;
        }
    }

    /// One frame forward (`dir` > 0) or back, onto the reference camera's frame times; pauses.
    pub fn step_frame(&mut self, dir: i32) {
        self.pause();
        let Some((i, local)) = self.target() else { return };
        let f = &self.segs[i].frames;
        if f.is_empty() {
            self.position = self.snap(self.position + dir.signum() as f64 * FRAME, dir > 0);
            return;
        }
        let shown = f.partition_point(|&p| p <= local).saturating_sub(1) as i64;
        let next = shown + dir.signum() as i64;
        let at = |s: &SegTime, k: usize| s.start + s.frames.get(k).copied().unwrap_or(0.0) + EPS;
        self.position = if next < 0 {
            match i.checked_sub(1).map(|j| &self.segs[j]) {
                Some(p) if !p.frames.is_empty() => at(p, p.frames.len() - 1),
                _ => self.position,
            }
        } else if next as usize >= f.len() {
            match self.segs.get(i + 1) {
                Some(n) => at(n, 0),
                None => self.position,
            }
        } else {
            at(&self.segs[i], next as usize)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Segments at 0 and 20 s (20 s each, adjacent), then one at 100 s after an 60 s gap.
    fn player() -> Player {
        let seg = |start: f64, dur: f64| SegTime { start, dur, frames: (0..(dur * 4.0) as usize).map(|k| k as f64 * 0.25).collect() };
        Player::new(vec![seg(0.0, 20.0), seg(20.0, 20.0), seg(100.0, 10.0)], 1000)
    }

    #[test]
    fn locate_with_gaps_and_boundaries() {
        let p = player();
        assert_eq!(p.duration(), 110.0);
        assert_eq!(p.locate(0.0), Some((0, 0.0)));
        assert_eq!(p.locate(19.5), Some((0, 19.5)));
        // The later segment owns the shared boundary.
        assert_eq!(p.locate(20.0), Some((1, 0.0)));
        assert_eq!(p.locate(40.0), Some((1, 20.0)));
        assert_eq!(p.locate(40.5), None);
        assert_eq!(p.locate(100.0), Some((2, 0.0)));
        assert_eq!(p.locate(110.0), Some((2, 10.0)));
        assert_eq!(p.locate(110.1), None);
        assert_eq!(p.locate(-0.1), None);
        assert_eq!((p.seg_at(70.0), p.seg_at(-1.0), p.seg_at(500.0)), (1, 0, 2));
        // Overlap: the second segment takes over at its own start.
        let mut q = player();
        q.segs[0].dur = 21.0;
        assert_eq!(q.locate(20.5), Some((1, 0.5)));
    }

    #[test]
    fn seeks_snap_out_of_gaps() {
        let mut p = player();
        p.seek(70.0);
        assert_eq!(p.position, 100.0);
        p.seek_by(-5.0);
        assert_eq!(p.position, 40.0, "back into a gap: end of the previous segment");
        p.seek_by(5.0);
        assert_eq!(p.position, 100.0);
        p.seek_by(30.0);
        assert_eq!(p.position, 110.0, "clamped to the end");
        p.seek(-3.0);
        assert_eq!(p.position, 0.0);
        p.seek_by(-30.0);
        assert_eq!(p.position, 0.0);
        p.end();
        assert_eq!(p.position, 110.0);
        p.home();
        assert_eq!(p.position, 0.0);
    }

    #[test]
    fn segment_jumps() {
        let mut p = player();
        p.seek(25.0);
        p.next_segment();
        assert_eq!(p.position, 100.0);
        p.next_segment();
        assert_eq!(p.position, 100.0);
        p.prev_segment();
        assert_eq!(p.position, 20.0);
        p.prev_segment();
        p.prev_segment();
        assert_eq!(p.position, 0.0);
    }

    #[test]
    fn clock_advances_at_rate_and_jumps_gaps() {
        let mut p = player();
        let t0 = Instant::now();
        p.play();
        p.tick(t0);
        assert_eq!(p.position, 0.0, "the first tick only starts the clock");
        p.rate = 4.0;
        p.tick(t0 + Duration::from_millis(500));
        assert!((p.position - 2.0).abs() < 1e-9);
        p.seek(39.0);
        p.rate = 2.0;
        p.tick(t0 + Duration::from_millis(1500));
        assert_eq!(p.position, 100.0, "played into the gap: next segment");
        p.pause();
        p.tick(t0 + Duration::from_secs(10));
        assert_eq!(p.position, 100.0);
        // Resuming doesn't count the paused time.
        p.play();
        p.tick(t0 + Duration::from_secs(20));
        p.tick(t0 + Duration::from_secs(25));
        assert!((p.position - 110.0).abs() < 1e-9 && !p.playing, "stops at the end");
        p.play();
        assert_eq!(p.position, 0.0, "play at the end restarts");
        assert_eq!(p.wall_clock(), 1000.0);
    }

    #[test]
    fn frame_steps() {
        let mut p = player();
        p.play();
        p.step_frame(1);
        assert!(!p.playing);
        assert_eq!(p.target().map(|(s, l)| (s, (l * 4.0).floor())), Some((0, 1.0)));
        p.step_frame(-1);
        p.step_frame(-1);
        assert_eq!(p.target().map(|(s, l)| (s, (l * 4.0).floor())), Some((0, 0.0)), "no frame before the first");
        // Across an adjacent boundary and across a gap, both ways.
        p.seek(19.8);
        p.step_frame(1);
        assert_eq!(p.target().map(|(s, l)| (s, (l * 4.0).floor())), Some((1, 0.0)));
        p.step_frame(-1);
        assert_eq!(p.target().map(|(s, l)| (s, (l * 4.0).floor())), Some((0, 79.0)));
        p.seek(39.9);
        p.step_frame(1);
        assert_eq!(p.segment(), 2);
        p.step_frame(-1);
        assert_eq!(p.target().map(|(s, l)| (s, (l * 4.0).floor())), Some((1, 79.0)));
        // No frame times: fixed 1/36 s steps.
        let mut q = Player::new(vec![SegTime { start: 0.0, dur: 1.0, frames: vec![] }], 0);
        q.step_frame(1);
        assert!((q.position - FRAME).abs() < 1e-12);
    }
}
