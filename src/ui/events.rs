//! The library panel, and the text it and the top bar show for an event.

use super::App;
use super::grid::{fit, flow};
use super::theme::{self, BG, Face, HOVER, Icon, LINE, LINE2, Look, SELECTED, TEXT, TEXT2, TEXT3, VIDEO_BG};
use crate::library::{Event, Kind, civil};
use eframe::egui::{Align2, Color32, CursorIcon, Frame, Panel, Rect, Role, ScrollArea, Sense, TextureHandle, Ui, UiBuilder, WidgetInfo, pos2, vec2};

pub const WIDTH: f32 = 288.0;
const PAD: f32 = 16.0;
const ROW_H: f32 = 73.0;
/// Between filters, and between their rows when they wrap.
const FILTER_GAP: f32 = 14.0;
const FILTER_H: f32 = 24.0;
const LOCK_HINT: &str = "Encrypted clips can't be played. Turn off Controls → Safety → Encrypt Dashcam Recordings in the car.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Saved,
    Sentry,
    Recent,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::All, Filter::Saved, Filter::Sentry, Filter::Recent];

    fn name(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Saved => "saved",
            Filter::Sentry => "sentry",
            Filter::Recent => "recent",
        }
    }

    pub fn matches(self, kind: Kind) -> bool {
        match self {
            Filter::All => true,
            Filter::Saved => kind == Kind::Saved,
            Filter::Sentry => kind == Kind::Sentry,
            Filter::Recent => kind == Kind::Recent,
        }
    }
}

pub fn kind_tag(kind: Kind) -> &'static str {
    match kind {
        Kind::Saved => "saved",
        Kind::Sentry => "sentry",
        Kind::Recent => "recent",
        Kind::Folder => "folder",
    }
}

/// `HH:MM:SS`.
pub fn hms(secs: i64) -> String {
    let (.., h, m, s) = civil(secs);
    format!("{h:02}:{m:02}:{s:02}")
}

/// `YYYY-MM-DD`.
pub fn ymd(secs: i64) -> String {
    let (y, m, d, ..) = civil(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `MM:SS`, with hours in front when there are any.
pub fn fmt_dur(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

/// Tesla's `reason` code in words.
pub fn reason_label(code: &str) -> String {
    if code.contains("dashcam") {
        return "Dashcam button".to_owned();
    }
    if code.contains("honk") {
        return "Honk".to_owned();
    }
    if code.contains("object_detection") {
        return "Object detected".to_owned();
    }
    if code.contains("accel") {
        return "Bump detected".to_owned();
    }
    let rest = code.strip_prefix("user_interaction_").or_else(|| code.strip_prefix("sentry_aware_")).unwrap_or(code);
    let words = rest.replace('_', " ");
    let mut c = words.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

/// Where playback starts on open: 10 s before a Saved/Sentry trigger.
pub fn start_position(ev: &Event) -> f64 {
    match (ev.kind, ev.info.as_ref().and_then(|i| i.timestamp)) {
        (Kind::Saved | Kind::Sentry, Some(ts)) => (ts - ev.segments[0].start - 10).max(0) as f64,
        _ => 0.0,
    }
}

/// Events the user can mark: the save button is pressed after the fact, so the moment is theirs to
/// place. A Sentry trigger already is the moment, and RecentClips rolls away.
pub fn markable(kind: Kind) -> bool {
    matches!(kind, Kind::Saved | Kind::Folder)
}

/// Seconds of lead-in before a mark when its event opens.
pub const MARK_LEAD: f64 = 3.0;

/// "Dashcam button · Oakland", or what stands in for it.
fn what(ev: &Event) -> String {
    let (reason, city) = ev.info.as_ref().map_or((String::new(), ""), |i| (reason_label(&i.reason), i.city.as_str()));
    match (reason.is_empty(), city.is_empty()) {
        (false, false) => format!("{reason} · {city}"),
        (false, true) => reason,
        (true, false) => city.to_owned(),
        (true, true) if ev.kind == Kind::Recent => "Rolling buffer".to_owned(),
        (true, true) => ev.name.clone(),
    }
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

pub fn title(ev: &Event) -> String {
    let t = ev.time();
    format!("{} {}", ymd(t), hms(t))
}

pub fn subtitle(ev: &Event) -> String {
    let cams = (0..6).filter(|&c| ev.segments.iter().any(|s| s.files[c].is_some())).count();
    let street = ev.info.as_ref().map_or("", |i| i.street.as_str());
    let place = if street.is_empty() { what(ev) } else { format!("{} · {street}", what(ev)) };
    format!("{place} · {} · {}", plural(ev.segments.len(), "segment"), plural(cams, "camera"))
}

/// The three text lines and the duration chip of a row.
#[derive(Debug, PartialEq)]
pub struct RowText {
    pub time: String,
    pub tag: &'static str,
    pub line2: String,
    pub meta: String,
    pub dur: String,
}

/// `mark`: the wall-clock second the user marked, if any.
pub fn row_text(ev: &Event, mark: Option<f64>) -> RowText {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let t = ev.time();
    let (_, m, d, ..) = civil(t);
    let date = format!("{} {d}", MONTHS[m as usize - 1]);
    let dur = fmt_dur(ev.approx_duration());
    if ev.encrypted {
        return RowText { time: hms(t), tag: "locked", line2: format!("{date} · Encrypted, can't play"), meta: "see Controls → Safety".to_owned(), dur };
    }
    let gaps = ev.segments.windows(2).filter(|w| w[1].start - w[0].start > 90).count();
    let mut meta = format!("{dur} · {} seg", ev.segments.len());
    if gaps > 0 {
        meta += &format!(" · {}", plural(gaps, "gap"));
    }
    if let Some(m) = mark {
        meta += &format!(" · mark {}", hms(m as i64));
    }
    RowText { time: hms(t), tag: kind_tag(ev.kind), line2: format!("{date} · {}", what(ev)), meta, dur }
}

enum Clicked {
    Open(usize),
    /// Right-clicked: archive or delete.
    Menu(usize),
    Collapse,
    Change,
    Import,
}

pub fn panel(app: &mut App, ui: &mut Ui) {
    let mut open = app.settings.library_open;
    let frame = Frame::NONE.fill(BG);
    let action = Panel::left("library").exact_size(WIDTH).resizable(false).frame(frame).show_collapsible(ui, &mut open, |ui| contents(ui, app));
    app.settings.library_open = open;
    match action.and_then(|r| r.inner) {
        // PLAN Phase 2: these start playing 10 s before their trigger.
        Some(Clicked::Open(i)) => app.open_event(ui.ctx(), i, matches!(app.events[i].kind, Kind::Saved | Kind::Sentry)),
        Some(Clicked::Menu(i)) => super::manage::ask(app, i),
        Some(Clicked::Collapse) => app.settings.library_open = false,
        Some(Clicked::Change) => app.pick_folder(ui.ctx()),
        Some(Clicked::Import) => super::import::open_for(app),
        None => {}
    }
}

fn contents(ui: &mut Ui, app: &mut App) -> Option<Clicked> {
    let full = ui.max_rect();
    let inner = full.shrink2(vec2(PAD, 0.0));
    let mut clicked = None;

    let head = Rect::from_min_size(pos2(inner.min.x, full.min.y + PAD), vec2(inner.width(), 32.0));
    theme::spaced(ui.painter(), head.left_center(), "LIBRARY", theme::medium(11.0), TEXT3, 1.54, 150.0);
    let collapse = head.with_min_x(head.max.x - 32.0);
    if theme::button(ui, collapse, Face::Icon(Icon::ChevronLeft, 14.0), Look::Bare, true, "Collapse library").clicked() {
        clicked = Some(Clicked::Collapse);
    }

    let path_row = Rect::from_min_size(pos2(inner.min.x, head.max.y + 14.0), vec2(inner.width(), 20.0));
    let path = app.folder.as_ref().map_or("no folder".to_owned(), |p| p.display().to_string());
    // A running import says so here; otherwise the open folder can be imported, unless it is the library.
    let importing = super::import::status(app);
    let importable = app.folder.is_some() && app.folder != app.settings.library_dir;
    let word = importing.as_deref().unwrap_or("import");
    let word_w = if importing.is_some() || importable { theme::text_width(ui, word, theme::mono(11.5)).ceil() } else { 0.0 };
    theme::label(ui.painter(), path_row.left_center(), &path, theme::mono(11.5), TEXT2, inner.width() - 68.0 - word_w);
    let change = path_row.with_min_x(path_row.max.x - 44.0);
    if link(ui, change, "change").clicked() {
        clicked = Some(Clicked::Change);
    }
    let import = path_row.with_max_x(change.min.x - 12.0).with_min_x(change.min.x - 12.0 - word_w);
    if word_w > 0.0 && link(ui, import, word).clicked() {
        clicked = Some(Clicked::Import);
    }

    let labels = Filter::ALL.map(|f| format!("{} {}", f.name(), app.events.iter().filter(|e| f.matches(e.kind)).count()));
    let widths = labels.each_ref().map(|l| theme::text_width(ui, l, theme::mono(11.5)).ceil());
    let spots = flow(&widths, widths.len(), inner.width(), FILTER_GAP);
    let filters_top = path_row.max.y + 14.0;
    let filter_y = |row: usize| filters_top + row as f32 * (FILTER_H + FILTER_GAP);
    for (i, f) in Filter::ALL.into_iter().enumerate() {
        let (row, x) = spots[i];
        let cell = Rect::from_min_size(pos2(inner.min.x + x, filter_y(row)), vec2(widths[i], FILTER_H));
        if theme::toggles(ui, cell, &[&labels[i]], (app.filter == f).then_some(0), 0.0).is_some() {
            app.filter = f;
        }
    }
    let rule = filter_y(spots[widths.len() - 1].0) + FILTER_H + 9.5;
    ui.painter().hline(full.x_range(), rule, theme::line(LINE));

    let mut list = full.with_min_y(rule + 0.5);
    let rows: Vec<usize> = (0..app.events.len()).filter(|&i| app.filter.matches(app.events[i].kind)).collect();
    if rows.is_empty() {
        let msg = if app.events.is_empty() { app.status_text() } else { "none of this kind".to_owned() };
        ui.painter().with_clip_rect(list).text(pos2(inner.min.x, list.min.y + 20.0), Align2::LEFT_CENTER, msg, theme::mono(11.0), TEXT3);
        return clicked;
    }
    // Here and not in a tooltip: that would reach over the left camera (design rule 1).
    if rows.iter().any(|&i| app.events[i].encrypted) {
        let g = ui.painter().layout(LOCK_HINT.to_owned(), theme::sans(11.5), TEXT3, inner.width());
        list.max.y -= g.size().y + 20.0;
        ui.painter().hline(full.x_range(), list.max.y + 0.5, theme::line(LINE));
        ui.painter().galley(pos2(inner.min.x, list.max.y + 10.0), g, TEXT3);
    }
    // The clicked row lights up at once, even if its event takes a moment to open.
    let (events, thumbs, current) = (&app.events, &app.thumbs, app.opening.as_ref().map_or(app.current, |o| Some(o.event)));
    let mark = |ev: &Event| app.settings.marks.get(&ev.name).filter(|_| markable(ev.kind)).map(|m| m.0);
    ui.scope_builder(UiBuilder::new().max_rect(list), |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for &i in &rows[range] {
                match row(ui, &events[i], mark(&events[i]), current == Some(i), thumbs.get(&i)) {
                    (true, _) => clicked = Some(Clicked::Open(i)),
                    (_, true) => clicked = Some(Clicked::Menu(i)),
                    _ => {}
                }
            }
        });
    });
    clicked
}

fn link(ui: &mut Ui, rect: Rect, text: &str) -> eframe::egui::Response {
    let r = ui.allocate_rect(rect, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    r.widget_info(|| WidgetInfo::labeled(Role::Button, true, text));
    let p = ui.painter();
    p.text(rect.center(), Align2::CENTER_CENTER, text, theme::mono(11.0), if r.hovered() { TEXT } else { TEXT3 });
    p.hline(rect.x_range(), rect.max.y - 0.5, theme::line(LINE2));
    r
}

/// (clicked to open, right-clicked). An encrypted event can't be opened, but it can be removed.
fn row(ui: &mut Ui, ev: &Event, mark: Option<f64>, selected: bool, thumb: Option<&TextureHandle>) -> (bool, bool) {
    let t = row_text(ev, mark);
    let (full, r) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    r.widget_info(|| WidgetInfo::labeled(Role::Button, !ev.encrypted, format!("{} {}", t.time, t.line2)));
    let r = r.on_hover_cursor(if ev.encrypted { CursorIcon::NotAllowed } else { CursorIcon::PointingHand });
    if !ui.is_rect_visible(full) {
        return (false, false);
    }
    let body = full.shrink2(vec2(PAD, 0.0));
    let p = ui.painter();
    if selected {
        p.rect_filled(body, 0.0, SELECTED);
    } else if r.hovered() && !ev.encrypted {
        p.rect_filled(body, 0.0, HOVER);
    }
    // Disabled rows are dimmed as a whole, like the mockup's opacity.
    let opacity = if ev.encrypted { 0.6 } else { 1.0 };
    let dim = |c: Color32| c.gamma_multiply(opacity);
    p.hline(body.x_range(), body.max.y - 0.5, theme::line(dim(LINE)));

    let cy = body.center().y;
    let image = Rect::from_center_size(pos2(body.min.x + 6.0 + 36.0, cy), vec2(72.0, 48.0));
    p.rect_filled(image, 0.0, VIDEO_BG);
    if ev.encrypted {
        theme::paint_icon(p, image.center(), Icon::Lock, 18.0, dim(TEXT3));
    } else {
        if let Some(tex) = thumb {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            p.image(tex.id(), fit(image, tex.size_vec2()), uv, Color32::WHITE);
        }
        let g = p.layout_no_wrap(t.dur.clone(), theme::mono(9.5), TEXT);
        let chip = Rect::from_min_max(image.max - g.size() - vec2(8.0, 2.0), image.max);
        p.rect_filled(chip, 0.0, BG);
        p.galley(chip.min + vec2(4.0, 1.0), g, TEXT);
    }

    let x = image.max.x + 10.0;
    let max = body.max.x - 6.0 - x;
    let top = cy - 26.0;
    let w = theme::label(p, pos2(x, top + 8.0), &t.time, theme::medium(12.5), dim(TEXT), max);
    theme::badge(p, pos2(x + w + 6.0, top + 8.0), t.tag, opacity);
    theme::label(p, pos2(x, top + 27.0), &t.line2, theme::sans(12.5), dim(TEXT2), max);
    theme::label(p, pos2(x, top + 45.0), &t.meta, theme::mono(11.0), dim(TEXT3), max);
    (r.clicked() && !ev.encrypted, r.secondary_clicked())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{EventInfo, Segment, stamp};
    use std::path::PathBuf;

    fn event(kind: Kind, starts: &[&str], info: Option<EventInfo>) -> Event {
        let segments = starts
            .iter()
            .map(|s| {
                let mut files: [Option<PathBuf>; 6] = Default::default();
                files[0] = Some(PathBuf::from("front.mp4"));
                files[4] = Some(PathBuf::from("pillar.mp4"));
                Segment { start: stamp(s).unwrap(), files }
            })
            .collect();
        Event { kind, name: "evt".to_owned(), dir: PathBuf::new(), segments, info, thumb: None, encrypted: false }
    }

    fn info(ts: &str) -> EventInfo {
        EventInfo { timestamp: stamp(ts), reason: "user_interaction_dashcam_launcher_action_tapped".to_owned(), city: "Oakland".to_owned(), ..EventInfo::default() }
    }

    #[test]
    fn saved_and_sentry_start_ten_seconds_before_the_trigger() {
        let seg = ["2025-11-02_18-50-00", "2025-11-02_18-51-00"];
        assert_eq!(start_position(&event(Kind::Saved, &seg, Some(info("2025-11-02_18-51-07")))), 57.0);
        assert_eq!(start_position(&event(Kind::Sentry, &seg, Some(info("2025-11-02_18-50-03")))), 0.0, "not before the clip");
        assert_eq!(start_position(&event(Kind::Recent, &seg, Some(info("2025-11-02_18-51-07")))), 0.0);
        assert_eq!(start_position(&event(Kind::Saved, &seg, None)), 0.0);
    }

    #[test]
    fn filter_labels_wrap_instead_of_colliding() {
        // 6.9 px per character, as in the 255 px inside the panel.
        let widths = ["all 12", "saved 8", "sentry 3", "recent 1"].map(|l| (l.len() as f32 * 6.9).ceil());
        let spots = flow(&widths, 4, 255.0, FILTER_GAP);
        assert!(spots.iter().all(|s| s.0 == 0), "{spots:?}");
        let widths = ["all 580", "saved 123", "sentry 456", "recent 1"].map(|l| (l.len() as f32 * 6.9).ceil());
        let spots = flow(&widths, 4, 255.0, FILTER_GAP);
        assert_eq!(spots[3].0, 1, "{spots:?}");
        for i in 1..4 {
            assert!(spots[i].0 > spots[i - 1].0 || spots[i].1 >= spots[i - 1].1 + widths[i - 1] + FILTER_GAP, "{spots:?}");
        }
    }

    #[test]
    fn filters() {
        assert!(Filter::All.matches(Kind::Folder));
        assert!(Filter::Saved.matches(Kind::Saved) && !Filter::Saved.matches(Kind::Sentry));
        assert!(Filter::Recent.matches(Kind::Recent) && !Filter::Sentry.matches(Kind::Folder));
    }

    #[test]
    fn durations_and_reasons() {
        assert_eq!((fmt_dur(658), fmt_dur(59), fmt_dur(3725), fmt_dur(-1)), ("10:58".into(), "00:59".into(), "1:02:05".into(), "00:00".into()));
        assert_eq!(reason_label("user_interaction_dashcam_launcher_action_tapped"), "Dashcam button");
        assert_eq!(reason_label("user_interaction_honk"), "Honk");
        assert_eq!(reason_label("sentry_aware_object_detection"), "Object detected");
        assert_eq!(reason_label("sentry_aware_accel_0.52"), "Bump detected");
        assert_eq!(reason_label("vehicle_auto_emergency_braking"), "Vehicle auto emergency braking");
        assert_eq!(reason_label(""), "");
    }

    #[test]
    fn row_and_title_text() {
        let e = event(Kind::Saved, &["2025-11-02_18-50-00", "2025-11-02_18-51-00"], Some(info("2025-11-02_18-51-07")));
        assert_eq!(title(&e), "2025-11-02 18:51:07");
        assert_eq!(subtitle(&e), "Dashcam button · Oakland · 2 segments · 2 cameras");
        let mut with_street = e.clone();
        with_street.info.as_mut().unwrap().street = "Broadway".to_owned();
        assert_eq!(subtitle(&with_street), "Dashcam button · Oakland · Broadway · 2 segments · 2 cameras");
        let r = row_text(&e, None);
        assert_eq!((r.time.as_str(), r.tag, r.line2.as_str()), ("18:51:07", "saved", "Nov 2 · Dashcam button · Oakland"));
        assert_eq!((r.meta.as_str(), r.dur.as_str()), ("02:00 · 2 seg", "02:00"));
        assert_eq!(row_text(&e, stamp("2025-11-02_18-50-41").map(|s| s as f64 + 0.6)).meta, "02:00 · 2 seg · mark 18:50:41");
        assert!(markable(Kind::Saved) && markable(Kind::Folder) && !markable(Kind::Sentry) && !markable(Kind::Recent));

        // RecentClips: no event.json, and the 9-minute hole counts as a gap.
        let e = event(Kind::Recent, &["2025-11-01_08-00-00", "2025-11-01_08-01-00", "2025-11-01_08-10-00"], None);
        let r = row_text(&e, None);
        assert_eq!((r.tag, r.line2.as_str(), r.meta.as_str()), ("recent", "Nov 1 · Rolling buffer", "11:00 · 3 seg · 1 gap"));
        assert_eq!(subtitle(&e), "Rolling buffer · 3 segments · 2 cameras");

        let mut e = event(Kind::Sentry, &["2025-10-30_19-47-55"], None);
        e.encrypted = true;
        let r = row_text(&e, None);
        assert_eq!((r.tag, r.meta.as_str()), ("locked", "see Controls → Safety"));
        assert!(r.line2.contains("Encrypted"));
    }
}
