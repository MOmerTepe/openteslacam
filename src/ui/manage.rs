//! Changing library events on disk. A right-click on an event in the list opens a menu to archive or
//! delete it; `trim` and `drop cameras` in the transport open a dialog that says what it costs before
//! anything goes. Only for events in the library folder; a drive stays as the car wrote it. The worker
//! lives in `crate::manage`.

use super::App;
use super::export::{BTN_H, W, check, footer, row};
use super::theme::{self, BG, Face, Icon, LINE2, Look, Row, TEXT, TEXT2, TEXT3};
use crate::library::{CAMERAS, Event};
use crate::manage::{self, Action, Plan};
use eframe::egui::{Area, Color32, Context, Frame, Id, Key, Margin, Modal, Order, Pos2, Sense, Ui, pos2, vec2};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;

/// Fits `confirm`; design rule 3 gives the others the same width.
const BTN_W: f32 = 104.0;
const MENU_W: f32 = 120.0;

/// What the dialog is about.
#[derive(Clone, Copy, PartialEq)]
pub enum What {
    Trim,
    Cameras,
}

pub struct Dialog {
    pub open: bool,
    what: What,
    /// Cameras to keep, indexed like `CAMERAS`.
    cams: [bool; 6],
    /// What would be done, or why it can't be.
    pending: Option<(String, Result<Plan, String>)>,
    pub(super) running: Option<Receiver<Result<(), String>>>,
    error: Option<String>,
    /// The right-clicked event of the list: its menu at this point, or with no point the question
    /// before deleting.
    quick: Option<(Event, Option<Pos2>)>,
}

impl Default for Dialog {
    fn default() -> Self {
        Dialog { open: false, what: What::Trim, cams: [true; 6], pending: None, running: None, error: None, quick: None }
    }
}

enum Step {
    None,
    Close,
    Confirm,
}

/// Whether the list shows the library folder (its archive included) and nothing else is at its files.
fn in_library(app: &App) -> bool {
    let in_library = app.folder.as_ref().zip(app.settings.library_dir.as_ref()).is_some_and(|(f, lib)| f.starts_with(lib));
    // Not while an export or import reads or writes the same files.
    in_library && app.export.running.is_none() && app.import.running.is_none()
}

/// Whether the open event may be trimmed or lose cameras.
pub fn allowed(app: &App) -> bool {
    in_library(app) && app.current.is_some() && app.player.is_some()
}

pub fn open(app: &mut App, what: What) {
    (app.manage.what, app.manage.open) = (what, true);
}

/// A right-click at `at` on event `i` of the list: opens the archive / delete menu, for library events only.
pub fn ask(app: &mut App, i: usize, at: Pos2) {
    if in_library(app) && app.manage.running.is_none() && !app.manage.open {
        app.manage.quick = Some((app.events[i].clone(), Some(at)));
    }
}

/// `delete` in the menu: the menu gives way to the question.
pub(super) fn ask_delete(app: &mut App) {
    if let Some(q) = &mut app.manage.quick {
        q.1 = None;
    }
}

/// Archives or deletes the right-clicked event. A refusal shows in the dialog's error view.
pub(super) fn quick_run(app: &mut App, action: &Action) {
    let Some((ev, _)) = app.manage.quick.take() else { return };
    // Neither action looks at segment times.
    match manage::plan(&ev, &[], action) {
        Ok(plan) => {
            app.manage.pending = Some((String::new(), Ok(plan)));
            confirm(app);
        }
        Err(e) => (app.manage.error, app.manage.open) = (Some(e), true),
    }
}

/// The range trimming keeps, in seconds from the event start, and whether it is In and Out. Those
/// win over the mark: they are the selection on screen, and a mark set earlier at another moment once
/// trimmed away footage that In and Out covered.
pub(super) fn range(app: &App) -> Option<(f64, f64, bool)> {
    let p = app.player.as_ref()?;
    let (a, b) = app.trim.range(p.duration());
    if app.trim.is_set() { Some((a, b, true)) } else { app.mark().map(|(a, b)| (a - p.start as f64, b - p.start as f64, false)) }
}

/// The menu under the pointer. It stays inside the library panel: nothing but a dialog may cover video.
fn menu(app: &mut App, ctx: &Context, ev: &Event, at: Pos2) {
    let archive = app.settings.library_dir.clone().filter(|lib| manage::plan(ev, &[], &Action::Archive(lib.clone())).is_ok());
    let at = pos2(at.x.min(super::events::WIDTH - MENU_W - 2.0).max(0.0), at.y.min(ctx.content_rect().max.y - 2.0 * BTN_H - 2.0));
    let area = Area::new(Id::unique("event-menu")).order(Order::Foreground).fixed_pos(at).show(ctx, |ui| {
        let frame = Frame::NONE.fill(BG).stroke(theme::line(LINE2)).inner_margin(Margin::same(1));
        let inner = frame.show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let item = |ui: &mut Ui, label: &str, on: bool| {
                let (r, _) = ui.allocate_exact_size(vec2(MENU_W, BTN_H), Sense::hover());
                theme::button(ui, r, Face::Text(label), Look::Bare, on, label).clicked()
            };
            (item(ui, "archive", archive.is_some()), item(ui, "delete", true))
        });
        inner.inner
    });
    let rect = area.response.rect;
    let away = ctx.input(|i| i.key_pressed(Key::Escape) || i.pointer.any_pressed() && i.pointer.interact_pos().is_none_or(|p| !rect.contains(p)));
    match (area.inner, archive) {
        ((true, _), Some(lib)) => quick_run(app, &Action::Archive(lib)),
        ((_, true), _) => ask_delete(app),
        _ if away => app.manage.quick = None,
        _ => {}
    }
}

/// Delete goes to the Recycle Bin, but it still asks.
fn sure(ui: &mut Ui, ev: &Event) -> Step {
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut step = Step::None;
    let head = row(ui, BTN_H);
    theme::spaced(ui.painter(), head.left_center(), "DELETE EVENT", theme::medium(11.0), TEXT3, 1.54, W - 40.0);
    if theme::button(ui, head.with_min_x(head.max.x - BTN_H), Face::Icon(Icon::Close, 14.0), Look::Bare, true, "Close").clicked() {
        step = Step::Close;
    }
    ui.add_space(18.0);
    let r = row(ui, 22.0);
    theme::label(ui.painter(), r.left_center(), &super::events::title(ev), theme::medium(15.0), TEXT, W);
    ui.add_space(18.0);
    let size = mb(manage::freed(&manage::plan(ev, &[], &Action::Delete).unwrap_or_default()));
    let g = ui.painter().layout(format!("Delete this whole event? {size} goes to the Recycle Bin."), theme::sans(12.5), TEXT2, W);
    let r = row(ui, g.size().y);
    ui.painter().galley(r.min, g, TEXT2);
    ui.add_space(18.0);
    let mut r = footer(ui);
    if theme::button(ui, r.right(BTN_W, 8.0), Face::Text("delete"), Look::Danger, true, "delete").clicked() {
        step = Step::Confirm;
    }
    if theme::text_button(ui, r.right(BTN_W, 8.0), "cancel", true).clicked() {
        step = Step::Close;
    }
    step
}

pub fn modal(app: &mut App, ctx: &Context) {
    if let Some(rx) = &app.manage.running {
        match rx.try_recv() {
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
            result => {
                app.manage.running = None;
                app.manage.error = result.unwrap_or_else(|_| Err("the work stopped unexpectedly".to_owned())).err();
                app.manage.open = app.manage.error.is_some();
                // The folder changed under the list either way.
                if let Some(f) = app.folder.clone() {
                    app.open_folder(ctx, f);
                }
            }
        }
    }
    let frame = Frame::NONE.fill(BG).stroke(theme::line(LINE2)).inner_margin(Margin::same(22));
    let backdrop = Color32::from_rgba_unmultiplied_const(10, 10, 9, 184);
    match app.manage.quick.clone() {
        Some((ev, Some(at))) => return menu(app, ctx, &ev, at),
        Some((ev, None)) => {
            let r = Modal::new(Id::unique("manage-sure")).frame(frame).backdrop_color(backdrop).show(ctx, |ui| sure(ui, &ev));
            match r.inner {
                Step::Confirm => quick_run(app, &Action::Delete),
                Step::Close => app.manage.quick = None,
                Step::None if r.should_close() => app.manage.quick = None,
                Step::None => {}
            }
            return;
        }
        None => {}
    }
    if !app.manage.open {
        return;
    }
    let r = Modal::new(Id::unique("manage")).frame(frame).backdrop_color(backdrop).show(ctx, |ui| contents(ui, app));
    let busy = app.manage.running.is_some();
    match r.inner {
        Step::Close => close(&mut app.manage),
        Step::None if !busy && r.should_close() => close(&mut app.manage),
        Step::Confirm => confirm(app),
        Step::None => {}
    }
}

/// Works out what `action` would do to the open event; nothing happens until `confirm`.
pub(super) fn pick(app: &mut App, name: &str, action: &Action) {
    let plan = app.current.zip(app.player.as_ref()).map(|(i, p)| manage::plan(&app.events[i], &p.segs, action));
    app.manage.pending = plan.map(|p| (name.to_owned(), p));
}

pub(super) fn confirm(app: &mut App) {
    let Some((_, Ok(plan))) = app.manage.pending.take() else { return };
    // The decoders and the telemetry reader hold the event's files open.
    app.close_event();
    let (tx, rx) = channel();
    // Tests delete for good: their files don't belong in the user's Recycle Bin.
    thread::spawn(move || drop(tx.send(manage::apply(&plan, !cfg!(test)))));
    app.manage.running = Some(rx);
}

fn close(d: &mut Dialog) {
    (d.open, d.pending, d.error) = (false, None, None);
}

fn mb(bytes: u64) -> String {
    if bytes >= 1_000_000_000 { format!("{:.1} GB", bytes as f64 / 1e9) } else { format!("{:.0} MB", bytes as f64 / 1e6) }
}

fn contents(ui: &mut Ui, app: &mut App) -> Step {
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut step = Step::None;
    let busy = app.manage.running.is_some();
    let stopped = busy || app.manage.error.is_some();
    let title = match app.manage.what {
        _ if stopped => "LIBRARY",
        What::Trim => "TRIM EVENT",
        What::Cameras => "DROP CAMERAS",
    };
    let head = row(ui, BTN_H);
    theme::spaced(ui.painter(), head.left_center(), title, theme::medium(11.0), TEXT3, 1.54, W - 40.0);
    if theme::button(ui, head.with_min_x(head.max.x - BTN_H), Face::Icon(Icon::Close, 14.0), Look::Bare, !busy, "Close").clicked() {
        step = Step::Close;
    }
    ui.add_space(18.0);
    if stopped {
        let text = app.manage.error.clone().unwrap_or_else(|| "working…".to_owned());
        let g = ui.painter().layout(text, theme::mono(11.5), TEXT, W);
        let r = row(ui, g.size().y);
        ui.painter().galley(r.min, g, TEXT);
        ui.add_space(18.0);
        let mut r = footer(ui);
        if !busy && theme::text_button(ui, r.right(BTN_W, 8.0), "close", true).clicked() {
            step = Step::Close;
        }
        return step;
    }
    let Some(ev) = app.current.map(|i| &app.events[i]) else { return Step::Close };
    let segments = ev.segments.len();
    let r = row(ui, 22.0);
    theme::label(ui.painter(), r.left_center(), &super::events::title(ev), theme::medium(15.0), TEXT, W);
    ui.add_space(18.0);

    // The exact range, before anything goes: what trim goes by must never be a guess.
    let trim = range(app);
    let start = app.player.as_ref().map_or(0.0, |p| p.start as f64);
    let clock = |t: f64| super::events::hms((start + t) as i64);
    let (name, about, action) = match (app.manage.what, trim) {
        (What::Cameras, _) => ("drop cameras", "Removes the cameras that aren't ticked.".to_owned(), Some(Action::Cameras(app.manage.cams))),
        (_, Some((a, b, true))) => ("trim", format!("Keeps In to Out, {} → {}, and removes the rest. Whole segments are kept, so a little more may stay.", clock(a), clock(b)), Some(Action::Trim(a, b))),
        (_, Some((a, b, false))) if b > a => ("trim", format!("Keeps the mark, {} → {}, and removes the rest. Whole segments are kept, so a little more may stay.", clock(a), clock(b)), Some(Action::Trim(a, b))),
        (_, Some((a, b, _))) => ("trim", format!("Keeps the segments around the mark at {} and removes the rest.", clock(a)), Some(Action::Trim(a, b))),
        (_, None) => ("trim", "Set In and Out, or a mark, to say which part to keep.".to_owned(), None),
    };
    // The text wraps and the row grows with it: it says what will be removed, so none of it may be cut off.
    let g = ui.painter().layout(about, theme::sans(12.5), TEXT2, W);
    let r = row(ui, g.size().y);
    ui.painter().galley(r.min, g, TEXT2);
    ui.add_space(12.0);
    if app.manage.what == What::Cameras {
        // Three to a row, as in the export dialog: six across cut the names down to "left …" twice.
        for cams in [0..3, 3..6] {
            let mut r = Row::new(row(ui, 16.0));
            for cam in cams {
                check(ui, r.left(W / 3.0, 0.0), CAMERAS[cam], &mut app.manage.cams[cam]);
            }
            ui.add_space(8.0);
        }
    }
    ui.add_space(6.0);
    match action {
        Some(a) => pick(app, name, &a),
        None => app.manage.pending = None,
    }

    let mut r = footer(ui);
    let ready = matches!(&app.manage.pending, Some((_, Ok(_))));
    if theme::button(ui, r.right(BTN_W, 8.0), Face::Text("confirm"), Look::Primary, ready, "confirm").clicked() {
        step = Step::Confirm;
    }
    if theme::text_button(ui, r.right(BTN_W, 8.0), "cancel", true).clicked() {
        step = Step::Close;
    }
    let what = match &app.manage.pending {
        None => String::new(),
        Some((name, Err(e))) => format!("{name}: {e}"),
        Some((name, Ok(p))) => format!("{name}: {} to the Recycle Bin · keeps {} of {segments} segments", mb(manage::freed(p)), p.kept),
    };
    theme::label(ui.painter(), r.rest.left_center(), &what, theme::mono(11.0), TEXT3, r.rest.width());
    step
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_switch_to_gigabytes() {
        assert_eq!(mb(78_000_000), "78 MB");
        assert_eq!(mb(4_260_000_000), "4.3 GB");
    }
}
