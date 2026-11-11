//! Vehicle strip at the top of the bottom panel: the telemetry at the playhead, per design rule 4.

use super::theme::{self, BG, LINE, LINE2, OFF, RED, RED_INK, TEXT, TEXT3, YELLOW};
use crate::sei::{self, AP_NONE, Telemetry};
use crate::telemetry::{HARD_G, Record, speed_in};
use eframe::egui::emath::Rot2;
use eframe::egui::{Align2, Color32, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};

pub const HEIGHT: f32 = 50.0;
/// Display order; the proto's enum order is P D R N, so go through `gear_slot`, never the raw value.
const GEARS: [&str; 4] = ["P", "R", "N", "D"];

/// (label, share of the width, placeholder value). Shares fit the widest value at the minimum window
/// width, and cells don't resize with their values.
const CELLS: [(&str, f32, &str); 9] = [
    ("speed", 1.6, ""),
    ("gear", 1.1, "P R N D"),
    ("brake", 0.7, "–"),
    ("accelerator", 1.3, "–"),
    ("steering", 1.4, "–"),
    ("blinkers", 0.8, "–"),
    ("hazards", 0.7, "–"),
    ("autopilot", 1.1, "–"),
    ("g-force", 1.0, "–"),
];

fn cell_rects(rect: Rect) -> [Rect; 9] {
    let total: f32 = CELLS.iter().map(|c| c.1).sum();
    let mut x = rect.min.x;
    CELLS.map(|c| {
        let w = rect.width() * c.1 / total;
        let r = rect.with_min_x(x).with_max_x(x + w);
        x += w;
        r
    })
}

/// Position in the P R N D row.
pub fn gear_slot(gear: i32) -> Option<usize> {
    GEARS.iter().position(|g| *g == sei::gear_letter(gear))
}

/// "16° right". Positive is right (see `telemetry`); straight ahead names no side.
pub fn steer_text(deg: f32) -> String {
    let a = deg.abs().round();
    match deg {
        _ if a == 0.0 => "0°".to_owned(),
        d if d > 0.0 => format!("{a:.0}° right"),
        _ => format!("{a:.0}° left"),
    }
}

/// "−0.52 g" (a real minus), "+0.00 g".
pub fn g_text(g: f32) -> String {
    let r = (g * 100.0).round() / 100.0;
    format!("{}{:.2} g", if r < 0.0 { '−' } else { '+' }, r.abs())
}

fn ap_text(state: i32) -> &'static str {
    if state == AP_NONE { "manual" } else { sei::ap_name(state) }
}

/// Filled with `fill` (lettered in `ink`) when `on`, else a thin dim box. Both 18 px tall.
fn chip(p: &Painter, left_center: Pos2, text: &str, on: bool, (fill, ink): (Color32, Color32)) {
    let (font, fg) = if on { (theme::medium(12.0), ink) } else { (theme::mono(12.0), TEXT3) };
    let g = p.layout_no_wrap(text.to_owned(), font, fg);
    let r = Rect::from_min_size(pos2(left_center.x, left_center.y - 9.0), vec2(g.size().x + 12.0, 18.0));
    if on {
        p.rect_filled(r, 0.0, fill);
    } else {
        p.rect_stroke(r, 0.0, theme::line(LINE2), StrokeKind::Inside);
    }
    p.galley(pos2(r.min.x + 6.0, left_center.y - g.size().y / 2.0), g, fg);
}

/// Blinkers and hazards: amber when lit, as on the car.
fn lit(on: bool) -> Color32 {
    if on { YELLOW } else { LINE2 }
}

/// The mockup's 24-unit polygons at `scale` px per unit, drawn around (12, 12).
fn poly(center: Pos2, scale: f32, pts: &[(f32, f32)]) -> Vec<Pos2> {
    pts.iter().map(|&(x, y)| center + vec2(x - 12.0, y - 12.0) * scale).collect()
}

fn gears(p: &Painter, at: Pos2, gear: i32) {
    let slot = gear_slot(gear);
    for (i, g) in GEARS.iter().enumerate() {
        let r = Rect::from_min_size(pos2(at.x + i as f32 * 22.0, at.y - 9.0), vec2(16.0, 18.0));
        if slot == Some(i) {
            p.rect_filled(r, 0.0, TEXT);
        }
        p.text(r.center(), Align2::CENTER_CENTER, g, theme::medium(12.0), if slot == Some(i) { BG } else { OFF });
    }
}

fn accelerator(p: &Painter, at: Pos2, pct: f32) {
    let pct = pct.clamp(0.0, 100.0);
    let bar = Rect::from_min_size(pos2(at.x, at.y - 4.0), vec2(44.0, 8.0));
    p.rect_filled(bar.with_max_x(bar.min.x + 44.0 * pct / 100.0), 0.0, TEXT);
    p.rect_stroke(bar, 0.0, theme::line(LINE2), StrokeKind::Inside);
    theme::label(p, pos2(bar.max.x + 6.0, at.y), &format!("{pct:.0} %"), theme::mono(12.0), TEXT, 60.0);
}

/// Turned by the wheel's angle: right (positive) is clockwise, as egui's y points down.
fn wheel(p: &Painter, center: Pos2, deg: f32) {
    let s = 16.0 / 24.0;
    let st = Stroke::new(1.75 * s, TEXT);
    let rot = Rot2::from_angle(deg.to_radians());
    let at = |x: f32, y: f32| center + rot * (Vec2::new(x, y) * s);
    p.circle_stroke(center, 9.0 * s, st);
    p.circle_stroke(center, 2.0 * s, st);
    for [a, b] in [[(-8.5, -1.5), (-2.0, -1.5)], [(2.0, -1.5), (8.5, -1.5)], [(0.0, 2.0), (0.0, 9.0)]] {
        p.line_segment([at(a.0, a.1), at(b.0, b.1)], st);
    }
}

fn blinkers(p: &Painter, at: Pos2, left: bool, right: bool) {
    let s = 16.0 / 24.0;
    let tri = |x: f32, pts: &[(f32, f32)], on: bool| {
        p.add(Shape::convex_polygon(poly(pos2(x, at.y), s, pts), lit(on), Stroke::NONE));
    };
    tri(at.x + 8.0, &[(15.0, 4.0), (6.0, 12.0), (15.0, 20.0)], left);
    tri(at.x + 30.0, &[(9.0, 4.0), (18.0, 12.0), (9.0, 20.0)], right);
}

fn hazards(p: &Painter, at: Pos2, on: bool) {
    let center = pos2(at.x + 9.0, at.y);
    let st = Stroke::new(1.5, lit(on));
    p.add(Shape::closed_line(poly(center, 16.0 / 22.0, &[(12.0, 3.0), (22.0, 21.0), (2.0, 21.0)]), st));
    p.add(Shape::closed_line(poly(center, 16.0 / 22.0, &[(12.0, 9.0), (17.0, 18.0), (7.0, 18.0)]), st));
}

fn speed(p: &Painter, cell: Rect, t: &Telemetry, metric: bool) {
    let number = format!("{:.0}", speed_in(t.speed_mps, metric));
    let (x, y) = (cell.min.x + 16.0, cell.center().y);
    let w = theme::label(p, pos2(x, y), &number, theme::medium(26.0), TEXT, cell.width());
    // On the number's baseline.
    theme::spaced(p, pos2(x + w + 6.0, y + 6.0), if metric { "KM/H" } else { "MPH" }, theme::mono(10.0), TEXT3, 1.4, 60.0);
}

/// `now` is the record at the playhead. Without one: `loading` says it may still come.
pub fn strip(ui: &Ui, rect: Rect, now: Option<&Record>, loading: bool, metric: bool) {
    let cells = cell_rects(rect);
    for (cell, (name, ..)) in cells.iter().zip(CELLS).skip(1) {
        theme::label(&ui.painter_at(*cell), pos2(cell.min.x + 14.0, cell.min.y + 14.0), name, theme::mono(10.0), TEXT3, cell.width() - 20.0);
    }
    for (i, cell) in cells.iter().enumerate() {
        let p = ui.painter_at(*cell);
        let at = pos2(cell.min.x + 14.0, cell.min.y + 33.0);
        let Some(r) = now else {
            let (_, _, value) = CELLS[i];
            if i == 0 {
                // The speed cell has no caption in the mockup: its big number and unit say it.
                let msg = if loading { "reading vehicle data…" } else { "no vehicle data" };
                theme::label(&p, pos2(at.x, cell.center().y), msg, theme::mono(11.0), TEXT3, cell.width() - 20.0);
            } else {
                theme::label(&p, at, value, theme::mono(12.0), if i == 1 { OFF } else { LINE2 }, cell.width() - 20.0);
            }
            continue;
        };
        let t = &r.tel;
        match i {
            0 => speed(&p, *cell, t, metric),
            1 => gears(&p, at, t.gear),
            2 => chip(&p, at, if t.brake { "on" } else { "off" }, t.brake, (RED, RED_INK)),
            3 => accelerator(&p, at, t.accel_pct),
            4 => {
                wheel(&p, pos2(at.x + 8.0, at.y), t.steering_deg);
                theme::label(&p, pos2(at.x + 22.0, at.y), &steer_text(t.steering_deg), theme::mono(12.0), TEXT, cell.width() - 40.0);
            }
            5 => blinkers(&p, at, t.blink_left, t.blink_right),
            6 => hazards(&p, at, t.blink_left && t.blink_right),
            7 => chip(&p, at, ap_text(t.autopilot), t.autopilot != AP_NONE, (TEXT, BG)),
            _ if r.g.abs() >= HARD_G => chip(&p, at, &g_text(r.g), true, (TEXT, BG)),
            _ => {
                theme::label(&p, at, &g_text(r.g), theme::mono(12.0), TEXT, cell.width() - 20.0);
            }
        }
    }
    let p = ui.painter_at(rect);
    for cell in cells {
        p.vline(cell.max.x - 0.5, cell.y_range(), theme::line(LINE));
    }
    p.hline(rect.x_range(), rect.max.y - 0.5, theme::line(LINE));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sei::{AP_AUTOSTEER, AP_FSD, AP_TACC, GEAR_DRIVE, GEAR_NEUTRAL, GEAR_PARK, GEAR_REVERSE};

    #[test]
    fn cells_fill_the_strip() {
        let r = Rect::from_min_max(pos2(288.0, 10.0), pos2(1440.0, 60.0));
        let c = cell_rects(r);
        assert_eq!(c[0].min.x, 288.0);
        assert!((c[8].max.x - 1440.0).abs() < 0.01);
        assert!(c.windows(2).all(|w| (w[0].max.x - w[1].min.x).abs() < 0.001));
    }

    #[test]
    fn gears_keep_their_labels_in_display_order() {
        let at = [GEAR_PARK, GEAR_REVERSE, GEAR_NEUTRAL, GEAR_DRIVE].map(gear_slot);
        assert_eq!(at, [Some(0), Some(1), Some(2), Some(3)]);
        // The proto's numbering.
        assert_eq!((0..4).map(|g| GEARS[gear_slot(g).unwrap()]).collect::<Vec<_>>(), ["P", "D", "R", "N"]);
        assert_eq!(gear_slot(7), None, "an unknown gear fills nothing");
    }

    #[test]
    fn steering_text() {
        assert_eq!(steer_text(16.4), "16° right");
        assert_eq!(steer_text(-220.0), "220° left");
        assert_eq!(steer_text(0.0), "0°");
        assert_eq!(steer_text(-0.3), "0°");
    }

    #[test]
    fn g_force_text() {
        assert_eq!(g_text(-0.523), "−0.52 g");
        assert_eq!(g_text(0.1), "+0.10 g");
        assert_eq!(g_text(-0.001), "+0.00 g", "no minus on zero");
    }

    #[test]
    fn autopilot_text() {
        assert_eq!(ap_text(AP_NONE), "manual");
        assert_eq!([AP_FSD, AP_AUTOSTEER, AP_TACC].map(ap_text), ["fsd", "autosteer", "tacc"]);
    }
}
