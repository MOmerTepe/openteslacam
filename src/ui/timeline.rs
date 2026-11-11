//! The scrubber: time-of-day ticks, segment strip, lanes and playhead as one custom widget.

use super::events::{hms, kind_tag, reason_label};
use super::theme::{self, BG, HOVER, LINE, LINE2, TEXT, TEXT2, TEXT3};
use super::vehicle::g_text;
use crate::library::Event;
use crate::player::Player;
use crate::sei;
use crate::telemetry::{HardEvent, Store, speed_in};
use eframe::egui::{Align2, Color32, CursorIcon, Id, Mesh, Painter, Pos2, Rect, Role, Sense, Shape, Stroke, StrokeKind, Ui, WidgetInfo, pos2, vec2};
use std::collections::HashMap;

const GUTTER: f32 = 84.0;
const GUTTER_GAP: f32 = 10.0;
const TICKS: f32 = 14.0;
/// The row above the times for what is pinned to a moment (g-force tags, the mark, the car's
/// trigger), so none of it covers a time or a lane. It takes the place of the gap under the transport.
pub const TAGS: f32 = 12.0;
const SEGMENTS: f32 = 10.0;
const GAP: f32 = 4.0;
/// (name, hint, height, has a center line).
const LANES: [(&str, &str, f32, bool); 4] = [
    ("speed", "", 34.0, false),
    ("pedals", "gas ↑ brake ↓", 28.0, true),
    ("steering", "left ↑ right ↓", 28.0, true),
    ("autopilot", "", 14.0, false),
];

/// Flashes closer than this count as one place on the bar.
const CHEVRON_PX: f32 = 6.0;
/// Height of a blinker band on the steering lane's edge.
const BLINK_H: f32 = 2.0;
/// Between a hard moment and its tag, and between tags.
const TAG_GAP: f32 = 6.0;
/// The steering lane's scale in degrees: past this the trace sits at the lane's edge.
const STEER_MAX: f32 = 180.0;

/// The mockup's rgba(230,230,225,0.07).
pub(super) const TRIM_FILL: Color32 = Color32::from_rgba_unmultiplied_const(230, 230, 225, 18);

/// What the scrubber shows besides the clock.
pub struct View<'a> {
    pub store: Option<&'a Store>,
    /// Keep between frames; set to None when `store` changes.
    pub cache: &'a mut Option<LaneCache>,
    /// Seconds from the event start, and the text.
    pub flag: Option<(f64, String)>,
    pub metric: bool,
    /// In and Out, in seconds from the event start.
    pub trim: Option<(f64, f64)>,
    /// Where the user says it happened, in seconds from the event start; a point has both ends equal.
    pub mark: Option<(f64, f64)>,
}

/// The lanes as shapes, built once per event and lane size.
pub struct LaneCache {
    bar: Rect,
    shapes: Vec<Shape>,
    /// Hard brake / accel tags that fit, in the tick row; their lines and the others' are among `shapes`.
    tags: Vec<(Rect, String)>,
}

/// Height of the whole widget.
pub fn height(lanes: bool) -> f32 {
    let lanes_h: f32 = if lanes { LANES.iter().map(|l| GAP + l.2).sum() } else { 0.0 };
    TAGS + TICKS + GAP + SEGMENTS + lanes_h
}

/// Timeline seconds → screen x.
pub fn x_at(left: f32, width: f32, dur: f64, t: f64) -> f32 {
    if dur <= 0.0 {
        return left;
    }
    left + (t / dur).clamp(0.0, 1.0) as f32 * width
}

/// Screen x → timeline seconds.
pub fn t_at(left: f32, width: f32, dur: f64, x: f32) -> f64 {
    if width <= 0.0 {
        return 0.0;
    }
    ((x - left) / width).clamp(0.0, 1.0) as f64 * dur
}

/// Labels at round times of day, at least `MIN_PX` apart: (timeline seconds, label).
pub fn ticks(start: i64, dur: f64, width: f32) -> Vec<(f64, String)> {
    const MIN_PX: f64 = 64.0;
    const STEPS: [i64; 16] = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 21600, 43200];
    if dur <= 0.0 || width <= 0.0 {
        return Vec::new();
    }
    let per_px = dur / width as f64;
    let step = STEPS.into_iter().find(|&s| s as f64 / per_px >= MIN_PX).unwrap_or(((MIN_PX * per_px / 86400.0).ceil() as i64).max(1) * 86400);
    let mut out = Vec::new();
    let mut wall = (start + step - 1).div_euclid(step) * step;
    while (wall - start) as f64 <= dur {
        let s = hms(wall);
        out.push(((wall - start) as f64, if step >= 60 { s[..5].to_owned() } else { s }));
        wall += step;
    }
    out
}

/// The event marker: event.json's time and reason, on the timeline of a player starting at `start`.
pub fn event_flag(ev: &Event, start: i64) -> Option<(f64, String)> {
    let info = ev.info.as_ref()?;
    let ts = info.timestamp?;
    let reason = reason_label(&info.reason).to_lowercase();
    let what = if reason.is_empty() { kind_tag(ev.kind) } else { reason.as_str() };
    Some(((ts - start) as f64, format!("{what} {}", hms(ts))))
}

/// y of a steering angle around the lane's center line: left (negative) up, right down.
pub fn steer_y(center: f32, half: f32, deg: f32) -> f32 {
    center + deg.clamp(-STEER_MAX, STEER_MAX) / STEER_MAX * half
}

/// One chevron per `px` of bar for each side (both sorted), at the first flash of either side there.
/// The shared buckets keep hazards paired whatever flashed just before them, and sparse flashes keep
/// their exact place.
fn thin(left: &[f32], right: &[f32], px: f32) -> [Vec<f32>; 2] {
    let bucket = |x: f32| (x / px).floor() as i64;
    let mut first: HashMap<i64, f32> = HashMap::new();
    for &x in left.iter().chain(right) {
        first.entry(bucket(x)).and_modify(|f| *f = f.min(x)).or_insert(x);
    }
    [left, right].map(|side| {
        let mut out: Vec<f32> = side.iter().map(|&x| first[&bucket(x)]).collect();
        out.dedup();
        out
    })
}

/// Left of its moment as in the mockup, unless that leaves the bar.
fn tag_left(x: f32, w: f32, bar_left: f32) -> f32 {
    if x - TAG_GAP - w < bar_left { x + TAG_GAP } else { x - TAG_GAP - w }
}

/// Which of the markers at `xs` get a tag `w` px wide: strongest first, where they end up not
/// overlapping.
fn pick_tags(hard: &[HardEvent], xs: &[f32], w: f32, bar_left: f32) -> Vec<bool> {
    let mut order: Vec<usize> = (0..hard.len()).collect();
    order.sort_by(|&a, &b| hard[b].g.abs().total_cmp(&hard[a].g.abs()));
    let mut keep = vec![false; hard.len()];
    let mut kept: Vec<f32> = Vec::new();
    for i in order {
        let left = tag_left(xs[i], w, bar_left);
        if kept.iter().all(|k| (k - left).abs() >= w + TAG_GAP) {
            keep[i] = true;
            kept.push(left);
        }
    }
    keep
}

fn lane_rects(bar: Rect, mut y: f32) -> [Rect; 4] {
    LANES.map(|l| {
        let r = Rect::from_min_size(pos2(bar.min.x, y + GAP), vec2(bar.width(), l.2));
        y = r.max.y;
        r
    })
}

/// Sorted flash positions as (from, to) stretches: flashes no further than `gap` apart are one signal.
fn runs(xs: &[f32], gap: f32) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = Vec::new();
    for &x in xs {
        match out.last_mut().filter(|r| x - r.1 <= gap) {
            Some(r) => r.1 = x,
            None => out.push((x, x)),
        }
    }
    out
}

/// Everything per-pixel of the lanes, from the store's downsampled columns (design rule 4).
fn build_lanes(p: &Painter, store: &Store, dur: f64, bar: Rect, [speed, pedals, steering, _]: [Rect; 4]) -> LaneCache {
    let n = bar.width().round().max(0.0) as usize;
    let cols = store.columns(dur, n);
    let col_w = bar.width() / n.max(1) as f32;
    let (ped_c, steer_c) = (pedals.center().y, steering.center().y);
    let (mut speed_fill, mut gas_fill, mut brake_fill) = (Mesh::default(), Mesh::default(), Mesh::default());
    let (mut speed_lines, mut gas_lines, mut steer_lines) = (Vec::new(), Vec::new(), Vec::new());
    let scale = store.speed_max.max(0.1);
    let mut c = 0;
    // A run is an unbroken stretch of data: lines must not cross a gap.
    for run in cols.chunk_by(|a, b| a.is_some() == b.is_some()) {
        let first = c;
        c += run.len();
        if run[0].is_none() {
            continue;
        }
        let (mut sp, mut gas, mut st) = (Vec::with_capacity(run.len()), Vec::with_capacity(run.len()), Vec::with_capacity(2 * run.len()));
        for (k, col) in run.iter().flatten().enumerate() {
            let x0 = bar.min.x + (first + k) as f32 * col_w;
            let (x1, mid) = (x0 + col_w, x0 + col_w / 2.0);
            let y = speed.max.y - col.speed / scale * (speed.height() - 2.0);
            speed_fill.add_colored_rect(Rect::from_x_y_ranges(x0..=x1, y..=speed.max.y), Color32::from_rgba_unmultiplied(230, 230, 225, 31));
            sp.push(pos2(mid, y));
            let y = ped_c - col.gas / 100.0 * (pedals.height() / 2.0 - 1.0);
            gas_fill.add_colored_rect(Rect::from_x_y_ranges(x0..=x1, y..=ped_c), Color32::from_rgba_unmultiplied(230, 230, 225, 56));
            gas.push(pos2(mid, y));
            if col.brake {
                brake_fill.add_colored_rect(Rect::from_x_y_ranges(x0..=x1, ped_c..=pedals.max.y), theme::RED);
            }
            // Both extremes of the column, nearest the previous point first, so peaks stay and
            // falling edges don't saw.
            let half = steering.height() / 2.0 - 1.0;
            let (hi, lo) = (steer_y(steer_c, half, col.steer_hi), steer_y(steer_c, half, col.steer_lo));
            let down = st.last().is_none_or(|l: &Pos2| (l.y - hi).abs() <= (l.y - lo).abs());
            let (a, b) = if down { (hi, lo) } else { (lo, hi) };
            st.push(pos2(mid, a));
            if b != a {
                st.push(pos2(mid, b));
            }
        }
        speed_lines.push(Shape::line(sp, Stroke::new(1.25, TEXT)));
        gas_lines.push(Shape::line(gas, Stroke::new(1.0, TEXT2)));
        steer_lines.push(Shape::line(st, Stroke::new(1.25, TEXT)));
    }

    let x = |t: f64| x_at(bar.min.x, bar.width(), dur, t);
    let mut shapes: Vec<Shape> = vec![speed_fill.into(), gas_fill.into()];
    shapes.extend(speed_lines);
    shapes.extend(gas_lines);
    shapes.push(brake_fill.into());
    shapes.push(Shape::line_segment([pos2(bar.min.x, ped_c), pos2(bar.max.x, ped_c)], theme::line(LINE2)));
    shapes.extend(steer_lines);
    // A signal is one amber band on the lane's edge, left on top and right below like the steering
    // line itself, and clear of it. One mark per flash was unreadable over the line.
    let screen_xs = |ts: &[f64]| ts.iter().map(|&t| x(t)).collect::<Vec<f32>>();
    let [left, right] = thin(&screen_xs(&store.left), &screen_xs(&store.right), CHEVRON_PX);
    // Flashes come about every 0.7 s: 1.5 s apart is still the same signal.
    let gap = (1.5 * bar.width() / dur.max(1e-3) as f32).max(2.0 * CHEVRON_PX);
    let mut blink = Mesh::default();
    for (xs, y) in [(left, steering.min.y), (right, steering.max.y - BLINK_H)] {
        for (a, b) in runs(&xs, gap) {
            blink.add_colored_rect(Rect::from_x_y_ranges(a..=(b + 3.0).min(bar.max.x), y..=y + BLINK_H), theme::YELLOW);
        }
    }
    shapes.push(blink.into());

    // The sign says which: minus is braking.
    let label = |h: &HardEvent| g_text(h.g);
    let xs: Vec<f32> = store.hard.iter().map(|h| x(h.t)).collect();
    let tag_w = store.hard.first().map_or(0.0, |h| p.layout_no_wrap(label(h), theme::mono(10.0), TEXT).size().x + 10.0);
    let mut tags = Vec::new();
    // A tag sits in the tag row above the times, with a line down to its moment on the speed lane.
    // One that found no room is the line in the lane alone.
    let top = speed.min.y - GAP - SEGMENTS - GAP - TICKS - TAGS;
    for ((h, &hx), tagged) in store.hard.iter().zip(&xs).zip(pick_tags(&store.hard, &xs, tag_w, bar.min.x)) {
        let from = if tagged { top } else { speed.min.y };
        shapes.push(Shape::line_segment([pos2(hx, from), pos2(hx, speed.max.y)], theme::line(TEXT)));
        if tagged {
            // Touching its line, so the two read as one flag.
            let left = if hx - tag_w < bar.min.x { hx } else { hx - tag_w };
            tags.push((Rect::from_min_size(pos2(left, top), vec2(tag_w, TAGS - 1.0)), label(h)));
        }
    }
    LaneCache { bar, shapes, tags }
}

/// Draws into `rect` (`height(lanes)` tall) and seeks on click or drag. True while the pointer holds
/// the scrubber: the caller decodes keyframes only meanwhile. The lanes toggle sits in the gutter.
pub fn show(ui: &Ui, rect: Rect, player: Option<&mut Player>, lanes_open: &mut bool, view: View) -> bool {
    // Everything below is laid out under the tag row; only the tags and their lines reach into it.
    let (full, rect) = (rect, rect.with_min_y(rect.min.y + TAGS));
    let lanes = *lanes_open;
    let bar = rect.with_min_x(rect.min.x + GUTTER + GUTTER_GAP);
    let segs_top = rect.min.y + TICKS + GAP;
    let segs = Rect::from_min_size(pos2(bar.min.x, segs_top), vec2(bar.width(), SEGMENTS));

    let p = ui.painter_at(full);
    let gutter = |y: f32, text: &str, size: f32, color: Color32| {
        p.text(pos2(bar.min.x - GUTTER_GAP, y), Align2::RIGHT_CENTER, text, theme::mono(size), color);
    };
    let label = if lanes { "hide lanes" } else { "show lanes" };
    let toggle_w = p.layout_no_wrap(label.to_owned(), theme::mono(10.0), TEXT).size().x;
    let toggle = Rect::from_min_size(pos2(bar.min.x - GUTTER_GAP - toggle_w, rect.min.y), vec2(toggle_w, TICKS));
    let r = ui.interact(toggle, Id::unique("lanes-toggle"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    r.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    gutter(toggle.center().y, label, 10.0, TEXT);
    p.hline(toggle.x_range(), toggle.max.y - 0.5, theme::line(LINE2));
    if r.clicked() {
        *lanes_open = !lanes;
    }

    gutter(segs.center().y, "segments", 10.0, TEXT3);
    let lane_r = lane_rects(bar, segs.max.y);
    let store = view.store.filter(|s| !s.records.is_empty());
    let speed_hint = store.map_or(String::new(), |s| format!("0–{:.0} {}", speed_in(s.speed_max, view.metric).ceil(), if view.metric { "km/h" } else { "mph" }));
    if lanes {
        for (lane, (name, hint, _, center_line)) in lane_r.into_iter().zip(LANES) {
            let hint = if name == "speed" { speed_hint.as_str() } else { hint };
            p.rect_filled(lane, 0.0, HOVER);
            if center_line {
                p.hline(lane.x_range(), lane.center().y, theme::line(LINE2));
            }
            if hint.is_empty() {
                gutter(lane.center().y, name, 10.0, TEXT3);
            } else {
                gutter(lane.center().y - 6.0, name, 10.0, TEXT3);
                gutter(lane.center().y + 6.0, hint, 9.0, theme::OFF);
            }
        }
    }

    let Some(player) = player else { return false };
    let dur = player.duration();
    let x = |t: f64| x_at(bar.min.x, bar.width(), dur, t);

    for (t, label) in ticks(player.start, dur, bar.width()) {
        p.text(pos2(x(t), rect.min.y + TICKS / 2.0), Align2::CENTER_CENTER, label, theme::mono(10.0), TEXT3);
    }
    let current = player.segment();
    for (i, s) in player.segs.iter().enumerate() {
        let color = match i.cmp(&current) {
            std::cmp::Ordering::Less => LINE2,
            std::cmp::Ordering::Equal => TEXT,
            std::cmp::Ordering::Greater => LINE,
        };
        // 2 px off the end keeps touching segments apart; time gaps show at their real width.
        let (x0, x1) = (x(s.start), x(s.start + s.dur));
        let block = segs.with_min_x(x0).with_max_x((x1 - 2.0).max(x0 + 1.0));
        p.rect_filled(block, 0.0, color);
    }
    let lane_store = store.filter(|_| lanes);
    if let Some(s) = lane_store {
        view.cache.take_if(|c| c.bar != bar);
        p.extend(view.cache.get_or_insert_with(|| build_lanes(&p, s, dur, bar, lane_r)).shapes.iter().cloned());
        for b in &s.bands {
            let x0 = x(b.t0);
            let band = lane_r[3].with_min_x(x0).with_max_x(x(b.t1).max(x0 + 1.0));
            p.rect_filled(band, 0.0, LINE2);
            let clipped = p.with_clip_rect(p.clip_rect().intersect(band));
            clipped.text(pos2(band.min.x + 4.0, band.center().y), Align2::LEFT_CENTER, sei::ap_name(b.state), theme::mono(9.5), TEXT);
        }
    }
    if let Some((a, b)) = view.trim {
        let (x0, x1) = (x(a), x(b));
        let range = Rect::from_x_y_ranges(x0..=x1, segs_top..=rect.max.y);
        p.rect_filled(range, 0.0, TRIM_FILL);
        p.vline(x0 + 0.5, range.y_range(), theme::line(TEXT3));
        p.vline(x1 - 0.5, range.y_range(), theme::line(TEXT3));
    }
    // The mark: a bright cap over its span with solid ends, and its word beside it.
    if let Some((a, b)) = view.mark.filter(|m| m.0 <= dur && m.1 >= 0.0) {
        let (x0, x1) = (x(a), x(b).max(x(a) + 2.0));
        // Yellow, like a highlighter: the user's own mark, apart from the car's trigger flag.
        p.rect_filled(Rect::from_x_y_ranges(x0..=x1, segs_top - 3.0..=segs_top - 1.0), 0.0, theme::YELLOW);
        p.vline(x0 + 0.5, segs_top - 3.0..=rect.max.y, theme::line(TEXT));
        p.vline(x1 - 0.5, segs_top - 3.0..=rect.max.y, theme::line(TEXT));
        let g = p.layout_no_wrap("mark".to_owned(), theme::medium(10.0), theme::YELLOW_INK);
        let w = g.size().x + 10.0;
        let left = if x1 + 4.0 + w > bar.max.x { x0 - 4.0 - w } else { x1 + 4.0 };
        let r = Rect::from_min_size(pos2(left, full.min.y), vec2(w, TAGS - 1.0));
        p.rect_filled(r, 0.0, theme::YELLOW);
        p.galley(pos2(r.min.x + 5.0, r.center().y - g.size().y / 2.0), g, theme::YELLOW_INK);
    }
    if lane_store.is_some()
        && let Some(cache) = view.cache
    {
        for (r, label) in &cache.tags {
            p.rect_filled(*r, 0.0, BG);
            p.rect_stroke(*r, 0.0, theme::line(TEXT), StrokeKind::Inside);
            p.text(r.center(), Align2::CENTER_CENTER, label, theme::mono(10.0), TEXT);
        }
    }
    if let Some((t, label)) = view.flag.filter(|f| (0.0..=dur).contains(&f.0)) {
        let fx = x(t);
        p.extend(Shape::dashed_line(&[pos2(fx, full.min.y), pos2(fx, rect.max.y)], theme::line(TEXT), 3.0, 3.0));
        let g = p.layout_no_wrap(label, theme::medium(10.0), BG);
        let w = g.size().x + 10.0;
        let left = if fx - 4.0 - w < bar.min.x { fx + 4.0 } else { fx - 4.0 - w };
        let r = Rect::from_min_size(pos2(left, full.min.y), vec2(w, TAGS - 1.0));
        p.rect_filled(r, 0.0, TEXT);
        p.galley(pos2(r.min.x + 5.0, r.center().y - g.size().y / 2.0), g, BG);
    }

    let r = ui.interact(bar, Id::unique("scrubber"), Sense::click_and_drag()).on_hover_cursor(CursorIcon::PointingHand);
    r.widget_info(|| WidgetInfo::labeled(Role::Slider, true, "Timeline position"));
    let holding = r.is_pointer_button_down_on();
    if (holding || r.clicked())
        && let Some(pos) = r.interact_pointer_pos()
    {
        player.seek(t_at(bar.min.x, bar.width(), dur, pos.x));
    }

    let head = x(player.position);
    let white = Color32::WHITE;
    p.rect_filled(Rect::from_min_max(pos2(head - 1.0, rect.min.y + TICKS), pos2(head + 1.0, rect.max.y)), 0.0, white);
    let handle = Rect::from_center_size(pos2(head, segs.center().y), vec2(10.0, 16.0));
    p.rect_filled(handle.expand(2.0), 0.0, BG);
    p.rect_filled(handle, 0.0, white);
    holding
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{EventInfo, Kind, stamp};
    use eframe::egui::{Context, Event as Input, Modifiers, PointerButton, RawInput};
    use std::path::PathBuf;

    #[test]
    fn time_and_pixels_map_both_ways() {
        assert_eq!(x_at(100.0, 200.0, 50.0, 25.0), 200.0);
        assert_eq!(x_at(100.0, 200.0, 50.0, -1.0), 100.0);
        assert_eq!(x_at(100.0, 200.0, 50.0, 51.0), 300.0);
        assert_eq!(x_at(100.0, 200.0, 0.0, 5.0), 100.0, "no duration yet");
        assert_eq!(t_at(100.0, 200.0, 50.0, 200.0), 25.0);
        assert_eq!(t_at(100.0, 200.0, 50.0, 0.0), 0.0);
        assert_eq!(t_at(100.0, 200.0, 50.0, 900.0), 50.0);
        assert_eq!(t_at(100.0, 0.0, 50.0, 100.0), 0.0);
    }

    #[test]
    fn ticks_fall_on_round_times_and_keep_their_distance() {
        let start = stamp("2025-11-02_18-41-02").unwrap();
        let t = ticks(start, 658.0, 1000.0);
        let labels: Vec<_> = t.iter().map(|x| x.1.as_str()).collect();
        assert_eq!(labels[..3], ["18:42", "18:43", "18:44"]);
        assert_eq!(t[0].0, 58.0);
        assert_eq!(t[1].0 - t[0].0, 60.0);
        // Short clips get seconds in the label.
        let t = ticks(start, 20.0, 1000.0);
        assert_eq!(t[0].1, "18:41:02");
        assert_eq!(t[1].1, "18:41:04");
        // Spacing holds at any zoom, and hours of RecentClips don't produce hundreds of labels.
        for (dur, w) in [(5.0, 900.0), (3600.0, 900.0), (86400.0, 900.0), (20.0 * 86400.0, 300.0)] {
            let t = ticks(start, dur, w);
            assert!(!t.is_empty() && t.len() <= (w / 64.0) as usize + 1, "{dur} {w}: {}", t.len());
            for pair in t.windows(2) {
                assert!((pair[1].0 - pair[0].0) / dur * w as f64 >= 63.9);
            }
        }
        assert!(ticks(start, 0.0, 100.0).is_empty());
        assert!(ticks(start, 10.0, 0.0).is_empty());
    }

    #[test]
    fn the_gutter_toggle_flips_the_lanes_without_a_player() {
        let ctx = Context::default();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, height(true)));
        let mut open = true;
        let pos = pos2(GUTTER - 5.0, TICKS / 2.0);
        let press = |pressed| Input::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        for events in [vec![Input::PointerMoved(pos)], vec![press(true)], vec![press(false)], vec![]] {
            let input = RawInput { screen_rect: Some(rect), events, ..Default::default() };
            ctx.run_ui(input, |ui| {
                show(ui, rect, None, &mut open, View { store: None, cache: &mut None, flag: None, metric: true, trim: None, mark: None });
            })
            .textures_delta
            .clear();
        }
        assert!(!open);
    }

    #[test]
    fn steering_goes_up_for_left_and_stops_at_the_edge() {
        // Real cars steer negative into a left turn; the mockup draws left up. Screen y grows down.
        assert_eq!(steer_y(14.0, 13.0, -90.0), 7.5);
        assert_eq!(steer_y(14.0, 13.0, 180.0), 27.0, "positive is right: down");
        assert_eq!(steer_y(14.0, 13.0, -450.0), 1.0);
        assert_eq!(steer_y(14.0, 13.0, 0.0), 14.0);
    }

    #[test]
    fn chevrons_thin_out_on_long_events() {
        let [left, right] = thin(&[0.0, 1.0, 5.9, 6.0, 20.0, 21.0], &[], 6.0);
        assert_eq!(left, [0.0, 6.0, 20.0]);
        assert!(right.is_empty());
        // Sparse flashes stay where they are: an even rhythm stays even.
        let sparse = [3.0, 13.7, 24.4, 35.1];
        assert_eq!(thin(&sparse, &[], 6.0)[0], sparse);
    }

    #[test]
    fn flashes_join_into_one_band_per_signal() {
        assert_eq!(runs(&[10.0, 16.0, 22.0, 80.0, 86.0, 200.0], 12.0), [(10.0, 22.0), (80.0, 86.0), (200.0, 200.0)]);
        assert!(runs(&[], 12.0).is_empty());
    }

    #[test]
    fn hazards_stay_paired_after_a_blinker() {
        // An hour on the bar, 3.6 s per px: left signal up to t = 601, hazards from 603 to 615.
        let px = |t: f64| (t / 3.6) as f32;
        let flashes = |from: f64, to: f64| (0..).map(move |k| from + f64::from(k) * 2.0 / 3.0).take_while(move |t| *t <= to);
        let hazards: Vec<f32> = flashes(603.0, 615.0).map(px).collect();
        let left: Vec<f32> = flashes(500.0, 601.0).map(px).chain(hazards.iter().copied()).collect();
        let [l, r] = thin(&left, &hazards, CHEVRON_PX);
        assert!(!r.is_empty());
        assert!(r.iter().all(|x| l.contains(x)), "left {l:?}, right {r:?}");
        assert!(l.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn tags_go_to_the_strongest_and_never_overlap() {
        let h = |g: f32| HardEvent { t: 0.0, g };
        let hard = [h(-0.4), h(-0.9), h(0.5), h(-0.35)];
        assert_eq!(pick_tags(&hard, &[100.0, 150.0, 400.0, 600.0], 114.0, 0.0), [false, true, true, true]);
        assert!(pick_tags(&[], &[], 114.0, 0.0).is_empty());
    }

    #[test]
    fn a_tag_flipped_at_the_bar_edge_still_counts_where_it_ends_up() {
        let h = |g: f32| HardEvent { t: 0.0, g };
        // Bar at 100: the first flips right onto 126..244, exactly where the second sits.
        assert_eq!(tag_left(120.0, 118.0, 100.0), 126.0);
        assert_eq!(tag_left(250.0, 118.0, 100.0), 126.0);
        assert_eq!(pick_tags(&[h(-0.9), h(-0.5)], &[120.0, 250.0], 118.0, 100.0), [true, false]);
        assert_eq!(pick_tags(&[h(-0.5), h(-0.9)], &[120.0, 250.0], 118.0, 100.0), [false, true]);
        // Further along there's room for both.
        assert_eq!(pick_tags(&[h(-0.9), h(-0.5)], &[120.0, 400.0], 118.0, 100.0), [true, true]);
    }

    #[test]
    fn the_event_flag_has_the_time_and_reason() {
        let start = stamp("2025-11-02_18-41-02").unwrap();
        let info = |reason: &str| EventInfo {
            timestamp: stamp("2025-11-02_18-51-07"),
            reason: reason.to_owned(),
            city: String::new(),
            street: String::new(),
            est_lat: None,
            est_lon: None,
            camera: String::new(),
        };
        let event = |info| Event { kind: Kind::Saved, name: String::new(), dir: PathBuf::new(), segments: Vec::new(), info, thumb: None, encrypted: false };
        assert_eq!(event_flag(&event(Some(info("user_interaction_honk"))), start), Some((605.0, "honk 18:51:07".to_owned())));
        assert_eq!(event_flag(&event(Some(info(""))), start), Some((605.0, "saved 18:51:07".to_owned())));
        assert_eq!(event_flag(&event(None), start), None);
    }

    #[test]
    fn height_counts_lanes_only_when_open() {
        assert_eq!(height(false), 40.0);
        assert_eq!(height(true), 40.0 + 4.0 * 4.0 + 34.0 + 28.0 + 28.0 + 14.0);
    }
}
