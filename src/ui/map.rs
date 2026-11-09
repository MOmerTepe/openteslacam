//! The bottom-left map cell. Phase 3 puts `walkers::Map` where the placeholder body is painted.

use super::theme::{self, LINE, TEXT, TEXT3};
use eframe::egui::{Color32, Rect, Ui, pos2};

pub const WIDTH: f32 = 288.0;
const HEADER: f32 = 36.0;
const FOOTER: f32 = 27.0;
const BODY_MIN: f32 = 110.0;
/// Header + the mockup's minimum body + coordinates line.
pub const MIN_HEIGHT: f32 = HEADER + BODY_MIN + FOOTER;

pub fn coords_text(coords: Option<(f64, f64)>) -> String {
    coords.map_or("no position".to_owned(), |(lat, lon)| format!("{lat:.4}, {lon:.4}").replace('-', "−"))
}

/// `rect` is the whole cell, as tall as the bottom panel. Phase 3: map/sat and north/heading toggles
/// go in the header (Settings::map_mode), the heading after the coordinates.
pub fn cell(ui: &Ui, rect: Rect, coords: Option<(f64, f64)>) {
    let p = ui.painter_at(rect);
    let (header, rest) = rect.split_top_bottom_at_y(rect.min.y + HEADER);
    let (body, footer) = rest.split_top_bottom_at_y(rect.max.y - FOOTER);
    theme::label(&p, pos2(rect.min.x + 16.0, header.center().y), "map", theme::mono(11.0), TEXT, 100.0);
    p.hline(rect.x_range(), header.max.y - 0.5, theme::line(LINE));
    p.rect_filled(body, 0.0, Color32::from_rgb(0x1a, 0x1a, 0x18));
    p.hline(rect.x_range(), footer.min.y + 0.5, theme::line(LINE));
    theme::label(&p, pos2(rect.min.x + 16.0, footer.center().y), &coords_text(coords), theme::mono(10.5), TEXT3, WIDTH - 32.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_line() {
        assert_eq!(coords_text(Some((37.80441, -122.27119))), "37.8044, −122.2712");
        assert_eq!(coords_text(None), "no position");
    }
}
