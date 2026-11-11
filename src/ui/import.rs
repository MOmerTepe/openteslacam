//! The import dialog, and the watch for Tesla drives being plugged in. The worker lives in `crate::import`.

use super::App;
use super::export::{BTN_H, W, check, footer, progress, row};
use super::theme::{self, BG, Face, Icon, LINE, LINE2, Look, Row, TEXT, TEXT2, TEXT3};
use crate::import::{self, CANCELLED, Job, Progress, Summary};
use eframe::egui::{Color32, Context, Frame, Id, Margin, Modal, StrokeKind, Ui, pos2};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;

/// Fits `import` and `cancel`; design rule 3 makes them equal.
const BTN_W: f32 = 72.0;

pub(super) struct Running {
    cancel: Arc<AtomicBool>,
    progress: Arc<Progress>,
    rx: Receiver<Result<Summary, String>>,
    /// Where it's going, to open once it's there.
    lib: PathBuf,
}

#[derive(Default)]
pub struct Dialog {
    pub open: bool,
    /// The drive or TeslaCam folder to import.
    pub(super) src: Option<PathBuf>,
    pub(super) running: Option<Running>,
    /// How the last run ended; cleared on closing.
    pub(super) outcome: Option<Result<Summary, String>>,
}

enum Step {
    None,
    Close,
    Pick,
    Start,
    Cancel,
}

/// TeslaCam folders at the root of the drives present now.
fn tesla_drives() -> Vec<PathBuf> {
    ('D'..='Z').map(|l| PathBuf::from(format!("{l}:\\TeslaCam"))).filter(|p| p.is_dir()).collect()
}

/// Sends each Tesla drive as it appears, the ones present at launch included. Off the UI thread: an
/// empty card reader or a sleeping disk can take seconds to answer.
pub fn watch(ctx: Context) -> Receiver<PathBuf> {
    let (tx, rx) = channel();
    if cfg!(windows) {
        thread::spawn(move || {
            let mut seen = Vec::new();
            loop {
                let now = tesla_drives();
                for d in now.iter().filter(|d| !seen.contains(*d)) {
                    if tx.send(d.clone()).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
                seen = now;
                thread::sleep(Duration::from_secs(3));
            }
        });
    }
    rx
}

/// A drive turned up. Only people who chose a library hear about it: by itself if they asked for
/// that, else as the dialog, and never on top of other work.
pub fn drive_arrived(app: &mut App, drive: PathBuf) {
    let busy = app.import.open || app.import.running.is_some() || app.export.open || app.update_open || app.shortcuts_open || app.onboard.open || app.manage.open || app.manage.running.is_some();
    if busy || app.settings.library_dir.is_none() {
        return;
    }
    app.import.src = Some(drive);
    app.import.open = true;
    if app.settings.import_auto {
        start(app);
    }
}

/// The open folder is what the library panel's `import` offers.
pub fn open_for(app: &mut App) {
    // While one runs, the link shows its progress instead.
    if app.import.running.is_none() {
        app.import.src = app.folder.clone();
        app.import.outcome = None;
    }
    app.import.open = true;
}

pub(super) fn start(app: &mut App) {
    let (Some(src), Some(lib)) = (app.import.src.clone(), app.settings.library_dir.clone()) else { return };
    let job = Job { src, lib: lib.clone(), clear: app.settings.import_clear };
    let (cancel, progress) = (Arc::new(AtomicBool::new(false)), Arc::new(Progress::default()));
    let (tx, rx) = channel();
    let (c, p) = (cancel.clone(), progress.clone());
    thread::spawn(move || {
        let _ = tx.send(import::run(&job, &c, &p));
    });
    app.import.outcome = None;
    app.import.running = Some(Running { cancel, progress, rx, lib });
}

/// Runs every frame: the import goes on with the dialog closed.
fn poll(app: &mut App, ctx: &Context) {
    let Some(r) = &app.import.running else { return };
    let result = match r.rx.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => Err("the import stopped unexpectedly".to_owned()),
    };
    let lib = r.lib.clone();
    app.import.running = None;
    // What got copied is in the library either way; show it unless an export is looking at the open event.
    if result.as_ref().is_ok_and(|s| s.copied + s.already > 0) && !app.export.open {
        app.open_folder(ctx, lib);
    }
    if result.as_ref().is_err_and(|e| e == CANCELLED) {
        return;
    }
    app.import.outcome = Some(result);
    app.import.open = true;
}

pub fn modal(app: &mut App, ctx: &Context) {
    poll(app, ctx);
    if !app.import.open {
        return;
    }
    let frame = Frame::NONE.fill(BG).stroke(theme::line(LINE2)).inner_margin(Margin::same(22));
    let backdrop = Color32::from_rgba_unmultiplied_const(10, 10, 9, 184);
    let r = Modal::new(Id::unique("import")).frame(frame).backdrop_color(backdrop).show(ctx, |ui| contents(ui, app));
    match r.inner {
        Step::Close => close(&mut app.import),
        Step::None if r.should_close() => close(&mut app.import),
        Step::Pick => {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                app.settings.library_dir = Some(p);
            }
        }
        Step::Start => start(app),
        Step::Cancel => {
            if let Some(run) = &app.import.running {
                run.cancel.store(true, Ordering::Relaxed);
            }
        }
        Step::None => {}
    }
    if app.import.running.is_some() {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn close(d: &mut Dialog) {
    d.open = false;
    d.outcome = None;
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

pub(super) fn path_row(ui: &mut Ui, word: &str, path: Option<&Path>, change: Option<&str>) -> bool {
    let mut r = Row::new(row(ui, BTN_H));
    let w = r.left(52.0, 10.0);
    theme::label(ui.painter(), w.left_center(), word, theme::mono(11.0), TEXT3, 52.0);
    let clicked = change.is_some_and(|label| theme::text_button(ui, r.right(64.0, 10.0), label, true).clicked());
    ui.painter().rect_stroke(r.rest, 0.0, theme::line(LINE), StrokeKind::Inside);
    let text = path.map_or("not chosen yet".to_owned(), |p| p.display().to_string());
    theme::label(ui.painter(), pos2(r.rest.min.x + 10.0, r.rest.center().y), &text, theme::mono(12.0), if path.is_some() { TEXT } else { TEXT3 }, r.rest.width() - 20.0);
    clicked
}

fn lines(ui: &mut Ui, title: &str, lines: &[String]) {
    let r = row(ui, 18.0);
    theme::label(ui.painter(), r.left_center(), title, theme::medium(13.0), TEXT, W);
    for line in lines {
        ui.add_space(6.0);
        let g = ui.painter().layout(line.clone(), theme::mono(11.0), TEXT2, W);
        let r = row(ui, g.size().y);
        ui.painter().galley(r.min, g, TEXT2);
    }
}

fn contents(ui: &mut Ui, app: &mut App) -> Step {
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut step = Step::None;
    let head = row(ui, BTN_H);
    theme::spaced(ui.painter(), head.left_center(), "IMPORT TO THIS PC", theme::medium(11.0), TEXT3, 1.54, W - 40.0);
    // Closing doesn't stop a running import; the library panel says it's going.
    if theme::button(ui, head.with_min_x(head.max.x - BTN_H), Face::Icon(Icon::Close, 14.0), Look::Bare, true, "Close").clicked() {
        step = Step::Close;
    }
    ui.add_space(18.0);

    if let Some(run) = &app.import.running {
        let n = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        let p = &run.progress;
        progress(ui, n(&p.bytes) as f32 / n(&p.total_bytes).max(1) as f32);
        ui.add_space(18.0);
        let mut r = footer(ui);
        if theme::text_button(ui, r.right(BTN_W, 8.0), "cancel", true).clicked() {
            step = Step::Cancel;
        }
        let what = format!("{} / {} files · {} / {}", n(&p.files), n(&p.total_files), gb(n(&p.bytes)), gb(n(&p.total_bytes)));
        theme::label(ui.painter(), r.rest.left_center(), &what, theme::mono(11.0), TEXT3, r.rest.width());
        return step;
    }
    if let Some(result) = &app.import.outcome {
        match result {
            Ok(s) => {
                let mut l = vec![format!("{} copied and checked · {} were already here", s.copied, s.already)];
                if s.cleared > 0 {
                    l.push(format!("{} cleared from the drive", s.cleared));
                }
                // A flaky drive can fail on hundreds of files; the first few say what's going on.
                l.extend(s.notes.iter().take(6).cloned());
                if s.notes.len() > 6 {
                    l.push(format!("and {} more files that couldn't be read", s.notes.len() - 6));
                }
                lines(ui, "import finished", &l);
            }
            Err(e) => lines(ui, "import stopped; what was copied so far is kept", std::slice::from_ref(e)),
        }
        ui.add_space(18.0);
        let mut r = footer(ui);
        if theme::text_button(ui, r.right(BTN_W, 8.0), "close", true).clicked() {
            step = Step::Close;
        }
        return step;
    }

    path_row(ui, "from", app.import.src.as_deref(), None);
    ui.add_space(8.0);
    let chosen = app.settings.library_dir.is_some();
    if path_row(ui, "to", app.settings.library_dir.as_deref(), Some(if chosen { "change" } else { "choose" })) {
        step = Step::Pick;
    }
    ui.add_space(18.0);
    let r = row(ui, 16.0);
    check(ui, r, "clear each file from the drive once its copy is checked", &mut app.settings.import_clear);
    ui.add_space(10.0);
    let r = row(ui, 16.0);
    check(ui, r, "import by itself when a Tesla drive is plugged in", &mut app.settings.import_auto);
    ui.add_space(18.0);

    let ready = chosen && app.import.src.is_some();
    let mut r = footer(ui);
    if theme::button(ui, r.right(BTN_W, 8.0), Face::Text("import"), Look::Primary, ready, "import").clicked() {
        step = Step::Start;
    }
    if theme::text_button(ui, r.right(BTN_W, 8.0), "cancel", true).clicked() {
        step = Step::Close;
    }
    let what = match (ready, app.settings.import_clear) {
        (false, _) => "choose where the library goes",
        (true, true) => "copies, checks, then clears the drive",
        (true, false) => "copies and checks · the drive stays as it is",
    };
    theme::label(ui.painter(), r.rest.left_center(), what, theme::mono(11.0), TEXT3, r.rest.width());
    step
}

/// "importing 41 %" for the library panel while the dialog is closed.
pub fn status(app: &App) -> Option<String> {
    let p = &app.import.running.as_ref()?.progress;
    let n = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    Some(format!("importing {:.0} %", n(&p.bytes) as f64 / n(&p.total_bytes).max(1) as f64 * 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_gigabytes() {
        assert_eq!(gb(0), "0.0 GB");
        assert_eq!(gb(45_250_000_000), "45.2 GB");
    }
}
