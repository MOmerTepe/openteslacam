//! The scrubber: time-of-day ticks, segment strip, lanes and playhead as one custom widget.

use super::events::hms;
use super::theme::{self, BG, HOVER, LINE, LINE2, TEXT, TEXT3};
use crate::player::Player;
use eframe::egui::{Align2, Color32, CursorIcon, Id, Rect, Role, Sense, Ui, WidgetInfo, pos2, vec2};

const GUTTER: f32 = 84.0;
const GUTTER_GAP: f32 = 10.0;
const TICKS: f32 = 14.0;
const SEGMENTS: f32 = 10.0;
const GAP: f32 = 4.0;
/// (name, hint, height, has a center line).
const LANES: [(&str, &str, f32, bool); 4] = [
    ("speed", "", 34.0, false),
    ("pedals", "gas ↑ brake ↓", 28.0, true),
    ("steering", "left ↑ right ↓", 28.0, true),
    ("autopilot", "", 14.0, false),
];

/// Height of the whole widget.
pub fn height(lanes: bool) -> f32 {
    let lanes_h: f32 = if lanes { LANES.iter().map(|l| GAP + l.2).sum() } else { 0.0 };
    TICKS + GAP + SEGMENTS + lanes_h
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

/// Draws into `rect` (`height(lanes)` tall) and seeks on click or drag. True while the pointer holds
/// the scrubber: the caller decodes keyframes only meanwhile. The lanes toggle sits in the gutter.
pub fn show(ui: &Ui, rect: Rect, player: Option<&mut Player>, lanes_open: &mut bool) -> bool {
    let lanes = *lanes_open;
    let bar = rect.with_min_x(rect.min.x + GUTTER + GUTTER_GAP);
    let segs_top = rect.min.y + TICKS + GAP;
    let segs = Rect::from_min_size(pos2(bar.min.x, segs_top), vec2(bar.width(), SEGMENTS));

    let p = ui.painter_at(rect);
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
    let mut y = segs.max.y;
    if lanes {
        for (name, hint, h, center_line) in LANES {
            let lane = Rect::from_min_size(pos2(bar.min.x, y + GAP), vec2(bar.width(), h));
            y = lane.max.y;
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
            // Phase 2: `lane` is where this signal's precomputed shapes go (design rule 4).
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
    // Phase 2: event marker (dashed line + "saved 18:51:07" flag) and hard-brake markers go here.
    // Phase 4: the trim range (translucent rect from x(in) to x(out), from segs_top down) goes here.

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
    use crate::library::stamp;
    use eframe::egui::{Context, Event, Modifiers, PointerButton, RawInput};

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
        let press = |pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        for events in [vec![Event::PointerMoved(pos)], vec![press(true)], vec![press(false)], vec![]] {
            let input = RawInput { screen_rect: Some(rect), events, ..Default::default() };
            ctx.run_ui(input, |ui| {
                show(ui, rect, None, &mut open);
            })
            .textures_delta
            .clear();
        }
        assert!(!open);
    }

    #[test]
    fn height_counts_lanes_only_when_open() {
        assert_eq!(height(false), 28.0);
        assert_eq!(height(true), 28.0 + 4.0 * 4.0 + 34.0 + 28.0 + 28.0 + 14.0);
    }
}
