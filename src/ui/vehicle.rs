//! Vehicle strip at the top of the bottom panel.

use super::theme::{self, LINE, LINE2, OFF, TEXT3};
use eframe::egui::{Rect, Ui, pos2};

pub const HEIGHT: f32 = 50.0;

/// (label, share of the width, placeholder value).
const CELLS: [(&str, f32, &str); 9] = [
    ("speed", 1.7, "no vehicle data"),
    ("gear", 1.2, "P R N D"),
    ("brake", 0.8, "–"),
    ("accelerator", 1.3, "–"),
    ("steering", 1.0, "–"),
    ("blinkers", 0.9, "–"),
    ("hazards", 0.9, "–"),
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

/// Phase 2: takes the telemetry at the playhead and draws each cell per design rule 4; "no vehicle
/// data" stays for clips without SEI.
pub fn strip(ui: &Ui, rect: Rect) {
    let p = ui.painter_at(rect);
    for (i, (cell, (name, _, value))) in cell_rects(rect).into_iter().zip(CELLS).enumerate() {
        let x = cell.min.x + 14.0;
        let max = cell.width() - 20.0;
        if i == 0 {
            // The speed cell has no caption in the mockup: its big number and unit say it.
            theme::label(&p, pos2(x, cell.center().y), value, theme::mono(11.0), TEXT3, max);
        } else {
            theme::label(&p, pos2(x, cell.min.y + 14.0), name, theme::mono(10.0), TEXT3, max);
            theme::label(&p, pos2(x, cell.min.y + 33.0), value, theme::mono(12.0), if i == 1 { OFF } else { LINE2 }, max);
        }
        p.vline(cell.max.x - 0.5, cell.y_range(), theme::line(LINE));
    }
    p.hline(rect.x_range(), rect.max.y - 0.5, theme::line(LINE));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_fill_the_strip() {
        let r = Rect::from_min_max(pos2(288.0, 10.0), pos2(1440.0, 60.0));
        let c = cell_rects(r);
        assert_eq!(c[0].min.x, 288.0);
        assert!((c[8].max.x - 1440.0).abs() < 0.01);
        assert!(c.windows(2).all(|w| (w[0].max.x - w[1].min.x).abs() < 0.001));
    }
}
