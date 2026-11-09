//! The setup window, in the viewer's design language: the same theme module, monochrome, hard edges,
//! same-size buttons, a filled primary action.

use crate::install::{self, Check, Event, Options, Out, Task};
use crate::theme::{self, Face, Look, Row, TEXT, TEXT2, TEXT3, line, medium, mono, sans};
use eframe::egui::{self, Align2, Color32, FontId, Painter, Pos2, Rect, Response, Role, ScrollArea, Sense, TextEdit, Ui, ViewportCommand, WidgetInfo, pos2, vec2};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::thread;

const WIDTH: f32 = 760.0;
const HEIGHT: f32 = 520.0;
const PAD: f32 = 24.0;
const HEADER_H: f32 = 56.0;
const FOOTER_H: f32 = 64.0;
const BTN_H: f32 = 32.0;

pub type Payload = Option<(Arc<Vec<u8>>, Arc<Vec<u8>>)>;

pub enum Mode {
    Install(Payload),
    Uninstall(PathBuf),
}

pub fn run(mode: Mode) -> Result<(), String> {
    let title = match mode {
        Mode::Install(_) => "OpenTeslaCam Setup",
        Mode::Uninstall(_) => "Uninstall OpenTeslaCam",
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_app_id("openteslacam-setup").with_inner_size([WIDTH, HEIGHT]).with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(App::new(mode, cc.egui_ctx.clone())))
        }),
    )
    .map_err(|e| e.to_string())
}

/// For when there's no window to show the error in, such as a machine with no working graphics driver.
pub fn message_box(text: &str) {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let _ = std::process::Command::new(Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-Command"])
        .arg("Add-Type -AssemblyName System.Windows.Forms;[void][System.Windows.Forms.MessageBox]::Show($env:OTC_MESSAGE,'OpenTeslaCam Setup')")
        .env("OTC_MESSAGE", text)
        .status();
}

#[derive(Clone, PartialEq)]
enum State {
    Waiting,
    Running,
    Done,
    Failed(String),
    Skipped(String),
}

enum Page {
    Options,
    Confirm,
    Progress,
}

struct App {
    ctx: egui::Context,
    uninstall_dir: Option<PathBuf>,
    payload: Payload,
    page: Page,
    dir: String,
    desktop: bool,
    tesla: bool,
    /// What's installed already, found through Windows' installed-apps entry.
    installed: Option<install::Installed>,
    checks: Option<Result<Vec<Check>, String>>,
    checks_rx: Option<Receiver<Result<Vec<Check>, String>>>,
    tasks: Vec<(Task, State)>,
    log: Vec<String>,
    events: Option<Receiver<Event>>,
    finished: bool,
    close_warned: bool,
}

impl App {
    fn new(mode: Mode, ctx: egui::Context) -> App {
        let installed = matches!(mode, Mode::Install(_)).then(install::installed).flatten();
        let (uninstall_dir, payload, page, dir) = match mode {
            // An update goes where the app already is, whatever folder that was.
            Mode::Install(payload) => (None, payload, Page::Options, installed.as_ref().map_or_else(install::default_dir, |i| i.dir.display().to_string())),
            Mode::Uninstall(dir) => {
                let shown = dir.display().to_string();
                (Some(dir), None, Page::Confirm, shown)
            }
        };
        let mut app = App {
            ctx,
            uninstall_dir,
            payload,
            page,
            dir,
            // An update leaves the desktop as it was: a shortcut there stays, and none is added.
            desktop: installed.is_none(),
            tesla: false,
            installed,
            checks: None,
            checks_rx: None,
            tasks: Vec::new(),
            log: Vec::new(),
            events: None,
            finished: false,
            close_warned: false,
        };
        if app.uninstall_dir.is_none() {
            app.recheck();
        }
        app
    }

    fn recheck(&mut self) {
        // Detection only needs a folder to look in; an unfinished path simply has no Tesla tools.
        let dir = install::validate_dir(&self.dir).unwrap_or_else(|_| PathBuf::from(install::default_dir()));
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(install::detect(&dir));
            ctx.request_repaint();
        });
        self.checks_rx = Some(rx);
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.checks_rx
            && let Ok(result) = rx.try_recv()
        {
            // A Tesla checkbox can't stay ticked for a folder that already has the tools.
            if let Ok(c) = &result
                && install::find(c, "tesla").is_some_and(Check::ok)
            {
                self.tesla = false;
            }
            self.checks = Some(result);
            self.checks_rx = None;
        }
        let Some(rx) = &self.events else { return };
        while let Ok(e) = rx.try_recv() {
            match e {
                Event::Start(t) => set_state(&mut self.tasks, t, State::Running),
                Event::Done(t) => set_state(&mut self.tasks, t, State::Done),
                Event::Failed(t, why) => {
                    self.log.push(format!("{}: {why}", t.label()));
                    set_state(&mut self.tasks, t, State::Failed(why));
                }
                Event::Skipped(t, why) => set_state(&mut self.tasks, t, State::Skipped(why)),
                Event::Log(l) => self.log.push(l),
                Event::Finished => self.finished = true,
            }
        }
    }

    fn check(&self, id: &str) -> Option<&Check> {
        self.checks.as_ref()?.as_ref().ok().and_then(|c| install::find(c, id))
    }

    fn sink(&self) -> (Out, Receiver<Event>) {
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        let out: Out = Arc::new(move |e| {
            let _ = tx.send(e);
            ctx.request_repaint();
        });
        (out, rx)
    }

    fn start_install(&mut self, dir: PathBuf) {
        let Some((stub, app)) = self.payload.clone() else { return };
        let checks = self.checks.as_ref().and_then(|c| c.as_ref().ok()).map_or(&[][..], |c| &c[..]);
        let options = Options {
            desktop: self.desktop,
            tesla: self.tesla,
            git: install::found_path(checks, "git"),
            winget: install::found_path(checks, "winget"),
            dir,
            app,
            stub,
        };
        self.tasks = install::plan(options.tesla, options.git.is_some()).into_iter().map(|t| (t, State::Waiting)).collect();
        let (out, rx) = self.sink();
        self.events = Some(rx);
        self.begin_progress();
        thread::spawn(move || install::run_install(options, out));
    }

    fn start_uninstall(&mut self) {
        let Some(dir) = self.uninstall_dir.clone() else { return };
        self.tasks = vec![(Task::Remove, State::Waiting)];
        let (out, rx) = self.sink();
        self.events = Some(rx);
        self.begin_progress();
        thread::spawn(move || install::run_uninstall(&dir, out));
    }

    fn begin_progress(&mut self) {
        self.log.clear();
        self.finished = false;
        self.close_warned = false;
        self.page = Page::Progress;
    }

    fn ok(&self) -> bool {
        self.tasks.iter().all(|(_, s)| !matches!(s, State::Failed(_) | State::Skipped(_)))
    }

    fn header(&self, ui: &Ui, full: Rect) {
        let p = ui.painter();
        let mid = full.min.y + HEADER_H / 2.0;
        let x = full.min.x + PAD;
        let w = theme::label(p, pos2(x, mid), "openteslacam", medium(15.0), TEXT, 200.0);
        let what = if self.uninstall_dir.is_some() { "uninstall" } else { concat!("setup ", env!("CARGO_PKG_VERSION")) };
        theme::label(p, pos2(x + w + 12.0, mid), what, mono(12.0), TEXT3, 120.0);
        let steps: &[&str] = if self.uninstall_dir.is_some() { &["confirm", "remove"] } else { &["options", "install", "done"] };
        let active = match self.page {
            Page::Options | Page::Confirm => 0,
            Page::Progress if self.finished && self.uninstall_dir.is_none() => 2,
            Page::Progress => 1,
        };
        let mut right = full.max.x - PAD;
        for (i, s) in steps.iter().enumerate().rev() {
            let width = theme::text_width(ui, s, mono(11.5));
            let cell = Rect::from_min_max(pos2(right - width, mid - 10.0), pos2(right, mid + 10.0));
            p.text(cell.center(), Align2::CENTER_CENTER, s, mono(11.5), if i == active { TEXT } else { theme::OFF });
            if i == active {
                p.hline(cell.x_range(), cell.max.y - 0.5, line(TEXT));
            }
            right -= width + 12.0;
        }
        p.hline(full.x_range(), full.min.y + HEADER_H - 0.5, line(theme::LINE));
    }

    fn options_page(&mut self, ui: &mut Ui, full: Rect) {
        let left = full.min.x + PAD;
        let right = full.max.x - PAD;
        let mut y = full.min.y + HEADER_H + 20.0;

        caption(ui, "Install location", left, y);
        y += 22.0;
        let mut row = Row::new(Rect::from_min_size(pos2(left, y), vec2(right - left, BTN_H)));
        if theme::text_button(ui, row.right(72.0, 8.0), "browse", true).clicked()
            && let Some(folder) = rfd::FileDialog::new().set_title("Choose the install folder").pick_folder()
        {
            self.dir = folder.display().to_string();
            self.recheck();
        }
        let field = row.rest;
        let edit = ui.put(field, TextEdit::singleline(&mut self.dir).font(mono(12.0)).desired_width(field.width()).margin(vec2(8.0, 8.0)));
        if edit.lost_focus() {
            self.recheck();
        }
        let dir = install::validate_dir(&self.dir);
        y += BTN_H + 10.0;
        checkbox(ui, Rect::from_min_size(pos2(left, y), vec2(14.0 + 8.0 + 160.0, 22.0)), &mut self.desktop, true, "desktop shortcut");
        theme::label(ui.painter(), pos2(left + 22.0, y + 11.0), "desktop shortcut", mono(12.0), TEXT, 200.0);
        y += 22.0 + 18.0;

        let rect = Rect::from_min_size(pos2(right - 72.0, y - 10.0), vec2(72.0, BTN_H));
        if theme::text_button(ui, rect, "re-check", self.checks_rx.is_none()).clicked() {
            self.recheck();
        }
        caption(ui, "Required", left, y);
        y += 22.0;
        let windows_fails = self.check("windows").is_some_and(|c| !c.ok());
        match (&self.checks, self.check("windows")) {
            (_, Some(c)) => status_row(ui, left, right, &mut y, "windows", &c.detail, if c.ok() { Mark::Ok } else { Mark::Bad("fail") }),
            (Some(Err(e)), _) => status_row(ui, left, right, &mut y, "windows", &format!("couldn't run the checks: {e}"), Mark::Bad("unknown")),
            _ => status_row(ui, left, right, &mut y, "windows", "checking", Mark::Waiting),
        }
        y += 8.0;
        y += text_block(ui.painter(), pos2(left, y), "Nothing else is needed to play clips: the video decoder and the vehicle-data parser are built in.", sans(12.5), TEXT3, right - left);
        y += 18.0;

        caption(ui, "Optional", left, y);
        y += 22.0;
        self.tesla_rows(ui, left, right, &mut y, dir.as_ref().ok());

        let foot = footer(ui, full);
        let mut row = Row::new(foot);
        let reinstall = dir.as_ref().is_ok_and(|d| d.join(install::APP_EXE).exists());
        let can_install = self.payload.is_some() && dir.is_ok() && self.checks_rx.is_none() && self.checks.is_some() && !windows_fails;
        // The installed one, if that's the folder being installed to and it's another version.
        let from = self.installed.as_ref().filter(|i| reinstall && dir.as_ref().is_ok_and(|d| *d == i.dir) && i.version != env!("CARGO_PKG_VERSION"));
        let word = if from.is_some() {
            "update"
        } else if reinstall {
            "reinstall"
        } else {
            "install"
        };
        if theme::button(ui, row.right(92.0, 8.0), Face::Text(word), Look::Primary, can_install, word).clicked() {
            if let Ok(d) = dir.clone() {
                self.start_install(d);
            }
            return;
        }
        if theme::text_button(ui, row.right(72.0, 16.0), "cancel", true).clicked() {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
        let hint = if self.payload.is_none() {
            "this exe has no app inside it; build it with scripts/build-installer.ps1".to_string()
        } else if let Err(e) = &dir {
            e.clone()
        } else if windows_fails {
            "this version of Windows isn't supported".to_string()
        } else if let Some(i) = from {
            format!("updates {} to {}; settings and saved layouts stay", i.version, env!("CARGO_PKG_VERSION"))
        } else if reinstall {
            "replaces the installed app; settings and saved layouts stay".to_string()
        } else {
            "installs for your user only, no administrator rights needed".to_string()
        };
        theme::label(ui.painter(), pos2(row.rest.min.x, row.rest.center().y), &hint, sans(12.5), TEXT3, row.rest.width());
    }

    fn tesla_rows(&mut self, ui: &mut Ui, left: f32, right: f32, y: &mut f32, dir: Option<&PathBuf>) {
        let git_ok = self.check("git").map(Check::ok);
        let winget_ok = self.check("winget").is_some_and(Check::ok);
        let already = self.check("tesla").is_some_and(Check::ok);
        let can_fetch = !already && (git_ok == Some(true) || (git_ok == Some(false) && winget_ok));
        if !can_fetch {
            self.tesla = false;
        }
        let top = *y;
        let row = Rect::from_min_size(pos2(left, top), vec2(right - left, 62.0));
        checkbox(ui, row, &mut self.tesla, can_fetch, "fetch Tesla's dashcam tools");
        let p = ui.painter();
        let x = left + 22.0;
        let width = right - x;
        let title_color = if can_fetch || already { TEXT } else { TEXT3 };
        theme::label(p, pos2(x, top + 11.0), "tesla dashcam tools", medium(12.0), title_color, width - 60.0);
        if already {
            mark(p, pos2(right, top + 11.0), Mark::Ok);
        }
        let dest = dir.map_or("the install folder".to_string(), |d| install::tesla_dir(d).display().to_string());
        theme::label(p, pos2(x, top + 29.0), &format!("github.com/teslamotors/dashcam  →  {dest}"), mono(11.0), TEXT2, width);
        theme::label(p, pos2(x, top + 47.0), "Tesla's reference and test tools, fetched from Tesla. Not needed to play clips.", sans(12.5), TEXT3, width);
        *y = row.max.y + 8.0;
        p.hline(left..=right, *y - 0.5, line(theme::LINE));

        let needs_git = self.tesla && git_ok == Some(false);
        let (detail, status) = match (self.check("git"), winget_ok) {
            (None, _) => (if self.checks.is_some() { "unknown".to_string() } else { "checking".to_string() }, Mark::Waiting),
            (Some(c), _) if c.ok() => (c.detail.clone(), Mark::Ok),
            (Some(_), true) if needs_git => ("not found. Installed first with winget (Git.Git), which accepts its agreements. Windows may ask for permission.".to_string(), Mark::Bad("missing")),
            (Some(_), true) => ("not found. Only needed for the tools above; setup offers it when you tick them.".to_string(), Mark::Dim("missing")),
            (Some(_), false) => ("not found, and winget isn't available to install it. Install git, then re-check.".to_string(), Mark::Bad("missing")),
        };
        let git_missing_no_winget = git_ok == Some(false) && !winget_ok;
        let row_right = if git_missing_no_winget { right - 80.0 } else { right };
        if git_missing_no_winget
            && theme::text_button(ui, Rect::from_min_size(pos2(right - 72.0, *y - 1.0), vec2(72.0, BTN_H)), "get git", true).clicked()
        {
            ui.ctx().open_url(egui::OpenUrl::new_tab(install::GIT_DOWNLOAD));
        }
        status_row(ui, left, row_right, y, "git", &detail, status);
    }

    fn confirm_page(&mut self, ui: &mut Ui, full: Rect) {
        let left = full.min.x + PAD;
        let right = full.max.x - PAD;
        let mut y = full.min.y + HEADER_H + 20.0;
        caption(ui, "Remove", left, y);
        y += 26.0;
        theme::label(ui.painter(), pos2(left, y + 8.0), &self.dir, mono(12.0), TEXT, right - left);
        y += 30.0;
        let text = "Removes the app, its shortcuts, its entry in Windows' installed apps, and Tesla's tools if setup fetched them.\n\nOther files in that folder aren't touched. Your clips, settings, saved layouts and git stay.";
        text_block(ui.painter(), pos2(left, y), text, sans(13.0), TEXT2, right - left);

        let foot = footer(ui, full);
        let mut row = Row::new(foot);
        if theme::button(ui, row.right(92.0, 8.0), Face::Text("uninstall"), Look::Primary, true, "uninstall").clicked() {
            self.start_uninstall();
            return;
        }
        if theme::text_button(ui, row.right(72.0, 16.0), "cancel", true).clicked() {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn progress_page(&mut self, ui: &mut Ui, full: Rect) {
        let left = full.min.x + PAD;
        let right = full.max.x - PAD;
        let mut y = full.min.y + HEADER_H + 20.0;
        let word = match (self.finished, self.uninstall_dir.is_some()) {
            (false, false) => "Installing",
            (false, true) => "Removing",
            (true, _) if self.ok() => "Finished",
            (true, _) => "Finished with problems",
        };
        caption(ui, word, left, y);
        y += 22.0;
        for (task, state) in self.tasks.clone() {
            let (detail, status) = match &state {
                State::Waiting => (String::new(), Mark::Dim("waiting")),
                State::Running => (String::new(), Mark::Bad("running")),
                State::Done => (String::new(), Mark::Ok),
                State::Failed(why) => (why.clone(), Mark::Bad("failed")),
                State::Skipped(why) => (why.clone(), Mark::Dim("skipped")),
            };
            let title = task.label();
            status_row(ui, left, right, &mut y, title, &detail, status);
        }
        y += 16.0;

        let foot_top = full.max.y - FOOTER_H;
        let log = Rect::from_min_max(pos2(left, y), pos2(right, foot_top - 16.0));
        ui.painter().rect_filled(log, 0.0, theme::VIDEO_BG);
        ui.painter().rect_stroke(log, 0.0, line(theme::LINE), egui::StrokeKind::Inside);
        let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(log.shrink(8.0)));
        ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(&mut inner, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            if self.log.is_empty() {
                ui.label(egui::RichText::new("output appears here").font(mono(11.0)).color(TEXT3));
            }
            for l in &self.log {
                ui.label(egui::RichText::new(l).font(mono(11.0)).color(TEXT2));
            }
        });

        let foot = footer(ui, full);
        let mut row = Row::new(foot);
        if !self.finished {
            let hint = if self.close_warned { "still working. Close again to quit anyway" } else { "installers that ask for permission may wait behind this window" };
            theme::label(ui.painter(), pos2(row.rest.min.x, row.rest.center().y), hint, sans(12.5), TEXT3, row.rest.width());
            return;
        }
        let dir = install::validate_dir(&self.dir).ok();
        let installed = self.uninstall_dir.is_none() && dir.as_ref().is_some_and(|d| d.join(install::APP_EXE).exists());
        if self.uninstall_dir.is_none() && !installed {
            if theme::button(ui, row.right(72.0, 8.0), Face::Text("back"), Look::Primary, true, "back").clicked() {
                self.page = Page::Options;
                self.recheck();
            }
        } else if installed
            && theme::button(ui, row.right(160.0, 8.0), Face::Text("open openteslacam"), Look::Primary, true, "open openteslacam").clicked()
            && let Some(d) = &dir
        {
            let _ = std::process::Command::new(d.join(install::APP_EXE)).current_dir(d).spawn();
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
        let look = if self.uninstall_dir.is_some() { Look::Primary } else { Look::Normal };
        if theme::button(ui, row.right(72.0, 16.0), Face::Text("close"), look, true, "close").clicked() {
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
        if !self.ok() && self.uninstall_dir.is_none() && installed {
            theme::label(ui.painter(), pos2(row.rest.min.x, row.rest.center().y), "the app is installed; see above for what didn't work", sans(12.5), TEXT3, row.rest.width());
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        self.poll();
        // The worker can't be interrupted safely, so closing mid-run takes a second try.
        if matches!(self.page, Page::Progress) && !self.finished && !self.close_warned && ui.ctx().input(|i| i.viewport().close_requested()) {
            ui.ctx().send_viewport_cmd(ViewportCommand::CancelClose);
            self.close_warned = true;
        }
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, theme::BG);
        self.header(ui, full);
        match self.page {
            Page::Options => self.options_page(ui, full),
            Page::Confirm => self.confirm_page(ui, full),
            Page::Progress => self.progress_page(ui, full),
        }
    }
}

fn set_state(tasks: &mut [(Task, State)], task: Task, state: State) {
    if let Some(t) = tasks.iter_mut().find(|(t, _)| *t == task) {
        t.1 = state;
    }
}

fn caption(ui: &Ui, text: &str, x: f32, y: f32) {
    theme::spaced(ui.painter(), pos2(x, y + 7.0), &text.to_uppercase(), medium(11.0), TEXT3, 1.54, 500.0);
}

/// The footer bar: its rule, and the rect for its row of buttons.
fn footer(ui: &Ui, full: Rect) -> Rect {
    let top = full.max.y - FOOTER_H;
    ui.painter().hline(full.x_range(), top + 0.5, line(theme::LINE));
    Rect::from_center_size(pos2(full.center().x, top + FOOTER_H / 2.0), vec2(full.width() - PAD * 2.0, BTN_H))
}

/// Wrapped text; returns its height.
fn text_block(p: &Painter, at: Pos2, text: &str, font: FontId, color: Color32, width: f32) -> f32 {
    let g = p.layout(text.to_owned(), font, color, width);
    let h = g.size().y;
    p.galley(at, g, color);
    h
}

#[derive(Clone, Copy)]
enum Mark {
    Ok,
    Waiting,
    /// Needs attention: filled, like every lit signal in the viewer.
    Bad(&'static str),
    Dim(&'static str),
}

/// Right-aligned on `at`; returns its width.
fn mark(p: &Painter, at: Pos2, m: Mark) -> f32 {
    let dim = |text: &str, color: Color32| {
        let g = p.layout_no_wrap(text.to_owned(), mono(11.0), color);
        let w = g.size().x;
        p.galley(pos2(at.x - w, at.y - g.size().y / 2.0), g, color);
        w
    };
    match m {
        Mark::Ok => dim("ok", TEXT3),
        Mark::Waiting => dim("…", theme::OFF),
        Mark::Dim(t) => dim(t, theme::OFF),
        Mark::Bad(t) => {
            let g = p.layout_no_wrap(t.to_owned(), medium(11.0), theme::BG);
            let w = g.size().x + 12.0;
            let rect = Rect::from_min_max(pos2(at.x - w, at.y - 9.0), pos2(at.x, at.y + 9.0));
            p.rect_filled(rect, 0.0, TEXT);
            p.galley(pos2(rect.min.x + 6.0, at.y - g.size().y / 2.0), g, theme::BG);
            w
        }
    }
}

/// One line per item with a rule under it. A long detail wraps onto extra lines.
fn status_row(ui: &Ui, left: f32, right: f32, y: &mut f32, name: &str, detail: &str, m: Mark) {
    let p = ui.painter();
    let name_w = (theme::text_width(ui, name, mono(12.0)) + 16.0).max(84.0);
    let mid = *y + 15.0;
    let mark_w = mark(p, pos2(right, mid), m);
    theme::label(p, pos2(left, mid), name, mono(12.0), TEXT, name_w);
    let detail_w = right - left - name_w - mark_w - 12.0;
    let g = p.layout(detail.to_owned(), sans(12.5), TEXT2, detail_w);
    let detail_h = g.size().y;
    let first_line = g.rows.first().map_or(16.0, |r| r.size.y);
    p.galley(pos2(left + name_w, mid - first_line / 2.0), g, TEXT2);
    *y += (30.0 + detail_h - first_line).ceil();
    p.hline(left..=right, *y - 0.5, line(theme::LINE));
}

/// A hard-edged checkbox at the left of `rect`, which is all clickable.
fn checkbox(ui: &mut Ui, rect: Rect, on: &mut bool, enabled: bool, name: &str) -> Response {
    let mut r = ui.allocate_rect(rect, if enabled { Sense::click() } else { Sense::hover() });
    r.widget_info(|| WidgetInfo::selected(Role::CheckBox, enabled, *on, name));
    if r.clicked() {
        *on = !*on;
        r.mark_changed();
    }
    let hot = enabled && r.hovered();
    let box_rect = Rect::from_min_size(pos2(rect.min.x, rect.min.y + 4.0), vec2(14.0, 14.0));
    let p = ui.painter();
    if *on {
        p.rect_filled(box_rect, 0.0, if hot { TEXT2 } else { TEXT });
        let at = |x: f32, y: f32| box_rect.min + vec2(x, y);
        p.line_segment([at(3.0, 7.0), at(6.0, 10.0)], egui::Stroke::new(1.5, theme::BG));
        p.line_segment([at(6.0, 10.0), at(11.0, 4.0)], egui::Stroke::new(1.5, theme::BG));
    } else {
        let border = if !enabled {
            theme::LINE
        } else if hot {
            TEXT3
        } else {
            theme::LINE2
        };
        p.rect_stroke(box_rect, 0.0, line(border), egui::StrokeKind::Inside);
    }
    if enabled { r.on_hover_cursor(egui::CursorIcon::PointingHand) } else { r }
}
