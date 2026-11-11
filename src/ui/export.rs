//! The export dialog, and what the composited video paints with the app's widgets: tile captions and
//! the band under the cameras. The worker lives in `crate::export`.

use super::bars::fmt_clock;
use super::events::{fmt_dur, hms, ymd};
use super::theme::{self, BG, Face, HOVER, Icon, LINE, LINE2, Look, Row, TEXT, TEXT2, TEXT3};
use super::{App, grid, map, vehicle};
use crate::export::{self, CANCELLED, Composite, Job, Mode, RESOLUTIONS, Summary};
use crate::library::{CAMERAS, fmt_stamp};
use crate::player::{Player, SegTime};
use crate::telemetry::Store;
use eframe::egui::{Align2, Color32, Context, Frame, Id, Margin, Modal, Rect, Role, Sense, Stroke, StrokeKind, Ui, WidgetInfo, pos2, vec2};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;

/// Height of the band under the cameras, in points of the 720 tall canvas.
pub const BAND_H: f32 = 80.0;
pub const CAPTION_H: f32 = 18.0;
const MAP_W: f32 = 256.0;
/// Dialog width inside its 22 px padding (the mockup's 580).
pub(super) const W: f32 = 536.0;
pub(super) const BTN_H: f32 = 32.0;
/// Fits `cancel` and `export`; design rule 3 makes them equal.
const BTN_W: f32 = 72.0;

/// The band's data for one frame.
pub struct Band<'a> {
    pub store: &'a Store,
    pub t: f64,
    pub wall: f64,
    pub hud: bool,
    pub map: bool,
    pub metric: bool,
}

/// Map cell on the left as in the app, then the time of day and the vehicle strip.
pub fn band(ui: &Ui, rect: Rect, b: &Band) {
    let p = ui.painter();
    p.rect_filled(rect, 0.0, BG);
    p.hline(rect.x_range(), rect.min.y + 0.5, theme::line(LINE));
    let mut rest = rect;
    let now = b.store.at(b.t);
    if b.map {
        let (cell, right) = rect.split_left_right_at_x(rect.min.x + MAP_W);
        map::bare_track(p, &b.store.records, b.t, now.map(|r| r.tel.heading), cell);
        p.vline(cell.max.x - 0.5, cell.y_range(), theme::line(LINE));
        rest = right;
    }
    let (clock, strip) = if b.hud { rest.split_top_bottom_at_y(rest.max.y - vehicle::HEIGHT) } else { (rest, Rect::NOTHING) };
    let x = clock.min.x + 16.0;
    let w = theme::label(p, pos2(x, clock.center().y), &fmt_clock(b.wall), theme::medium(18.0), TEXT, clock.width());
    theme::label(p, pos2(x + w + 12.0, clock.center().y), &ymd(b.wall as i64), theme::mono(11.0), TEXT3, clock.width());
    if b.hud {
        vehicle::strip(ui, strip, now, false, b.metric);
    }
}

/// The strip above a tile, as on screen.
pub fn caption(ui: &Ui, rect: Rect, cam: usize) {
    let p = ui.painter();
    p.rect_filled(rect, 0.0, BG);
    p.hline(rect.x_range(), rect.min.y + 0.5, theme::line(LINE));
    p.vline(rect.min.x + 0.5, rect.y_range(), theme::line(LINE));
    p.vline(rect.max.x - 0.5, rect.y_range(), theme::line(LINE));
    theme::spaced(p, pos2(rect.min.x + 8.0, rect.center().y), grid::LABELS[cam], theme::medium(10.0), TEXT2, 1.4, rect.width() - 16.0);
}

/// `30 s`, or `12:03` from a minute up. Rounds up: a sliver of footage isn't `0 s`.
pub fn span_text(secs: f64) -> String {
    let s = secs.ceil() as i64;
    if s < 60 { format!("{s} s") } else { fmt_dur(s) }
}

/// `segment 3`, or `segments 10–11` for what `from..=to` touches.
fn segments_text(segs: &[SegTime], from: f64, to: f64) -> String {
    let hit: Vec<usize> = (0..segs.len()).filter(|&i| segs[i].dur > 0.0 && segs[i].start <= to && segs[i].start + segs[i].dur > from).collect();
    match (hit.first(), hit.last()) {
        (Some(a), Some(b)) if a != b => format!("segments {}–{}", a + 1, b + 1),
        (Some(a), _) => format!("segment {}", a + 1),
        _ => "no footage".to_owned(),
    }
}

/// What will be written, as in the mockup's footer.
fn summary_text(composite: bool, cameras: usize, csv: bool) -> String {
    let csv = if csv { " + telemetry.csv" } else { "" };
    if composite { format!("1 × mp4 (h.264){csv}") } else { format!("{cameras} × mp4{csv} · no re-encode") }
}

/// `dir/<stem>` without the .mp4 a save dialog may have added.
fn base_of(picked: PathBuf) -> PathBuf {
    if picked.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")) { picked.with_extension("") } else { picked }
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map_or_else(|| PathBuf::from("."), PathBuf::from)
}

pub(super) struct Running {
    cancel: Arc<AtomicBool>,
    /// f32 bits, 0..=1.
    progress: Arc<AtomicU32>,
    rx: Receiver<Result<Summary, String>>,
}

pub struct Dialog {
    pub open: bool,
    pub(super) composite: bool,
    /// Indexed like `CAMERAS`.
    cams: [bool; 6],
    /// Index into `RESOLUTIONS`.
    res: usize,
    hud: bool,
    map: bool,
    csv: bool,
    /// Encode on the GPU; on by default each run, where the machine has a GPU encoder.
    gpu: bool,
    /// Picked with "change"; until then the default name follows In.
    pub(super) dest: Option<PathBuf>,
    pub(super) running: Option<Running>,
    /// How the last run ended; cleared on closing.
    pub(super) outcome: Option<Result<Summary, String>>,
}

impl Default for Dialog {
    fn default() -> Self {
        Dialog { open: false, composite: false, cams: [true; 6], res: 1, hud: true, map: true, csv: true, gpu: true, dest: None, running: None, outcome: None }
    }
}

enum Step {
    None,
    Close,
    Pick,
    Start,
    Cancel,
}

/// Where the export would go now, as a path without extension.
fn base(app: &App, p: &Player) -> PathBuf {
    let dir = || app.settings.export_dir.clone().unwrap_or_else(home);
    let (from, _) = app.trim.range(p.duration());
    app.export.dest.clone().unwrap_or_else(|| dir().join(fmt_stamp(p.start + from as i64)))
}

/// Starts the worker with the dialog's settings; it runs on clones, so I/O and seeks don't reach it.
// shortcut: quitting mid-export leaves its .part files behind (never a finished-looking file); cancel and join on exit if they pile up.
pub fn start(app: &mut App) {
    let (Some(p), Some(i)) = (&app.player, app.current) else { return };
    let (from, to) = app.trim.range(p.duration());
    let d = &app.export;
    let mode = if d.composite {
        let (_, width, height, bps) = RESOLUTIONS[d.res];
        Mode::Composite(Composite { width, height, bps, hud: d.hud, map: d.map, metric: app.settings.metric, cells: grid::current_cells(&app.settings, app.focus), back_crop: app.settings.back_crop, gpu: d.gpu })
    } else {
        Mode::Lossless(d.cams)
    };
    let job = Job { event: app.events[i].clone(), segs: p.segs.clone(), from, to, base: base(app, p), mode, csv: d.csv };
    app.settings.export_dir = job.base.parent().map(Path::to_path_buf);
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(AtomicU32::new(0));
    let (tx, rx) = channel();
    let (c, pr) = (cancel.clone(), progress.clone());
    thread::spawn(move || {
        let _ = tx.send(export::run(&job, &c, |f| pr.store(f.to_bits(), Ordering::Relaxed)));
    });
    app.export.running = Some(Running { cancel, progress, rx });
}

fn poll(d: &mut Dialog) {
    let Some(r) = &d.running else { return };
    match r.rx.try_recv() {
        Ok(Err(e)) if e == CANCELLED => d.running = None,
        Ok(result) => {
            d.running = None;
            d.outcome = Some(result);
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            d.running = None;
            d.outcome = Some(Err("the export stopped unexpectedly".to_owned()));
        }
    }
}

pub fn modal(app: &mut App, ctx: &Context) {
    if !app.export.open {
        return;
    }
    poll(&mut app.export);
    let frame = Frame::NONE.fill(BG).stroke(theme::line(LINE2)).inner_margin(Margin::same(22));
    let backdrop = Color32::from_rgba_unmultiplied_const(10, 10, 9, 184);
    let r = Modal::new(Id::unique("export")).frame(frame).backdrop_color(backdrop).show(ctx, |ui| contents(ui, app));
    let running = app.export.running.is_some();
    let close = !running && r.should_close();
    match r.inner {
        Step::Close => close_dialog(&mut app.export),
        _ if close => close_dialog(&mut app.export),
        Step::Pick => pick(app),
        Step::Start => start(app),
        Step::Cancel => {
            if let Some(run) = &app.export.running {
                run.cancel.store(true, Ordering::Relaxed);
            }
        }
        Step::None => {}
    }
    if running {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn close_dialog(d: &mut Dialog) {
    d.open = false;
    d.outcome = None;
}

fn pick(app: &mut App) {
    let Some(p) = &app.player else { return };
    let current = base(app, p);
    let mut dialog = rfd::FileDialog::new().add_filter("MP4 video", &["mp4"]);
    if let Some(dir) = current.parent().filter(|d| d.is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    if let Some(name) = current.file_name() {
        dialog = dialog.set_file_name(name.to_string_lossy());
    }
    if let Some(picked) = dialog.save_file() {
        app.export.dest = Some(base_of(picked));
    }
}

pub(super) fn row(ui: &mut Ui, h: f32) -> Rect {
    ui.allocate_exact_size(vec2(W, h), Sense::hover()).0
}

fn legend(ui: &mut Ui, text: &str) {
    let r = row(ui, 14.0);
    theme::label(ui.painter(), r.left_center(), text, theme::mono(11.0), TEXT3, W);
}

/// The whole `rect` clicks: a 14 px box and its label, as the mockup's checkboxes.
pub(super) fn check(ui: &mut Ui, rect: Rect, text: &str, on: &mut bool) {
    let r = ui.allocate_rect(rect, Sense::click());
    r.widget_info(|| WidgetInfo::selected(Role::CheckBox, true, *on, text));
    if r.clicked() {
        *on = !*on;
    }
    let p = ui.painter();
    let bx = Rect::from_center_size(pos2(rect.min.x + 7.0, rect.center().y), vec2(14.0, 14.0));
    if *on {
        p.rect_filled(bx, 0.0, TEXT);
        p.line(vec![pos2(bx.min.x + 3.0, bx.center().y), pos2(bx.center().x - 1.0, bx.max.y - 4.0), pos2(bx.max.x - 3.0, bx.min.y + 4.0)], Stroke::new(1.75, BG));
    } else {
        p.rect_stroke(bx, 0.0, theme::line(if r.hovered() { TEXT3 } else { LINE2 }), StrokeKind::Inside);
    }
    theme::label(p, pos2(bx.max.x + 8.0, rect.center().y), text, theme::mono(12.0), TEXT, rect.width() - 22.0);
}

/// One of the two output choices: outlined bright with a lifted ground when picked. True on click.
fn mode_card(ui: &mut Ui, title: &str, about: &str, on: bool) -> bool {
    const PAD: f32 = 12.0;
    const RADIO: f32 = 13.0;
    let text_w = W - 2.0 * PAD - RADIO - 10.0;
    let g = ui.painter().layout(about.to_owned(), theme::sans(12.5), TEXT2, text_w);
    let (rect, r) = ui.allocate_exact_size(vec2(W, 2.0 * PAD + 18.0 + 4.0 + g.size().y), Sense::click());
    r.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, on, title));
    let p = ui.painter();
    if on {
        p.rect_filled(rect, 0.0, HOVER);
    }
    p.rect_stroke(rect, 0.0, theme::line(if on { TEXT } else { LINE }), StrokeKind::Inside);
    let dot = pos2(rect.min.x + PAD + RADIO / 2.0, rect.min.y + PAD + 9.0);
    p.circle_stroke(dot, RADIO / 2.0 - 0.5, theme::line(if on { TEXT } else { LINE2 }));
    if on {
        p.circle_filled(dot, 3.5, TEXT);
    }
    let x = rect.min.x + PAD + RADIO + 10.0;
    theme::label(p, pos2(x, dot.y), title, theme::medium(13.0), TEXT, text_w);
    p.galley(pos2(x, rect.min.y + PAD + 22.0), g, TEXT2);
    r.clicked()
}

fn contents(ui: &mut Ui, app: &mut App) -> Step {
    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
    let mut step = Step::None;
    let head = row(ui, BTN_H);
    theme::spaced(ui.painter(), head.left_center(), "EXPORT CLIP", theme::medium(11.0), TEXT3, 1.54, W - 40.0);
    let close = head.with_min_x(head.max.x - BTN_H);
    let running = app.export.running.is_some();
    if theme::button(ui, close, Face::Icon(Icon::Close, 14.0), Look::Bare, !running, "Close").clicked() {
        step = Step::Close;
    }
    ui.add_space(18.0);

    let Some(p) = &app.player else { return Step::Close };
    let (from, to) = app.trim.range(p.duration());
    let r = row(ui, 22.0);
    let painter = ui.painter();
    let times = format!("{} → {}", hms(p.start + from as i64), hms(p.start + to as i64));
    let w = theme::label(painter, r.left_center(), &times, theme::medium(15.0), TEXT, W);
    let detail = format!("{} · {}", span_text(export::footage(&p.segs, from, to)), segments_text(&p.segs, from, to));
    theme::label(painter, pos2(r.min.x + w + 12.0, r.center().y), &detail, theme::mono(11.5), TEXT3, W - w - 130.0);
    painter.text(r.right_center(), Align2::RIGHT_CENTER, "adjust with i / o", theme::mono(11.0), TEXT3);
    ui.add_space(14.0);
    let r = row(ui, 1.0);
    ui.painter().hline(r.x_range(), r.min.y + 0.5, theme::line(LINE));
    ui.add_space(18.0);
    let current_base = base(app, p);
    let cells_empty = grid::current_cells(&app.settings, app.focus).is_empty();

    if let Some(run) = &app.export.running {
        progress(ui, f32::from_bits(run.progress.load(Ordering::Relaxed)));
        ui.add_space(18.0);
        let mut row_ = footer(ui);
        if theme::text_button(ui, row_.right(BTN_W, 0.0), "cancel", true).clicked() {
            step = Step::Cancel;
        }
        return step;
    }
    if let Some(result) = &app.export.outcome {
        finished(ui, result);
        ui.add_space(18.0);
        let mut row_ = footer(ui);
        if theme::text_button(ui, row_.right(BTN_W, 0.0), "close", true).clicked() {
            step = Step::Close;
        }
        return step;
    }

    let d = &mut app.export;
    legend(ui, "output");
    ui.add_space(8.0);
    if mode_card(ui, "original cameras · lossless", "Copies the H.264 frames without re-encoding. One MP4 per camera, starting at the keyframe before In.", !d.composite) {
        d.composite = false;
    }
    ui.add_space(8.0);
    if mode_card(ui, "composited video", "One MP4 of the current camera layout, with the vehicle-data strip and map below the cameras, never over them. Re-encoded.", d.composite) {
        d.composite = true;
    }
    ui.add_space(18.0);
    if d.composite {
        legend(ui, "resolution");
        ui.add_space(6.0);
        let mut r = Row::new(row(ui, BTN_H));
        let labels = RESOLUTIONS.map(|x| x.0);
        let w = theme::toggles_width(ui, &labels, 12.0);
        if let Some(i) = theme::toggles(ui, r.left(w, 24.0), &labels, Some(d.res), 12.0) {
            d.res = i;
        }
        check(ui, r.left(156.0, 18.0), "vehicle-data strip", &mut d.hud);
        check(ui, r.left(60.0, 0.0), "map", &mut d.map);
        // Machines without a GPU encoder don't get the choice: they always encode on the CPU.
        if let Some(name) = export::gpu_encoder() {
            ui.add_space(18.0);
            legend(ui, "encoder");
            ui.add_space(6.0);
            let mut r = Row::new(row(ui, BTN_H));
            let labels = ["gpu", "cpu"];
            let w = theme::toggles_width(ui, &labels, 12.0);
            if let Some(i) = theme::toggles(ui, r.left(w, 18.0), &labels, Some(usize::from(!d.gpu)), 12.0) {
                d.gpu = i == 0;
            }
            let about = if d.gpu { name } else { "slower, leaves the graphics card free" };
            theme::label(ui.painter(), r.rest.left_center(), about, theme::mono(11.0), TEXT3, r.rest.width());
        }
    } else {
        legend(ui, "cameras");
        ui.add_space(10.0);
        // The mockup's order, not `CAMERAS`.
        for (k, rows) in [[0, 1, 4], [5, 2, 3]].into_iter().enumerate() {
            if k > 0 {
                ui.add_space(10.0);
            }
            let mut r = Row::new(row(ui, 16.0));
            for cam in rows {
                check(ui, r.left((W - 20.0) / 3.0, 10.0), CAMERAS[cam], &mut d.cams[cam]);
            }
        }
    }
    ui.add_space(18.0);
    let r = row(ui, 16.0);
    check(ui, r, "also save telemetry.csv", &mut d.csv);
    ui.add_space(18.0);

    let mut r = Row::new(row(ui, BTN_H));
    let word = r.left(44.0, 10.0);
    theme::label(ui.painter(), word.left_center(), "save to", theme::mono(11.0), TEXT3, 44.0);
    if theme::text_button(ui, r.right(64.0, 10.0), "change", true).clicked() {
        step = Step::Pick;
    }
    ui.painter().rect_stroke(r.rest, 0.0, theme::line(LINE), StrokeKind::Inside);
    theme::label(ui.painter(), pos2(r.rest.min.x + 10.0, r.rest.center().y), &current_base.display().to_string(), theme::mono(12.0), TEXT, r.rest.width() - 20.0);
    ui.add_space(18.0);

    let cameras = d.cams.iter().filter(|c| **c).count();
    let ready = if d.composite { !cells_empty } else { cameras > 0 };
    let mut r = footer(ui);
    if theme::button(ui, r.right(BTN_W, 8.0), Face::Text("export"), Look::Primary, ready, "export").clicked() {
        step = Step::Start;
    }
    if theme::text_button(ui, r.right(BTN_W, 8.0), "cancel", true).clicked() {
        step = Step::Close;
    }
    let what = if ready { summary_text(d.composite, cameras, d.csv) } else { "nothing selected".to_owned() };
    theme::label(ui.painter(), r.rest.left_center(), &what, theme::mono(11.0), TEXT3, r.rest.width());
    step
}

/// The rule, 14 px, then the button row.
pub(super) fn footer(ui: &mut Ui) -> Row {
    let r = row(ui, 1.0);
    ui.painter().hline(r.x_range(), r.min.y + 0.5, theme::line(LINE));
    ui.add_space(14.0);
    Row::new(row(ui, BTN_H))
}

pub(super) fn progress(ui: &mut Ui, f: f32) {
    let f = f.clamp(0.0, 1.0);
    let mut r = Row::new(row(ui, 16.0));
    let text = r.right(48.0, 12.0);
    let bar = Rect::from_center_size(r.rest.center(), vec2(r.rest.width(), 8.0));
    let p = ui.painter();
    p.rect_filled(bar.with_max_x(bar.min.x + bar.width() * f), 0.0, TEXT);
    p.rect_stroke(bar, 0.0, theme::line(LINE2), StrokeKind::Inside);
    p.text(text.right_center(), Align2::RIGHT_CENTER, format!("{:.0} %", f * 100.0), theme::mono(12.0), TEXT);
}

fn finished(ui: &mut Ui, result: &Result<Summary, String>) {
    let (title, lines) = match result {
        Ok(s) => {
            let n = s.files.len();
            let mut lines: Vec<String> = s.files.iter().map(|f| f.display().to_string()).collect();
            lines.extend(s.notes.iter().cloned());
            (format!("saved {n} file{}", if n == 1 { "" } else { "s" }), lines)
        }
        Err(e) => ("export failed, nothing was saved".to_owned(), vec![e.clone()]),
    };
    let r = row(ui, 18.0);
    theme::label(ui.painter(), r.left_center(), &title, theme::medium(13.0), TEXT, W);
    for line in lines {
        ui.add_space(6.0);
        let g = ui.painter().layout(line, theme::mono(11.0), TEXT2, W);
        let r = row(ui, g.size().y);
        ui.painter().galley(r.min, g, TEXT2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_rounds_up_and_switches_to_minutes() {
        assert_eq!(span_text(30.0), "30 s");
        assert_eq!(span_text(0.2), "1 s");
        assert_eq!(span_text(59.5), "01:00");
        assert_eq!(span_text(723.0), "12:03");
    }

    #[test]
    fn segments_named_as_in_the_mockup() {
        let s = |start: f64, dur: f64| SegTime { start, dur, frames: vec![] };
        let segs = [s(0.0, 60.0), s(60.0, 60.0), s(120.0, 0.0), s(180.0, 60.0)];
        assert_eq!(segments_text(&segs, 10.0, 20.0), "segment 1");
        assert_eq!(segments_text(&segs, 50.0, 70.0), "segments 1–2");
        assert_eq!(segments_text(&segs, 60.0, 70.0), "segment 2");
        assert_eq!(segments_text(&segs, 50.0, 200.0), "segments 1–4");
        assert_eq!(segments_text(&segs, 130.0, 140.0), "no footage");
    }

    #[test]
    fn the_summary_says_what_gets_written() {
        assert_eq!(summary_text(false, 6, true), "6 × mp4 + telemetry.csv · no re-encode");
        assert_eq!(summary_text(false, 2, false), "2 × mp4 · no re-encode");
        assert_eq!(summary_text(true, 6, true), "1 × mp4 (h.264) + telemetry.csv");
    }

    #[test]
    fn a_picked_name_loses_only_its_mp4() {
        assert_eq!(base_of(PathBuf::from("a/b.MP4")), PathBuf::from("a/b"));
        assert_eq!(base_of(PathBuf::from("a/b.c")), PathBuf::from("a/b.c"));
        assert_eq!(base_of(PathBuf::from("a/b")), PathBuf::from("a/b"));
    }
}
