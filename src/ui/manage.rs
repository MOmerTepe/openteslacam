//! The manage dialog for the open event: pick one thing to do, see what it costs, confirm. Only for
//! events in the library folder; a drive stays as the car wrote it. The worker lives in `crate::manage`.

use super::App;
use super::export::{BTN_H, W, check, footer, row};
use super::theme::{self, BG, Face, Icon, LINE2, Look, Row, TEXT, TEXT2, TEXT3};
use crate::library::CAMERAS;
use crate::manage::{self, Action, Plan};
use eframe::egui::{Color32, Context, Frame, Id, Margin, Modal, Ui};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;

/// Fits `confirm`; design rule 3 gives the others the same width.
const BTN_W: f32 = 104.0;

pub struct Dialog {
    pub open: bool,
    /// Cameras to keep, indexed like `CAMERAS`.
    cams: [bool; 6],
    /// What was picked and what it would do, or why it can't be done.
    pending: Option<(String, Result<Plan, String>)>,
    pub(super) running: Option<Receiver<Result<(), String>>>,
    error: Option<String>,
}

impl Default for Dialog {
    fn default() -> Self {
        Dialog { open: false, cams: [true; 6], pending: None, running: None, error: None }
    }
}

enum Step {
    None,
    Close,
    Pick(&'static str, Action),
    Confirm,
}

/// Whether the open event may be managed: it lies in the library folder (its archive included).
pub fn allowed(app: &App) -> bool {
    let in_library = app.folder.as_ref().zip(app.settings.library_dir.as_ref()).is_some_and(|(f, lib)| f.starts_with(lib));
    // Not while an export or import reads or writes the same files.
    in_library && app.current.is_some() && app.player.is_some() && app.export.running.is_none() && app.import.running.is_none()
}

/// The range trimming keeps, in seconds from the event start, and whether it is In and Out. Those
/// win over the mark: they are the selection on screen, and a mark set earlier at another moment once
/// trimmed away footage that In and Out covered.
pub(super) fn range(app: &App) -> Option<(f64, f64, bool)> {
    let p = app.player.as_ref()?;
    let (a, b) = app.trim.range(p.duration());
    if app.trim.is_set() { Some((a, b, true)) } else { app.mark().map(|(a, b)| (a - p.start as f64, b - p.start as f64, false)) }
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
    if !app.manage.open {
        return;
    }
    let frame = Frame::NONE.fill(BG).stroke(theme::line(LINE2)).inner_margin(Margin::same(22));
    let backdrop = Color32::from_rgba_unmultiplied_const(10, 10, 9, 184);
    let r = Modal::new(Id::unique("manage")).frame(frame).backdrop_color(backdrop).show(ctx, |ui| contents(ui, app));
    let busy = app.manage.running.is_some();
    match r.inner {
        Step::Close => close(&mut app.manage),
        Step::None if !busy && r.should_close() => close(&mut app.manage),
        Step::Pick(name, action) => pick(app, name, &action),
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

/// One choice: its button on the left, what it does beside it.
fn choice(ui: &mut Ui, label: &'static str, about: &str, enabled: bool) -> bool {
    let mut r = Row::new(row(ui, BTN_H));
    let clicked = theme::text_button(ui, r.left(BTN_W, 12.0), label, enabled).clicked();
    theme::label(ui.painter(), r.rest.left_center(), about, theme::sans(12.5), if enabled { TEXT2 } else { TEXT3 }, r.rest.width());
    ui.add_space(8.0);
    clicked
}

fn contents(ui: &mut Ui, app: &mut App) -> Step {
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut step = Step::None;
    let busy = app.manage.running.is_some();
    let head = row(ui, BTN_H);
    theme::spaced(ui.painter(), head.left_center(), "MANAGE EVENT", theme::medium(11.0), TEXT3, 1.54, W - 40.0);
    if theme::button(ui, head.with_min_x(head.max.x - BTN_H), Face::Icon(Icon::Close, 14.0), Look::Bare, !busy, "Close").clicked() {
        step = Step::Close;
    }
    ui.add_space(18.0);
    if busy || app.manage.error.is_some() {
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
    let r = row(ui, 22.0);
    theme::label(ui.painter(), r.left_center(), &super::events::title(ev), theme::medium(15.0), TEXT, W);
    ui.add_space(18.0);

    // The exact range, before anything is picked: what trim goes by must never be a guess.
    let trim = range(app);
    let start = app.player.as_ref().map_or(0.0, |p| p.start as f64);
    let clock = |t: f64| super::events::hms((start + t) as i64);
    let about = match trim {
        Some((a, b, true)) => format!("Keeps In to Out, {} → {}, in whole segments, and removes the rest.", clock(a), clock(b)),
        Some((a, b, false)) if b > a => format!("Keeps the mark, {} → {}, in whole segments, and removes the rest.", clock(a), clock(b)),
        Some((a, ..)) => format!("Keeps the segments around the mark at {} and removes the rest.", clock(a)),
        None => "Set In and Out, or a mark, to say which part to keep.".to_owned(),
    };
    if choice(ui, "trim", &about, trim.is_some()) {
        let (a, b, _) = trim.unwrap_or_default();
        step = Step::Pick("trim", Action::Trim(a, b));
    }
    if choice(ui, "drop cameras", "Removes the cameras that aren't ticked below.", true) {
        step = Step::Pick("drop cameras", Action::Cameras(app.manage.cams));
    }
    let mut r = Row::new(row(ui, 16.0));
    r.left(BTN_W, 12.0);
    for cam in 0..6 {
        check(ui, r.left((W - BTN_W - 12.0) / 6.0, 0.0), &CAMERAS[cam].replace("_repeater", " rep").replace("_pillar", " pil"), &mut app.manage.cams[cam]);
    }
    ui.add_space(14.0);
    if choice(ui, "archive", "Moves the event out of the list, into the library's Archive folder.", true)
        && let Some(lib) = app.settings.library_dir.clone()
    {
        step = Step::Pick("archive", Action::Archive(lib));
    }
    if choice(ui, "delete", "Removes the whole event.", true) {
        step = Step::Pick("delete", Action::Delete);
    }
    ui.add_space(10.0);

    let mut r = footer(ui);
    let ready = matches!(&app.manage.pending, Some((_, Ok(_))));
    if theme::button(ui, r.right(BTN_W, 8.0), Face::Text("confirm"), Look::Primary, ready, "confirm").clicked() {
        step = Step::Confirm;
    }
    if theme::text_button(ui, r.right(BTN_W, 8.0), "cancel", true).clicked() {
        step = Step::Close;
    }
    let what = match &app.manage.pending {
        None => "pick one above".to_owned(),
        Some((name, Err(e))) => format!("{name}: {e}"),
        Some((name, Ok(p))) if p.rename.is_some() => format!("{name}: moves it, nothing is removed"),
        Some((name, Ok(p))) => format!("{name}: {} to the Recycle Bin · keeps {} of {} segments", mb(manage::freed(p)), p.kept, ev.segments.len()),
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
