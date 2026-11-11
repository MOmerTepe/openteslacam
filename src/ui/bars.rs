//! Top bar and bottom panel (map cell, vehicle strip, transport, scrubber).

use super::App;
use super::events::{self, fmt_dur, hms};
use super::export;
use super::keys::Action;
use super::settings::Layout;
use super::theme::{self, BG, Face, Icon, LINE, LINE2, Look, Row, TEXT, TEXT3, centered_strip};
use super::{map, timeline, vehicle};
use crate::player::RATES;
use eframe::egui::{Frame, Panel, Rect, Response, Ui, pos2, vec2};

const TOP_H: f32 = 56.0;
const TRANSPORT_H: f32 = 36.0;
const ROW_GAP: f32 = 12.0;
const BOTTOM_PAD: f32 = 16.0;
/// Fits `in --:--:--` at 11.5 px; design rule 3 gives Export the same width.
const TRIM_BTN_W: f32 = 104.0;
const RATE_LABELS: [&str; 5] = ["¼×", "½×", "1×", "2×", "4×"];

/// Time of day with tenths.
pub fn fmt_clock(wall: f64) -> String {
    let s = wall.floor();
    format!("{}.{}", hms(s as i64), (((wall - s) * 10.0) as u32).min(9))
}

/// The map cell is as tall as this panel, so its minimum sets the floor.
pub fn bottom_height(lanes: bool) -> f32 {
    let main = vehicle::HEIGHT + ROW_GAP + TRANSPORT_H + ROW_GAP + timeline::height(lanes) + BOTTOM_PAD;
    main.max(map::MIN_HEIGHT)
}

pub fn top_bar(app: &mut App, ui: &mut Ui) {
    Panel::top("top").exact_size(TOP_H).resizable(false).frame(Frame::NONE.fill(BG)).show(ui, |ui| {
        let bar = ui.max_rect().shrink2(vec2(16.0, 0.0));
        let mut row = Row::new(centered_strip(bar, 32.0));

        if theme::button(ui, row.left(32.0, 16.0), Face::Icon(Icon::Library, 16.0), Look::Normal, true, "Toggle library panel").clicked() {
            app.settings.library_open = !app.settings.library_open;
        }
        let word = "openteslacam";
        let word_w = theme::text_width(ui, word, theme::medium(15.0)).ceil();
        let word_r = row.left(word_w, 16.0);
        theme::label(ui.painter(), word_r.left_center(), word, theme::medium(15.0), TEXT, word_w + 1.0);
        let sep = row.left(1.0, 16.0);
        ui.painter().vline(sep.min.x + 0.5, sep.y_range(), theme::line(LINE));

        // Right side, from the window edge inwards.
        if theme::button(ui, row.right(32.0, 20.0), Face::Text("?"), Look::Normal, true, "Keyboard shortcuts").clicked() {
            app.shortcuts_open = true;
        }
        if let Some(r) = crate::update::available() {
            let text = format!("update to {}", r.version);
            let w = theme::text_width(ui, &text, theme::mono(11.5)).ceil() + 24.0;
            if theme::button(ui, row.right(w, 20.0), Face::Text(&text), Look::Primary, true, "Show what's new and update").clicked() {
                app.update_open = true;
            }
        }
        if theme::text_button(ui, row.right(100.0, 20.0), "open folder", true).clicked() {
            app.pick_folder(ui.ctx());
        }
        let units = ["km/h", "mph"];
        let w = theme::toggles_width(ui, &units, 12.0);
        let active = if app.settings.metric { 0 } else { 1 };
        if let Some(i) = theme::toggles(ui, row.right(w, 20.0), &units, Some(active), 12.0) {
            app.settings.metric = i == 0;
        }
        let names = Layout::ALL.map(Layout::name);
        let w = theme::toggles_width(ui, &names, 12.0);
        let active = Layout::ALL.iter().position(|l| *l == app.settings.layout);
        if let Some(i) = theme::toggles(ui, row.right(w, 20.0), &names, active, 12.0) {
            app.settings.layout = Layout::ALL[i];
            app.focus = None;
        }

        // Event title in what's left.
        let Some(ev) = app.current.map(|i| &app.events[i]) else { return };
        let p = ui.painter_at(row.rest.expand2(vec2(0.0, 8.0)));
        let (x, y) = (row.rest.min.x, bar.center().y);
        let badge_w = theme::badge(&p, pos2(x, y - 9.0), events::kind_tag(ev.kind), 1.0);
        let title_x = x + badge_w + 8.0;
        theme::label(&p, pos2(title_x, y - 9.0), &events::title(ev), theme::medium(13.0), TEXT, row.rest.max.x - title_x);
        theme::label(&p, pos2(x, y + 10.0), &events::subtitle(ev), theme::sans(12.5), TEXT3, row.rest.width());
    });
}

pub fn bottom_panel(app: &mut App, ui: &mut Ui) {
    let h = bottom_height(app.settings.lanes_open);
    Panel::bottom("bottom").exact_size(h).resizable(false).frame(Frame::NONE.fill(BG)).show(ui, |ui| {
        let full = ui.max_rect();
        let (map_cell, main) = full.split_left_right_at_x(full.min.x + map::WIDTH);
        let est = app.current.and_then(|i| app.events[i].info.as_ref()).and_then(|i| Some((i.est_lat?, i.est_lon?)));
        let view = map::View { now: app.player.as_ref().map(|p| p.position), store: app.telemetry.as_ref(), loading: app.telemetry_rx.is_some(), est };
        let seek = map::cell(ui, map_cell, &mut app.settings, &mut app.map, view);
        if let Some((p, t)) = app.player.as_mut().zip(seek) {
            p.seek(t);
        }
        ui.painter().vline(map_cell.max.x - 0.5, map_cell.y_range(), theme::line(LINE));

        let (strip, rest) = main.split_top_bottom_at_y(main.min.y + vehicle::HEIGHT);
        let now = app.player.as_ref().zip(app.telemetry.as_ref()).and_then(|(p, s)| s.at(p.position));
        vehicle::strip(ui, strip, now, app.telemetry_rx.is_some(), app.settings.metric);

        let x = rest.x_range();
        let transport_r = Rect::from_x_y_ranges(x.min + 16.0..=x.max - 16.0, rest.min.y + ROW_GAP..=rest.min.y + ROW_GAP + TRANSPORT_H);
        transport(app, ui, transport_r);
        let top = transport_r.max.y + ROW_GAP;
        let scrub_r = Rect::from_x_y_ranges(transport_r.x_range(), top..=top + timeline::height(app.settings.lanes_open));
        let flag = app.current.zip(app.player.as_ref()).and_then(|(i, p)| timeline::event_flag(&app.events[i], p.start));
        let trim = app.player.as_ref().filter(|_| app.trim.is_set()).map(|p| app.trim.range(p.duration()));
        let mark = app.mark().zip(app.player.as_ref()).map(|((a, b), p)| (a - p.start as f64, b - p.start as f64));
        let view = timeline::View { store: app.telemetry.as_ref(), cache: &mut app.lane_cache, flag, metric: app.settings.metric, trim, mark };
        app.scrubbing = timeline::show(ui, scrub_r, app.player.as_mut(), &mut app.settings.lanes_open, view);
    });
}

fn transport(app: &mut App, ui: &mut Ui, rect: Rect) {
    let mut row = Row::new(centered_strip(rect, 32.0));
    let has = app.player.is_some();
    let playing = app.player.as_ref().is_some_and(|p| p.playing);
    let mut act = None;
    let mut btn = |ui: &mut Ui, r: Rect, icon: Icon, look: Look, enabled: bool, name: &str, a: Action| {
        if theme::button(ui, r, Face::Icon(icon, 14.0), look, enabled, name).on_hover_text(name).clicked() {
            act = Some(a);
        }
    };
    let stepping = has && !playing;
    btn(ui, row.left(32.0, 4.0), Icon::PrevSeg, Look::Normal, has, "Previous segment (Page Up)", Action::PrevSegment);
    btn(ui, row.left(32.0, 4.0), Icon::ChevronLeft, Look::Normal, stepping, "Step back one frame (,)", Action::Frame(-1));
    let (icon, name) = if playing { (Icon::Pause, "Pause (Space)") } else { (Icon::Play, "Play (Space)") };
    btn(ui, row.left(32.0, 4.0), icon, Look::Primary, has, name, Action::TogglePlay);
    btn(ui, row.left(32.0, 4.0), Icon::ChevronRight, Look::Normal, stepping, "Step forward one frame (.)", Action::Frame(1));
    btn(ui, row.left(32.0, 10.0), Icon::NextSeg, Look::Normal, has, "Next segment (Page Down)", Action::NextSegment);
    if let Some(a) = act {
        app.apply(a);
    }

    let label = app.player.as_ref().map_or("export →".to_owned(), |p| {
        let (from, to) = app.trim.range(p.duration());
        format!("export {} →", export::span_text(crate::export::footage(&p.segs, from, to)))
    });
    if theme::text_button(ui, row.right(TRIM_BTN_W, 14.0), &label, has).clicked() {
        app.export.open = true;
    }
    // Only for events in the library: a drive stays as the car wrote it. Both ask before anything goes.
    if super::manage::allowed(app) {
        for (icon, what, name) in [
            (Icon::DropCameras, super::manage::What::Cameras, "Drop cameras: remove the ones you don't need from this event"),
            (Icon::Trim, super::manage::What::Trim, "Trim: keep In to Out (or the mark) and remove the rest of this event"),
        ] {
            let gap = if what == super::manage::What::Trim { 14.0 } else { 4.0 };
            if theme::button(ui, row.right(32.0, gap), Face::Icon(icon, 14.0), Look::Normal, true, name).on_hover_text(name).clicked() {
                super::manage::open(app, what);
            }
        }
    }
    let start = app.player.as_ref().map_or(0, |p| p.start);
    let out = trim_button(ui, row.right(TRIM_BTN_W, 4.0), "out", app.trim.end, start, has, "Set Out point (O), right-click clears");
    if out.clicked() {
        app.apply(Action::TrimOut);
    }
    if out.secondary_clicked() {
        app.trim.end = None;
    }
    let inn = trim_button(ui, row.right(TRIM_BTN_W, 4.0), "in", app.trim.start, start, has, "Set In point (I), right-click clears");
    if inn.clicked() {
        app.apply(Action::TrimIn);
    }
    if inn.secondary_clicked() {
        app.trim.start = None;
    }
    // Only where a mark means something: Saved events and loose folders.
    let markable = app.current.is_some_and(|i| events::markable(app.events[i].kind));
    let at = app.mark().map(|m| m.0 - start as f64);
    let mark = trim_button(ui, row.right(TRIM_BTN_W, 14.0), "mark", at, start, has && markable, "Mark where it happened (M): In to Out, or here. Right-click clears");
    if mark.clicked() {
        app.apply(Action::Mark);
    }
    if mark.secondary_clicked()
        && let Some(i) = app.current
    {
        app.settings.marks.remove(&app.events[i].name);
    }
    let w = theme::toggles_width(ui, &RATE_LABELS, 12.0);
    let active = app.player.as_ref().and_then(|p| RATES.iter().position(|r| *r == p.rate));
    if let Some(i) = theme::toggles(ui, row.right(w, 14.0), &RATE_LABELS, active, 12.0)
        && let Some(p) = &mut app.player
    {
        p.rate = RATES[i];
    }

    let Some(p) = &app.player else { return };
    let x = row.rest.min.x;
    let painter = ui.painter();
    theme::label(painter, pos2(x, rect.min.y + 12.0), &fmt_clock(p.wall_clock()), theme::medium(18.0), TEXT, row.rest.width());
    let line2 = format!("{} / {} · segment {}/{}", fmt_dur(p.position as i64), fmt_dur(p.duration() as i64), p.segment() + 1, p.segs.len());
    theme::label(painter, pos2(x, rect.max.y - 5.0), &line2, theme::mono(11.0), TEXT3, row.rest.width());
}

/// `in 18:50:50`: the word dim, the time bright, as in the mockup.
fn trim_button(ui: &mut Ui, rect: Rect, word: &str, at: Option<f64>, start: i64, enabled: bool, name: &str) -> Response {
    let r = theme::button(ui, rect, Face::Text(""), Look::Normal, enabled, name).on_hover_text(name);
    let value = at.map_or("--:--:--".to_owned(), |t| hms(start + t as i64));
    let font = theme::mono(11.5);
    let (word_w, value_w) = (theme::text_width(ui, word, font.clone()), theme::text_width(ui, &value, font.clone()));
    let y = rect.center().y;
    let x = rect.center().x - (word_w + 7.0 + value_w) / 2.0;
    let (dim, bright) = if enabled { (TEXT3, TEXT) } else { (LINE2, LINE2) };
    let p = ui.painter();
    theme::label(p, pos2(x, y), word, font.clone(), dim, word_w + 1.0);
    theme::label(p, pos2(x + word_w + 7.0, y), &value, font, bright, value_w + 1.0);
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::stamp;

    #[test]
    fn clock_text() {
        let t = stamp("2025-11-02_18-51-07").unwrap() as f64;
        assert_eq!(fmt_clock(t), "18:51:07.0");
        assert_eq!(fmt_clock(t + 0.27), "18:51:07.2");
        assert_eq!(fmt_clock(t + 0.999), "18:51:07.9");
    }

    #[test]
    fn map_cell_floor_holds_with_lanes_closed() {
        assert_eq!(bottom_height(false), map::MIN_HEIGHT);
        assert!(bottom_height(true) > map::MIN_HEIGHT);
    }
}
