//! The viewer window: app state and the per-frame order of panels.

mod bars;
mod events;
pub(crate) mod export;
pub(crate) mod grid;
mod import;
mod keys;
mod manage;
mod map;
mod onboard;
mod settings;
pub(crate) mod theme;
mod timeline;
mod update;
mod vehicle;

use crate::decode::{CamWorker, Mode};
use crate::export::Trim;
use crate::library::{self, CAMERAS, Event};
use crate::player::Player;
use crate::telemetry::{self, Store};
use eframe::egui::{self, Align2, CentralPanel, ColorImage, Context, Frame, TextureHandle, TextureOptions, Ui};
use events::Filter;
use grid::Tile;
use keys::Action;
use settings::Settings;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Instant;

pub fn run(folder: Option<PathBuf>) -> Result<(), String> {
    // shortcut: rows don't wrap like the mockup's flex-wrap, the minimum width keeps them from colliding; wrap if people want smaller windows.
    // Without this eframe shows its own icon, which has a margin and so looks small on the taskbar.
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png")).map_err(|e| e.to_string())?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_app_id("openteslacam").with_inner_size([1440.0, 960.0]).with_min_inner_size([1280.0, 720.0]).with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "openteslacam",
        options,
        Box::new(move |cc| {
            let settings = cc.storage.and_then(|s| s.get_string(settings::KEY)).map_or_else(Settings::default, |s| Settings::from_json(&s));
            crate::update::check(cc.egui_ctx.clone());
            let mut app = App::new(&cc.egui_ctx, settings, folder);
            app.drives_rx = Some(import::watch(cc.egui_ctx.clone()));
            // Here and not in `new`: once per install, on a real launch.
            app.onboard.open = !app.settings.onboarded;
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| e.to_string())
}

pub struct App {
    settings: Settings,
    folder: Option<PathBuf>,
    events: Vec<Event>,
    filter: Filter,
    /// Set while a scan runs; replacing it abandons the old one.
    scan_rx: Option<Receiver<io::Result<Vec<Event>>>>,
    scan_error: Option<String>,
    thumb_rx: Option<Receiver<(usize, ColorImage)>>,
    /// By index into `events`.
    thumbs: HashMap<usize, TextureHandle>,
    /// Index into `events`.
    current: Option<usize>,
    player: Option<Player>,
    /// The open event's vehicle data so far. It grows per segment, complete once `telemetry_rx` is None.
    telemetry: Option<Store>,
    /// Set while the event's SEI is being read; replacing it abandons that load.
    telemetry_rx: Option<Receiver<Store>>,
    lane_cache: Option<timeline::LaneCache>,
    map: map::State,
    /// Indexed like `CAMERAS`.
    workers: Vec<CamWorker>,
    tiles: [Tile; 6],
    /// The pointer holds the scrubber.
    scrubbing: bool,
    focus: Option<usize>,
    editor: grid::Editor,
    shortcuts_open: bool,
    update_open: bool,
    trim: Trim,
    export: export::Dialog,
    import: import::Dialog,
    manage: manage::Dialog,
    onboard: onboard::Dialog,
    /// Tesla drives as they're plugged in; None where nothing watches (tests).
    drives_rx: Option<Receiver<PathBuf>>,
    /// Set while an event's segment table is being read; replacing it abandons that one. The event
    /// that's open now stays until the new one is ready.
    opening: Option<Opening>,
}

struct Opening {
    /// Index into `events`.
    event: usize,
    play: bool,
    rx: Receiver<Player>,
}

impl App {
    fn new(ctx: &Context, settings: Settings, folder: Option<PathBuf>) -> App {
        theme::install(ctx);
        let folder = folder.or_else(|| settings.last_folder.clone().filter(|p| p.is_dir()));
        let mut app = App {
            settings,
            folder: None,
            events: Vec::new(),
            filter: Filter::default(),
            scan_rx: None,
            scan_error: None,
            thumb_rx: None,
            thumbs: HashMap::new(),
            current: None,
            player: None,
            telemetry: None,
            telemetry_rx: None,
            lane_cache: None,
            map: map::State::default(),
            workers: CAMERAS.iter().map(|c| CamWorker::spawn(c, ctx.clone())).collect(),
            tiles: Default::default(),
            scrubbing: false,
            focus: None,
            editor: grid::Editor::default(),
            shortcuts_open: false,
            update_open: false,
            trim: Trim::default(),
            export: export::Dialog::default(),
            import: import::Dialog::default(),
            manage: manage::Dialog::default(),
            onboard: onboard::Dialog::default(),
            drives_rx: None,
            opening: None,
        };
        if let Some(f) = folder {
            app.open_folder(ctx, f);
        }
        app
    }

    fn pick_folder(&mut self, ctx: &Context) {
        if let Some(p) = rfd::FileDialog::new().pick_folder() {
            self.open_folder(ctx, p);
        }
    }

    /// Scans off the UI thread: a slow USB drive with hundreds of events must not freeze the window.
    fn open_folder(&mut self, ctx: &Context, path: PathBuf) {
        self.close_event();
        self.events.clear();
        self.thumbs.clear();
        self.thumb_rx = None;
        self.scan_error = None;
        self.folder = Some(path.clone());
        let (tx, rx) = channel();
        self.scan_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(library::scan(&path));
            ctx.request_repaint();
        });
    }

    /// The open event's mark, as wall-clock seconds.
    fn mark(&self) -> Option<(f64, f64)> {
        let ev = &self.events[self.current?];
        self.settings.marks.get(&ev.name).copied().filter(|_| events::markable(ev.kind))
    }

    fn close_event(&mut self) {
        for w in &self.workers {
            w.open(Vec::new());
        }
        self.tiles = Default::default();
        (self.current, self.player, self.focus) = (None, None, None);
        (self.telemetry, self.telemetry_rx, self.lane_cache) = (None, None, None);
        self.map.new_event();
        self.trim = Trim::default();
        self.export.dest = None;
        self.opening = None;
    }

    /// Reads the segment table off the UI thread, as the scan does: a long RecentClips set on a USB
    /// stick is many file opens. `finish_open` takes over when it arrives; `play` starts it then.
    fn open_event(&mut self, ctx: &Context, i: usize, play: bool) {
        let ev = &self.events[i];
        if ev.encrypted {
            return;
        }
        let ev = ev.clone();
        let (tx, rx) = channel();
        self.opening = Some(Opening { event: i, play, rx });
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(Player::open(&ev));
            ctx.request_repaint();
        });
    }

    fn finish_open(&mut self, ctx: &Context, i: usize, mut player: Player, play: bool) {
        let ev = &self.events[i];
        player.seek(events::start_position(ev));
        if let Some(old) = &self.player {
            player.rate = old.rate;
        }
        if play {
            player.play();
        }
        for (cam, w) in self.workers.iter().enumerate() {
            w.open(ev.segments.iter().map(|s| s.files[cam].clone()).collect());
        }
        self.tiles = Default::default();
        self.focus = None;
        self.current = Some(i);
        (self.telemetry, self.lane_cache) = (None, None);
        self.map.new_event();
        self.trim = Trim::default();
        // A marked event opens just before its mark, with a marked range ready as In and Out.
        if let Some((a, b)) = self.mark() {
            let (a, b) = (a - player.start as f64, b - player.start as f64);
            player.seek((a - events::MARK_LEAD).max(0.0));
            if b > a {
                self.trim = Trim { start: Some(a), end: Some(b) };
            }
        }
        self.export.dest = None;
        // Reads whole clips: seconds per segment on a USB stick.
        let mut jobs = telemetry::jobs(ev, &player.segs);
        telemetry::nearest_first(&mut jobs, player.position);
        let (tx, rx) = channel();
        self.telemetry_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            telemetry::load(jobs, |store| {
                let wanted = tx.send(store).is_ok();
                ctx.request_repaint();
                wanted
            });
            // Hang up first, so the repaint sees the load as finished.
            drop(tx);
            ctx.request_repaint();
        });
        self.player = Some(player);
    }

    /// A dropped folder (a stray file must not close what's open, as in the legacy app), the
    /// finished scan, and thumbnails that have arrived.
    fn poll(&mut self, ctx: &Context) {
        // The export dialog is modal for drops too: it shows the open event's range.
        if let Some(p) = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).find(|p| p.is_dir())).filter(|_| !self.export.open) {
            self.open_folder(ctx, p);
        }
        if let Some(drive) = self.drives_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            import::drive_arrived(self, drive);
        }
        if let Some(result) = self.scan_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.scan_rx = None;
            match result {
                Ok(events) => {
                    // Only a folder that held something is worth coming back to.
                    if !events.is_empty() {
                        self.settings.last_folder = self.folder.clone();
                    }
                    self.events = events;
                    self.load_thumbs(ctx);
                    // Like the legacy app: the newest event is ready to play, paused.
                    if let Some(i) = self.events.iter().position(|e| !e.encrypted) {
                        self.open_event(ctx, i, false);
                    }
                }
                Err(e) => self.scan_error = Some(e.to_string()),
            }
        }
        // Not under the export dialog, which shows the open event's range.
        if let Some(o) = self.opening.as_ref().filter(|_| !self.export.open) {
            match o.rx.try_recv() {
                Ok(player) => {
                    let (i, play) = (o.event, o.play);
                    self.opening = None;
                    self.finish_open(ctx, i, player, play);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.opening = None,
            }
        }
        while let Some(rx) = &self.telemetry_rx {
            match rx.try_recv() {
                Ok(store) => {
                    (self.telemetry, self.lane_cache) = (Some(store), None);
                    self.map.new_data();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => self.telemetry_rx = None,
            }
        }
        if let Some(rx) = &self.thumb_rx {
            for (i, image) in rx.try_iter() {
                self.thumbs.insert(i, ctx.load_texture(format!("thumb{i}"), image, TextureOptions::LINEAR));
            }
        }
    }

    fn load_thumbs(&mut self, ctx: &Context) {
        let list: Vec<(usize, PathBuf)> = self.events.iter().enumerate().filter_map(|(i, e)| Some((i, e.thumb.clone()?))).collect();
        let (tx, rx) = channel();
        self.thumb_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            for (i, path) in list {
                let Some(image) = std::fs::read(&path).ok().and_then(|b| png_image(&b)) else { continue };
                // The receiver is gone once another folder was opened.
                if tx.send((i, image)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
    }

    fn keyboard(&mut self, ctx: &Context) {
        // The update dialog takes Esc itself; nothing plays or seeks under it.
        if ctx.text_edit_focused() || self.update_open || self.import.open || self.manage.open || self.onboard.open {
            return;
        }
        // Space on a tabbed-to button already clicks that button.
        let tabbed = ctx.memory(|m| m.focused()).is_some();
        let actions: Vec<Action> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, repeat, modifiers, .. } => {
                        keys::action_for(*key, *modifiers).filter(|a| (!repeat || a.repeats()) && !(tabbed && *a == Action::TogglePlay))
                    }
                    _ => None,
                })
                .collect()
        });
        for a in actions {
            if self.export.open {
                // "adjust with i / o" works under the dialog, which handles Esc itself.
                if !matches!(a, Action::ExitFocus | Action::Shortcuts) {
                    self.apply(a);
                }
            } else if !self.shortcuts_open {
                self.apply(a);
            } else if matches!(a, Action::Shortcuts | Action::ExitFocus) {
                self.shortcuts_open = false;
            }
        }
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::ExitFocus => self.focus = None,
            Action::Shortcuts => self.shortcuts_open = !self.shortcuts_open,
            Action::TrimIn => {
                if let Some(p) = &self.player {
                    self.trim.set_in(p.position);
                }
            }
            Action::TrimOut => {
                if let Some(p) = &self.player {
                    self.trim.set_out(p.position);
                }
            }
            Action::Mark => {
                if let (Some(p), Some(i)) = (&self.player, self.current)
                    && events::markable(self.events[i].kind)
                {
                    // The range if one is set, else the moment on screen.
                    let (a, b) = if self.trim.is_set() { self.trim.range(p.duration()) } else { (p.position, p.position) };
                    self.settings.toggle_mark(&self.events[i].name, (p.start as f64 + a, p.start as f64 + b));
                }
            }
            a => {
                if let Some(p) = &mut self.player {
                    a.on_player(p);
                }
            }
        }
    }

    /// Runs after the panels that can move the clock, so a seek reaches the decoders this frame.
    // shortcut: below 4x hidden and unfocused cameras decode in full, thin them out too if weaker GPUs can't do 2x.
    fn pump_decoders(&mut self, ctx: &Context) {
        let Some(p) = &self.player else { return };
        let target = p.target();
        let mode = if self.scrubbing {
            Mode::Keyframes
        } else if p.playing {
            Mode::Play
        } else {
            Mode::Exact
        };
        let front = self.tiles[0].tex.as_ref().map(|t| t.size());
        for (cam, (w, tile)) in self.workers.iter().zip(&mut self.tiles).enumerate() {
            if let Some((seg, local)) = target {
                w.seek(seg, local, cam_mode(mode, p.rate, front, self.focus, cam));
            }
            match w.take() {
                Some(Ok(f)) => tile.show_frame(ctx, cam, f),
                Some(Err(e)) => tile.show_error(e),
                None => {}
            }
        }
    }

    /// The line where the event list would be.
    fn status_text(&self) -> String {
        if self.scan_rx.is_some() {
            "scanning…".to_owned()
        } else if self.opening.is_some() {
            "opening…".to_owned()
        } else if let Some(e) = &self.scan_error {
            e.clone()
        } else if self.folder.is_none() {
            "open your TeslaCam folder".to_owned()
        } else if !self.events.is_empty() {
            "every clip here is encrypted".to_owned()
        } else {
            "no clips here".to_owned()
        }
    }

    fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.keyboard(&ctx);
        if let Some(p) = &mut self.player {
            p.tick(Instant::now());
        }

        bars::top_bar(self, ui);
        bars::bottom_panel(self, ui);
        // After the bottom panel, so the library only spans the top row (design rule 1).
        events::panel(self, ui);
        CentralPanel::no_frame().frame(Frame::NONE.fill(theme::BG)).show(ui, |ui| {
            self.pump_decoders(&ctx);
            let panel = ui.max_rect();
            if self.player.is_some() {
                grid::show(ui, panel, &mut self.settings, &mut self.editor, &self.tiles, &mut self.focus);
            } else {
                let msg = if self.folder.is_some() { self.status_text() } else { "open a folder, or drop one here".to_owned() };
                ui.painter().text(panel.center(), Align2::CENTER_CENTER, msg, theme::mono(11.5), theme::TEXT3);
            }
        });
        keys::shortcuts_modal(&ctx, &mut self.shortcuts_open);
        export::modal(self, &ctx);
        update::modal(self, &ctx);
        import::modal(self, &ctx);
        manage::modal(self, &ctx);
        onboard::modal(self, &ctx);

        if self.player.as_ref().is_some_and(|p| p.playing) {
            ctx.request_repaint();
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(settings::KEY, self.settings.to_json());
    }
}

/// How hard camera `cam` should decode. An HW4 set at 4x is more pixels per second than a GPU's
/// decoder manages, and a saturated one starves the front camera (its frames are 4x the work): the
/// camera that matters keeps every frame, the rest show keyframes.
fn cam_mode(mode: Mode, rate: f64, front_size: Option<[usize; 2]>, focus: Option<usize>, cam: usize) -> Mode {
    /// Between HW3's 1.2 MP and HW4's 5.4 MP front camera.
    const HW4_FRONT: usize = 3_000_000;
    let heavy = front_size.is_some_and(|[w, h]| w * h > HW4_FRONT);
    if mode == Mode::Play && rate >= 4.0 && heavy && cam != focus.unwrap_or(0) { Mode::Keyframes } else { mode }
}

fn png_image(bytes: &[u8]) -> Option<ColorImage> {
    let icon = eframe::icon_data::from_png_bytes(bytes).ok()?;
    Some(ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::library::Kind;
    use eframe::egui::{DroppedFile, Event as InputEvent, Key, Modifiers, RawInput, Rect, pos2, vec2};
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn only_hw4_at_4x_thins_out_the_other_cameras() {
        const HW4: Option<[usize; 2]> = Some([2896, 1876]);
        const HW3: Option<[usize; 2]> = Some([1280, 960]);
        let play = |rate, front, focus, cam| cam_mode(Mode::Play, rate, front, focus, cam);
        assert_eq!(play(4.0, HW4, None, 0), Mode::Play, "front keeps every frame");
        assert_eq!(play(4.0, HW4, None, 1), Mode::Keyframes);
        assert_eq!(play(4.0, HW4, Some(3), 3), Mode::Play, "the focused camera does instead");
        assert_eq!(play(4.0, HW4, Some(3), 0), Mode::Keyframes);
        assert_eq!(play(2.0, HW4, None, 1), Mode::Play, "2x keeps up");
        assert_eq!(play(4.0, HW3, None, 1), Mode::Play, "HW3 is a quarter of the pixels");
        assert_eq!(play(4.0, None, None, 1), Mode::Play, "no front picture to go by yet");
        // Paused and scrubbing aren't touched.
        assert_eq!(cam_mode(Mode::Exact, 4.0, HW4, None, 1), Mode::Exact);
        assert_eq!(cam_mode(Mode::Keyframes, 1.0, HW4, None, 0), Mode::Keyframes);
    }
    use std::time::{Duration, Instant};

    impl App {
        /// `open_event`, paused, and wait for it to take over.
        fn open_now(&mut self, ctx: &Context, i: usize) {
            self.open_event(ctx, i, false);
            let t0 = Instant::now();
            while self.opening.is_some() {
                assert!(t0.elapsed() < Duration::from_secs(20), "timed out opening event {i}");
                self.poll(ctx);
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn raw(w: f32, h: f32) -> RawInput {
        RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h))), ..Default::default() }
    }

    fn step(ctx: &Context, app: &mut App, input: RawInput) {
        ctx.run_ui(input, |ui| app.frame(ui)).textures_delta.clear();
    }

    fn press(key: Key) -> RawInput {
        let mut input = raw(1440.0, 960.0);
        for pressed in [true, false] {
            input.events.push(InputEvent::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE });
        }
        input
    }

    #[derive(Debug)]
    struct Dropped(PathBuf);

    impl DroppedFile for Dropped {
        fn path(&self) -> &Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            Err("not read".to_owned())
        }
    }

    fn drop_on(path: PathBuf) -> RawInput {
        let mut input = raw(1440.0, 960.0);
        input.dropped_files.push(Arc::new(Dropped(path)));
        input
    }

    /// The whole pipeline without a window: scan a generated TeslaCam tree, auto-open the newest
    /// playable event, decode real frames into the tiles, drive it from the keyboard, and lay
    /// everything out at sizes from sane to absurd.
    #[test]
    fn headless_session_on_a_fixture() {
        let dir = std::env::temp_dir().join(format!("otc-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fixture::make_fixture(&dir, 2, 64, 48).unwrap();

        let ctx = Context::default();
        let mut app = App::new(&ctx, Settings { last_folder: Some(dir.clone()), ..Settings::default() }, None);
        step(&ctx, &mut app, raw(1440.0, 960.0));

        let until = |app: &mut App, ctx: &Context, what: &str, done: &dyn Fn(&App) -> bool| {
            let t0 = Instant::now();
            while !done(app) {
                assert!(t0.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
                step(ctx, app, raw(1440.0, 960.0));
                thread::sleep(Duration::from_millis(5));
            }
        };

        until(&mut app, &ctx, "the scan", &|a| a.scan_rx.is_none());
        assert_eq!(app.scan_error, None);
        assert_eq!(app.events.len(), 4);
        until(&mut app, &ctx, "the auto-open", &|a| a.opening.is_none());
        assert!(!app.player.as_ref().unwrap().playing, "ready to play, paused");
        let opened = app.current.expect("auto-opened an event");
        assert!(!app.events[opened].encrypted);
        until(&mut app, &ctx, "thumbnails", &|a| !a.thumbs.is_empty());
        until(&mut app, &ctx, "a decoded frame on every tile", &|a| a.tiles.iter().all(|t| t.tex.is_some()));

        // Space plays, the clock runs and the pictures follow; Space pauses.
        step(&ctx, &mut app, press(Key::Space));
        assert!(app.player.as_ref().unwrap().playing);
        let before = app.player.as_ref().unwrap().position;
        let shown = app.tiles[0].shown;
        until(&mut app, &ctx, "the clock", &|a| {
            let p = a.player.as_ref().unwrap();
            !p.playing || p.position > before + 0.2
        });
        until(&mut app, &ctx, "a newer frame", &|a| a.tiles[0].shown != shown || !a.player.as_ref().unwrap().playing);
        if app.player.as_ref().unwrap().playing {
            step(&ctx, &mut app, press(Key::Space));
        }
        assert!(!app.player.as_ref().unwrap().playing);

        // Home goes to the start; focus and Esc; the modal swallows other keys.
        step(&ctx, &mut app, press(Key::Home));
        assert_eq!(app.player.as_ref().unwrap().position, 0.0);
        app.focus = Some(2);
        step(&ctx, &mut app, raw(1440.0, 960.0));
        step(&ctx, &mut app, press(Key::Escape));
        assert_eq!(app.focus, None);
        app.apply(Action::Shortcuts);
        step(&ctx, &mut app, press(Key::Space));
        assert!(!app.player.as_ref().unwrap().playing);
        step(&ctx, &mut app, press(Key::Escape));
        assert!(!app.shortcuts_open);

        // An encrypted event can't be opened; a Saved one starts where its event.json says.
        let locked = app.events.iter().position(|e| e.encrypted).expect("the fixture has one");
        let current = app.current;
        app.open_now(&ctx, locked);
        assert_eq!(app.current, current);
        let saved = app.events.iter().position(|e| e.kind == Kind::Saved).expect("the fixture has one");
        app.open_now(&ctx, saved);
        assert_eq!(app.current, Some(saved));

        // Layout must survive any window size, library open or closed, lanes open or closed, in every
        // camera layout, including the editor and with every camera hidden.
        for (w, h) in [(1280.0, 720.0), (3840.0, 2160.0), (300.0, 200.0), (1.0, 1.0)] {
            for (lib, lanes) in [(true, true), (false, false)] {
                app.settings.library_open = lib;
                app.settings.lanes_open = lanes;
                app.editor.editing = lanes;
                for layout in settings::Layout::ALL {
                    app.settings.layout = layout;
                    for _ in 0..3 {
                        step(&ctx, &mut app, raw(w, h));
                    }
                }
            }
        }
        app.editor.editing = true;
        app.settings.custom_layouts.insert(app.settings.custom_active.clone(), [None; 6]);
        step(&ctx, &mut app, raw(1280.0, 720.0));
        step(&ctx, &mut app, raw(1280.0, 720.0));

        // A stray dropped file is ignored; a dropped folder is opened.
        let stray = dir.join("stray.txt");
        std::fs::write(&stray, b"x").unwrap();
        step(&ctx, &mut app, drop_on(stray));
        assert!(app.player.is_some() && app.scan_rx.is_none());
        let empty = dir.join("empty");
        std::fs::create_dir(&empty).unwrap();
        step(&ctx, &mut app, drop_on(empty.clone()));
        assert_eq!(app.folder.as_ref(), Some(&empty));
        until(&mut app, &ctx, "the empty scan", &|a| a.scan_rx.is_none());
        assert!(app.events.is_empty());
        assert_eq!(app.settings.last_folder.as_ref(), Some(&dir), "an empty folder isn't remembered");

        // Another folder replaces everything; a bad one reports instead of failing, and isn't remembered.
        app.open_folder(&ctx, dir.join("missing"));
        assert!(app.player.is_none() && app.events.is_empty());
        until(&mut app, &ctx, "the failed scan", &|a| a.scan_rx.is_none());
        assert!(app.scan_error.is_some());
        assert_eq!(app.settings.last_folder.as_ref(), Some(&dir));
        step(&ctx, &mut app, raw(1440.0, 960.0));

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn collect_text(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Vec(v) => v.iter().for_each(|s| collect_text(s, out)),
            egui::Shape::Text(t) => out.push(t.galley.text().to_owned()),
            _ => {}
        }
    }

    /// Runs one frame and returns all the text it painted.
    fn painted(ctx: &Context, app: &mut App) -> Vec<String> {
        let mut out = ctx.run_ui(raw(1440.0, 960.0), |ui| app.frame(ui));
        out.textures_delta.clear();
        let mut text = Vec::new();
        out.shapes.iter().for_each(|c| collect_text(&c.shape, &mut text));
        text
    }

    /// From a generated clip's SEI to the words on the strip and the lanes, through the real thread.
    #[test]
    fn telemetry_reaches_the_strip() {
        let dir = std::env::temp_dir().join(format!("otc-ui-tel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fixture::make_fixture(&dir, 2, 64, 48).unwrap();
        let ctx = Context::default();
        let mut app = App::new(&ctx, Settings { last_folder: Some(dir.clone()), ..Settings::default() }, None);
        let wait = |app: &mut App, what: &str, done: &dyn Fn(&App) -> bool| {
            let t0 = Instant::now();
            loop {
                step(&ctx, app, raw(1440.0, 960.0));
                if done(app) {
                    return;
                }
                assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&mut app, "the scan", &|a| a.scan_rx.is_none());
        let at = |app: &App, kind: Kind| app.events.iter().position(|e| e.kind == kind).unwrap();

        // A parked Sentry event has no SEI: the strip says so, playback goes on.
        app.open_now(&ctx, at(&app, Kind::Sentry));
        wait(&mut app, "the Sentry load", &|a| a.telemetry_rx.is_none());
        assert!(app.telemetry.as_ref().unwrap().records.is_empty());
        assert!(app.player.as_ref().unwrap().duration() > 0.0);
        let text = painted(&ctx, &mut app);
        assert!(text.iter().any(|s| s == "no vehicle data"), "{text:?}");

        // A Saved event with SEI: mapped across its two segments.
        app.open_now(&ctx, at(&app, Kind::Saved));
        wait(&mut app, "the Saved load", &|a| a.telemetry_rx.is_none());
        let store = app.telemetry.as_ref().unwrap();
        assert_eq!(store.records.len(), 144);
        assert!(store.records.windows(2).all(|w| w[0].t < w[1].t));
        assert!((store.records[72].t - 2.0).abs() < 1e-9, "the second segment starts at its own time");
        let brake = *store.hard.iter().find(|h| h.g < 0.0).expect("the fixture brakes hard");
        assert!(!store.left.is_empty() && !store.right.is_empty() && !store.bands.is_empty());
        let rec = *store.at(brake.t).unwrap();

        app.player.as_mut().unwrap().seek(brake.t);
        let text = painted(&ctx, &mut app);
        let has = |s: &str| text.iter().any(|t| t == s);
        assert!(has("KM/H"), "{text:?}");
        assert!(has(&format!("{:.0}", rec.tel.speed_mps * 3.6)), "{text:?}");
        assert!(has(&vehicle::steer_text(rec.tel.steering_deg)), "{text:?}");
        assert!(has(&vehicle::g_text(rec.g)), "{text:?}");
        assert!(has("on"), "the brake is on: {text:?}");
        assert!(!has("no vehicle data"));
        assert!(has(&format!("hard brake {}", vehicle::g_text(brake.g))), "{text:?}");
        assert!(has("dashcam button 15:30:02"), "the event marker: {text:?}");
        for band in ["tacc", "autosteer", "fsd"] {
            assert!(has(band), "{text:?}");
        }

        // Units reach the number and the lane's scale.
        app.settings.metric = false;
        let text = painted(&ctx, &mut app);
        assert!(text.iter().any(|t| t == "MPH") && text.iter().any(|t| t.starts_with("0–") && t.ends_with("mph")), "{text:?}");

        // The cache survives frames; closing the lanes doesn't throw it away either.
        assert!(app.lane_cache.is_some());
        app.settings.lanes_open = false;
        step(&ctx, &mut app, raw(1440.0, 960.0));
        assert!(app.lane_cache.is_some());
        app.settings.lanes_open = true;

        // Switching events mid-load abandons the old one, and its store doesn't land on the new event.
        app.open_now(&ctx, at(&app, Kind::Recent));
        app.open_now(&ctx, at(&app, Kind::Sentry));
        wait(&mut app, "the last load", &|a| a.telemetry_rx.is_none());
        assert!(app.telemetry.as_ref().unwrap().records.is_empty());

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A click in the library plays Saved and Sentry events from before their trigger; a scan's
    /// auto-open and a RecentClips row stay paused. Opening never waits on the UI thread.
    #[test]
    fn opening_from_the_library() {
        let dir = std::env::temp_dir().join(format!("otc-ui-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fixture::make_fixture(&dir, 2, 64, 48).unwrap();
        let ctx = Context::default();
        let mut app = App::new(&ctx, Settings { last_folder: Some(dir.clone()), ..Settings::default() }, None);
        let wait = |app: &mut App, what: &str, done: &dyn Fn(&App) -> bool| {
            let t0 = Instant::now();
            loop {
                step(&ctx, app, raw(1440.0, 960.0));
                if done(app) {
                    return;
                }
                assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&mut app, "the scan and auto-open", &|a| a.scan_rx.is_none() && a.opening.is_none());
        assert!(app.current.is_some() && !app.player.as_ref().unwrap().playing);
        let at = |app: &App, kind: Kind| app.events.iter().position(|e| e.kind == kind).unwrap();
        let (saved, recent) = (at(&app, Kind::Saved), at(&app, Kind::Recent));

        // The first row under a filter: top bar, title, path and filter rows, half a row.
        let first_row = pos2(events::WIDTH / 2.0, 56.0 + 16.0 + 32.0 + 14.0 + 20.0 + 14.0 + 24.0 + 10.0 + 36.0);
        app.filter = Filter::Saved;
        click(&ctx, &mut app, first_row);
        wait(&mut app, "the Saved event", &|a| a.current == Some(saved) && a.opening.is_none());
        assert!(app.player.as_ref().unwrap().playing);
        app.filter = Filter::Recent;
        click(&ctx, &mut app, first_row);
        wait(&mut app, "the Recent event", &|a| a.current == Some(recent) && a.opening.is_none());
        assert!(!app.player.as_ref().unwrap().playing);

        // Until its table arrives the old event stays whole; a second click replaces the first.
        app.open_event(&ctx, saved, false);
        assert_eq!(app.current, Some(recent));
        assert!(app.player.is_some());
        let sentry = at(&app, Kind::Sentry);
        app.open_event(&ctx, sentry, false);
        wait(&mut app, "the last click", &|a| a.opening.is_none());
        assert_eq!(app.current, Some(sentry));

        // Not under the export dialog; another folder drops what's on its way.
        app.open_event(&ctx, saved, false);
        app.export.open = true;
        thread::sleep(Duration::from_millis(200));
        app.poll(&ctx);
        assert_eq!(app.current, Some(sentry));
        app.export.open = false;
        wait(&mut app, "the open after the dialog", &|a| a.opening.is_none());
        assert_eq!(app.current, Some(saved));
        app.open_event(&ctx, sentry, false);
        app.open_folder(&ctx, dir.join("empty"));
        assert!(app.opening.is_none() && app.current.is_none());

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether a frame paints a rect of this fill.
    fn paints_fill(ctx: &Context, app: &mut App, fill: egui::Color32) -> bool {
        fn has(shape: &egui::Shape, fill: egui::Color32) -> bool {
            match shape {
                egui::Shape::Vec(v) => v.iter().any(|s| has(s, fill)),
                egui::Shape::Rect(r) => r.fill == fill,
                _ => false,
            }
        }
        let mut out = ctx.run_ui(raw(1440.0, 960.0), |ui| app.frame(ui));
        out.textures_delta.clear();
        out.shapes.iter().any(|c| has(&c.shape, fill))
    }

    /// M marks the moment or the In–Out range of a Saved event, it's kept by wall-clock time, shows in
    /// the library, and the event opens just before it next time. Sentry events take no mark.
    #[test]
    fn marks_on_a_fixture() {
        let dir = std::env::temp_dir().join(format!("otc-ui-mark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fixture::make_fixture(&dir, 2, 64, 48).unwrap();
        let ctx = Context::default();
        let mut app = App::new(&ctx, Settings { last_folder: Some(dir.clone()), ..Settings::default() }, None);
        let t0 = Instant::now();
        while app.scan_rx.is_some() {
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for the scan");
            step(&ctx, &mut app, raw(1440.0, 960.0));
            thread::sleep(Duration::from_millis(5));
        }
        let at = |app: &App, kind: Kind| app.events.iter().position(|e| e.kind == kind).unwrap();
        let (saved, sentry) = (at(&app, Kind::Saved), at(&app, Kind::Sentry));
        let name = app.events[saved].name.clone();
        app.open_now(&ctx, saved);
        app.player.as_mut().unwrap().pause();
        let start = app.player.as_ref().unwrap().start as f64;
        let seek = |app: &mut App, t: f64| app.player.as_mut().unwrap().seek(t);

        // A point, shown on the transport and in the library row; the same key there clears it.
        seek(&mut app, 3.5);
        step(&ctx, &mut app, press(Key::M));
        assert_eq!(app.settings.marks.get(&name), Some(&(start + 3.5, start + 3.5)));
        let text = painted(&ctx, &mut app);
        assert!(text.iter().any(|t| t.ends_with(&format!("· mark {}", events::hms((start + 3.5) as i64)))), "{text:?}");
        assert!(text.iter().filter(|t| *t == "mark").count() == 2, "the button and the timeline: {text:?}");
        step(&ctx, &mut app, press(Key::M));
        assert!(app.settings.marks.is_empty());

        // With In and Out set it's the range, and it comes back as In and Out when the event reopens.
        seek(&mut app, 3.25);
        step(&ctx, &mut app, press(Key::I));
        seek(&mut app, 3.75);
        step(&ctx, &mut app, press(Key::O));
        step(&ctx, &mut app, press(Key::M));
        assert_eq!(app.settings.marks.get(&name), Some(&(start + 3.25, start + 3.75)));
        assert_eq!(Settings::from_json(&app.settings.to_json()).marks, app.settings.marks, "kept across a restart");
        app.open_now(&ctx, sentry);
        step(&ctx, &mut app, press(Key::M));
        assert_eq!(app.settings.marks.len(), 1, "a Sentry event takes no mark");
        app.open_now(&ctx, saved);
        assert_eq!(app.player.as_ref().unwrap().position, 3.25 - events::MARK_LEAD);
        assert_eq!(app.trim, Trim { start: Some(3.25), end: Some(3.75) });

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The library panel's import, through the real worker: the fixture lands in the library folder in
    /// Tesla's layout, the app moves over to it, and a plugged-in drive only speaks up once a library is chosen.
    #[test]
    fn import_on_a_fixture() {
        let dir = std::env::temp_dir().join(format!("otc-ui-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (usb, lib) = (dir.join("usb"), dir.join("lib"));
        fixture::make_fixture(&usb, 2, 64, 48).unwrap();
        let ctx = Context::default();
        let mut app = App::new(&ctx, Settings { last_folder: Some(usb.clone()), ..Settings::default() }, None);
        let wait = |app: &mut App, what: &str, done: &dyn Fn(&App) -> bool| {
            let t0 = Instant::now();
            while !done(app) {
                assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
                step(&ctx, app, raw(1440.0, 960.0));
                thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&mut app, "the scan", &|a| a.scan_rx.is_none() && a.opening.is_none());
        let events = app.events.len();

        // No library chosen: a drive turning up changes nothing.
        import::drive_arrived(&mut app, usb.clone());
        assert!(!app.import.open);

        import::open_for(&mut app);
        assert!(app.import.open && app.import.src.as_ref() == Some(&usb));
        // A dialog's first frame only measures it.
        step(&ctx, &mut app, raw(1440.0, 960.0));
        let text = painted(&ctx, &mut app);
        assert!(text.iter().any(|t| t == "choose where the library goes"), "{text:?}");
        app.settings.library_dir = Some(lib.clone());
        import::start(&mut app);
        wait(&mut app, "the import", &|a| a.import.running.is_none());
        let done = app.import.outcome.as_ref().unwrap().as_ref().unwrap();
        assert!(done.copied > 0 && done.cleared == 0, "{done:?}");
        step(&ctx, &mut app, raw(1440.0, 960.0));
        assert!(painted(&ctx, &mut app).iter().any(|t| t == "import finished"));
        assert_eq!(app.folder.as_ref(), Some(&lib), "the library is what's open now");
        wait(&mut app, "the library scan", &|a| a.scan_rx.is_none());
        // The fixture's encrypted event is plain files to the import; everything else reads the same.
        assert_eq!(app.events.len(), events);
        assert!(usb.join("SavedClips").is_dir(), "the drive is as it was");

        // With a library, a drive opens the dialog; with auto-import it starts by itself.
        app.import.open = false;
        import::drive_arrived(&mut app, usb.clone());
        assert!(app.import.open && app.import.running.is_none());
        app.import.open = false;
        app.settings.import_auto = true;
        import::drive_arrived(&mut app, usb.clone());
        assert!(app.import.running.is_some());
        wait(&mut app, "the second import", &|a| a.import.running.is_none());
        let again = app.import.outcome.as_ref().unwrap().as_ref().unwrap();
        assert_eq!((again.copied, again.already), (0, done_copied(&lib)));

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Files under `dir`, two folder levels deep as a library is.
    fn done_copied(dir: &Path) -> u64 {
        let files = |d: &Path| std::fs::read_dir(d).unwrap().map(|e| e.unwrap().path()).collect::<Vec<_>>();
        files(dir).iter().flat_map(|k| files(k)).map(|p| if p.is_dir() { files(&p).len() as u64 } else { 1 }).sum()
    }

    /// The welcome: the library page, the controls page (the same guide as the `?` sheet), and Esc
    /// puts it away for good. A plain `App::new` never opens it by itself.
    #[test]
    fn welcome_shows_once() {
        let ctx = Context::default();
        let mut app = App::new(&ctx, Settings::default(), None);
        assert!(!app.onboard.open && !app.settings.onboarded);
        let shown = |app: &mut App| {
            step(&ctx, app, raw(1440.0, 960.0));
            painted(&ctx, app)
        };
        app.onboard.open = true;
        let text = shown(&mut app);
        for s in ["WELCOME · 1 OF 2", "keep in", "not chosen yet", "choose", "skip", "next", "no folder: footage plays from the drive"] {
            assert!(text.iter().any(|t| t == s), "{s}: {text:?}");
        }
        app.settings.library_dir = Some(std::env::temp_dir());
        assert!(shown(&mut app).iter().any(|t| t == "footage is copied here when you import"));
        // No drive prompt, update notice or key reaches past it.
        import::drive_arrived(&mut app, std::env::temp_dir());
        assert!(!app.import.open);

        app.onboard.page = 1;
        let text = shown(&mut app);
        for s in ["WELCOME · 2 OF 2", "timeline", "mark", "Space", "back", "done"] {
            assert!(text.iter().any(|t| t == s), "{s}: {text:?}");
        }
        step(&ctx, &mut app, press(Key::Escape));
        assert!(!app.onboard.open && app.settings.onboarded);
        assert!(Settings::from_json(&app.settings.to_json()).onboarded, "and it stays away after a restart");

        // The same guide is behind `?`.
        app.shortcuts_open = true;
        let text = shown(&mut app);
        assert!(text.iter().any(|t| t == "CONTROLS") && text.iter().any(|t| t == "timeline") && text.iter().any(|t| t == "Space"), "{text:?}");
    }

    /// Managing is for the library only, and goes through the real worker while the event is open:
    /// trim to the mark, then delete, with the list following.
    #[test]
    fn manage_on_a_fixture() {
        let dir = std::env::temp_dir().join(format!("otc-ui-manage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fixture::make_fixture(&dir, 2, 64, 48).unwrap();
        let ctx = Context::default();
        let mut app = App::new(&ctx, Settings { last_folder: Some(dir.clone()), ..Settings::default() }, None);
        let wait = |app: &mut App, what: &str, done: &dyn Fn(&App) -> bool| {
            let t0 = Instant::now();
            while !done(app) {
                assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
                step(&ctx, app, raw(1440.0, 960.0));
                thread::sleep(Duration::from_millis(5));
            }
        };
        let settled = |a: &App| a.scan_rx.is_none() && a.opening.is_none() && a.manage.running.is_none();
        wait(&mut app, "the scan", &settled);
        let saved = |app: &App| app.events.iter().position(|e| e.kind == Kind::Saved && !e.encrypted);
        app.open_now(&ctx, saved(&app).unwrap());
        wait(&mut app, "frames, so the files are in use", &|a| a.tiles.iter().all(|t| t.tex.is_some()));

        // A drive: nothing to manage. The same folder as the library: there it is.
        assert!(!manage::allowed(&app));
        app.settings.library_dir = Some(dir.clone());
        assert!(manage::allowed(&app));

        // Nothing marked: trim has nothing to go by. Marked in the second segment: the first one goes.
        manage::open(&mut app, manage::What::Cameras);
        step(&ctx, &mut app, raw(1440.0, 960.0));
        let text = painted(&ctx, &mut app);
        for cam in CAMERAS {
            assert!(text.iter().any(|t| t == cam), "{cam} in full: {text:?}");
        }
        assert!(text.iter().any(|t| t == "drop cameras: every camera here is ticked"), "{text:?}");
        manage::open(&mut app, manage::What::Trim);
        step(&ctx, &mut app, raw(1440.0, 960.0));
        assert!(painted(&ctx, &mut app).iter().any(|t| t.starts_with("Set In and Out")));
        let (events, name) = (app.events.len(), app.events[saved(&app).unwrap()].name.clone());
        let start = app.player.as_ref().unwrap().start as f64;
        // The fixture's segments are 2 s, well inside trim's lead-in, so nothing can be dropped.
        app.settings.toggle_mark(&name, (start + 3.5, start + 3.5));
        assert_eq!(manage::range(&app), Some((3.5, 3.5, false)));
        // In and Out are the selection on screen: they decide, not a mark set somewhere else (2.2.0
        // went by the mark and removed footage inside In and Out).
        app.trim = Trim { start: Some(0.5), end: None };
        assert_eq!(manage::range(&app), Some((0.5, 4.0, true)));
        step(&ctx, &mut app, raw(1440.0, 960.0));
        let text = painted(&ctx, &mut app);
        assert!(text.iter().any(|t| t.starts_with("Keeps In to Out, 15:30:00 → 15:30:04") && t.ends_with("may stay.")), "{text:?}");
        app.trim = Trim::default();
        manage::pick(&mut app, "trim", &crate::manage::Action::Trim(3.5, 3.5));
        step(&ctx, &mut app, raw(1440.0, 960.0));
        assert!(painted(&ctx, &mut app).iter().any(|t| t == "trim: the range already covers every segment"));
        manage::confirm(&mut app);
        assert!(app.manage.running.is_none() && app.player.is_some(), "nothing to confirm");

        // Right-click on a row: a menu, and delete asks before it goes.
        app.manage.open = false;
        let at = eframe::egui::pos2(100.0, 300.0);
        let i = saved(&app).unwrap();
        manage::ask(&mut app, i, at);
        step(&ctx, &mut app, raw(1440.0, 960.0));
        let text = painted(&ctx, &mut app);
        assert!(text.iter().any(|t| t == "archive") && text.iter().any(|t| t == "delete") && !text.iter().any(|t| t.starts_with("Delete this")), "{text:?}");
        manage::ask_delete(&mut app);
        step(&ctx, &mut app, raw(1440.0, 960.0));
        let text = painted(&ctx, &mut app);
        assert!(text.iter().any(|t| t.starts_with("Delete this whole event?") && t.ends_with("Recycle Bin.")), "{text:?}");
        manage::quick_run(&mut app, &crate::manage::Action::Delete);
        assert!(app.player.is_none(), "the event is let go of first");
        wait(&mut app, "the delete and the rescan", &settled);
        assert_eq!(app.events.len(), events - 1);
        assert!(!app.events.iter().any(|e| e.name == name) && !dir.join("SavedClips").join(&name).exists());
        assert!(!app.manage.open, "done without a word");

        // Archive goes at once.
        let sentry = app.events.iter().position(|e| e.kind == Kind::Sentry).unwrap();
        let name = app.events[sentry].name.clone();
        manage::ask(&mut app, sentry, at);
        manage::quick_run(&mut app, &crate::manage::Action::Archive(dir.clone()));
        wait(&mut app, "the archive and the rescan", &settled);
        assert!(!app.events.iter().any(|e| e.name == name) && dir.join("Archive").join("SentryClips").join(&name).exists());
        // On a drive nothing pops up.
        app.settings.library_dir = None;
        manage::ask(&mut app, 0, at);
        assert!(!painted(&ctx, &mut app).iter().any(|t| t == "archive"));

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// I and O through the keyboard, the transport's words, the dialog, and both export modes
    /// through the real worker thread.
    #[test]
    fn trim_and_export_on_a_fixture() {
        let dir = std::env::temp_dir().join(format!("otc-ui-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fixture::make_fixture(&dir, 2, 64, 48).unwrap();
        let out = dir.join("out");
        std::fs::create_dir(&out).unwrap();
        let ctx = Context::default();
        let mut app = App::new(&ctx, Settings { last_folder: Some(dir.clone()), ..Settings::default() }, None);
        let wait = |app: &mut App, what: &str, done: &dyn Fn(&App) -> bool| {
            let t0 = Instant::now();
            loop {
                step(&ctx, app, raw(1440.0, 960.0));
                if done(app) {
                    return;
                }
                assert!(t0.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&mut app, "the scan", &|a| a.scan_rx.is_none());
        let saved = app.events.iter().position(|e| e.kind == Kind::Saved).unwrap();
        app.open_now(&ctx, saved);

        // Nothing set: the dashes, the whole event, no range on the timeline.
        app.player.as_mut().unwrap().pause();
        let text = painted(&ctx, &mut app);
        assert!(text.iter().filter(|t| *t == "--:--:--").count() == 3, "in, out and mark: {text:?}");
        assert!(text.iter().any(|t| t == "export 4 s →"), "{text:?}");
        assert!(!paints_fill(&ctx, &mut app, timeline::TRIM_FILL));

        let seek = |app: &mut App, t: f64| app.player.as_mut().unwrap().seek(t);
        seek(&mut app, 1.0);
        step(&ctx, &mut app, press(Key::I));
        seek(&mut app, 2.5);
        step(&ctx, &mut app, press(Key::O));
        assert_eq!(app.trim, Trim { start: Some(1.0), end: Some(2.5) });
        let text = painted(&ctx, &mut app);
        for s in ["in", "out", "15:30:01", "15:30:02", "export 2 s →"] {
            assert!(text.iter().any(|t| t == s), "{s}: {text:?}");
        }
        assert!(paints_fill(&ctx, &mut app, timeline::TRIM_FILL));

        // The transport's buttons do the same, right-click clears, and export opens the dialog.
        let y = 960.0 - bars::bottom_height(true) + vehicle::HEIGHT + 12.0 + 18.0;
        let (in_at, export_at) = (pos2(1146.0, y), pos2(1372.0, y));
        seek(&mut app, 1.25);
        click(&ctx, &mut app, in_at);
        assert_eq!(app.trim.start, Some(1.25));
        for events in [vec![InputEvent::PointerMoved(in_at)], vec![InputEvent::PointerMoved(in_at)], vec![secondary(in_at, true)], vec![secondary(in_at, false)], vec![]] {
            step(&ctx, &mut app, pointer(events));
        }
        assert_eq!(app.trim.start, None);
        step(&ctx, &mut app, press(Key::Home));
        seek(&mut app, 1.0);
        step(&ctx, &mut app, press(Key::I));
        seek(&mut app, 2.5);
        click(&ctx, &mut app, export_at);
        assert!(app.export.open);
        app.export.open = false;

        // The same point again clears; a new event starts without a range or a picked name.
        step(&ctx, &mut app, press(Key::O));
        assert_eq!(app.trim.end, None);
        step(&ctx, &mut app, press(Key::O));
        assert_eq!(app.trim.end, Some(2.5));
        app.export.dest = Some(out.join("from-the-last-event"));
        app.open_now(&ctx, saved);
        assert!(!app.trim.is_set());
        assert_eq!(app.export.dest, None);
        app.player.as_mut().unwrap().pause();
        app.trim = Trim { start: Some(1.0), end: Some(2.5) };

        // The dialog: i / o still adjust under it, Esc closes it and only it.
        app.export.open = true;
        // An egui Area sizes itself invisibly on its first frame.
        step(&ctx, &mut app, raw(1440.0, 960.0));
        let text = painted(&ctx, &mut app);
        for s in ["EXPORT CLIP", "15:30:01 → 15:30:02", "2 s · segments 1–2", "adjust with i / o", "6 × mp4 + telemetry.csv · no re-encode", "left_pillar"] {
            assert!(text.iter().any(|t| t == s), "{s}: {text:?}");
        }
        seek(&mut app, 1.5);
        step(&ctx, &mut app, press(Key::I));
        assert_eq!(app.trim.start, Some(1.5));
        app.focus = Some(2);
        step(&ctx, &mut app, press(Key::Escape));
        assert!(!app.export.open);
        assert_eq!(app.focus, Some(2));
        app.focus = None;

        // Lossless.
        app.export.open = true;
        app.export.dest = Some(out.join("clip"));
        export::start(&mut app);
        assert!(app.export.running.is_some());
        // Esc doesn't close a running export.
        step(&ctx, &mut app, press(Key::Escape));
        assert!(app.export.open);
        wait(&mut app, "the lossless export", &|a| a.export.running.is_none());
        assert_eq!(app.settings.export_dir.as_ref(), Some(&out));
        let files = app.export.outcome.as_ref().unwrap().as_ref().unwrap().files.clone();
        assert_eq!(files.len(), 7);
        assert!(files.iter().all(|f| f.exists()));
        assert!(has_line(&painted(&ctx, &mut app), "saved 7 files"));
        step(&ctx, &mut app, press(Key::Escape));
        assert!(!app.export.open && app.export.outcome.is_none());

        // Composited, at the dialog's default 1080p.
        app.export.open = true;
        app.export.composite = true;
        app.export.dest = Some(out.join("video"));
        app.trim = Trim { start: Some(1.0), end: Some(1.1) };
        let text = painted(&ctx, &mut app);
        assert!(has_line(&text, "1 × mp4 (h.264) + telemetry.csv") && has_line(&text, "4k"), "{text:?}");
        export::start(&mut app);
        wait(&mut app, "the composited export", &|a| a.export.running.is_none());
        let done = app.export.outcome.as_ref().unwrap().as_ref().unwrap();
        let t = crate::mp4::read_track(&done.files[0]).unwrap();
        assert_eq!((t.width, t.height, t.samples.len()), (1920, 1080, 4));

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn secondary(at: egui::Pos2, pressed: bool) -> InputEvent {
        InputEvent::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: Modifiers::NONE }
    }

    fn has_line(text: &[String], s: &str) -> bool {
        text.iter().any(|t| t == s)
    }

    fn pointer(events: Vec<InputEvent>) -> RawInput {
        RawInput { events, ..raw(1440.0, 960.0) }
    }

    fn button(at: egui::Pos2, pressed: bool) -> InputEvent {
        InputEvent::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    fn click(ctx: &Context, app: &mut App, at: egui::Pos2) {
        for events in [vec![InputEvent::PointerMoved(at)], vec![InputEvent::PointerMoved(at)], vec![button(at, true)], vec![button(at, false)], vec![]] {
            step(ctx, app, pointer(events));
        }
    }

    /// The map cell through the real thread, without tiles: tests never touch the network.
    #[test]
    fn the_map_cell_on_a_fixture() {
        let dir = std::env::temp_dir().join(format!("otc-ui-map-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fixture::make_fixture(&dir, 2, 64, 48).unwrap();
        let ctx = Context::default();
        let mut app = App::new(&ctx, Settings { last_folder: Some(dir.clone()), ..Settings::default() }, None);
        let wait = |app: &mut App, what: &str, done: &dyn Fn(&App) -> bool| {
            let t0 = Instant::now();
            loop {
                step(&ctx, app, raw(1440.0, 960.0));
                if done(app) {
                    return;
                }
                assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&mut app, "the scan", &|a| a.scan_rx.is_none());
        let at = |app: &App, kind: Kind| app.events.iter().position(|e| e.kind == kind).unwrap();
        let has = |text: &[String], s: &str| text.iter().any(|t| t == s);

        // No SEI: says so, and centers on event.json's estimate.
        app.open_now(&ctx, at(&app, Kind::Sentry));
        wait(&mut app, "the Sentry load", &|a| a.telemetry_rx.is_none());
        let text = painted(&ctx, &mut app);
        assert!(has(&text, "no gps data"), "{text:?}");
        assert!(has(&text, "52.5163, 13.3777"), "{text:?}");

        // With SEI: position, heading and attribution.
        app.open_now(&ctx, at(&app, Kind::Saved));
        wait(&mut app, "the Saved load", &|a| a.telemetry_rx.is_none());
        let text = painted(&ctx, &mut app);
        assert!(!has(&text, "no gps data"), "{text:?}");
        assert!(text.iter().any(|t| t.starts_with("52.") && t.contains(" · heading ") && t.ends_with('°')), "{text:?}");
        assert!(has(&text, "© OpenStreetMap contributors"), "{text:?}");

        // The attribution is a link, and the click goes to it rather than the map under it.
        let credit = pos2(map::WIDTH - 10.0, 960.0 - 27.0 - 5.0);
        let seeked = app.player.as_ref().unwrap().position;
        let mut commands = Vec::new();
        for events in [vec![InputEvent::PointerMoved(credit)], vec![InputEvent::PointerMoved(credit)], vec![button(credit, true)], vec![button(credit, false)], vec![]] {
            let mut out = ctx.run_ui(pointer(events), |ui| app.frame(ui));
            out.textures_delta.clear();
            commands.append(&mut out.platform_output.commands);
        }
        let opened: Vec<&str> = commands.iter().filter_map(|c| if let egui::OutputCommand::OpenUrl(u) = c { Some(u.url.as_str()) } else { None }).collect();
        assert_eq!(opened, ["https://www.openstreetmap.org/copyright"]);
        assert_eq!(app.player.as_ref().unwrap().position, seeked);

        // The toggles change what's persisted, and the attribution follows the layer.
        let h = bars::bottom_height(true);
        let top = 960.0 - h;
        click(&ctx, &mut app, pos2(56.0, top + 18.0));
        assert_eq!(app.settings.map_mode, "sat");
        let text = painted(&ctx, &mut app);
        assert!(text.iter().any(|t| t.starts_with("© Esri")), "{text:?}");
        click(&ctx, &mut app, pos2(map::WIDTH - 36.0, top + 18.0));
        assert!(app.settings.map_heading);

        // A click on the track finds that moment, heading-up or not, wherever the playhead is.
        let middle = pos2(map::WIDTH / 2.0, top + 36.0 + (h - 36.0 - 27.0) / 2.0);
        let (target, target_pos) = {
            let r = app.telemetry.as_ref().unwrap().records[100];
            (r.t, (r.tel.lat, r.tel.lon))
        };
        for heading_up in [true, false] {
            app.settings.map_heading = heading_up;
            app.player.as_mut().unwrap().seek(0.0);
            app.map.look_at(target_pos);
            click(&ctx, &mut app, middle);
            let got = app.player.as_ref().unwrap().position;
            assert!((got - target).abs() < 0.5, "heading-up {heading_up}: seeked to {got}, wanted {target}");
        }
        // The follow button takes the click, not the map under it, and brings the car back.
        assert!(has(&painted(&ctx, &mut app), "follow"));
        let seeked = app.player.as_ref().unwrap().position;
        click(&ctx, &mut app, pos2(map::WIDTH - 34.0, top + 53.0));
        assert_eq!(app.player.as_ref().unwrap().position, seeked);
        assert!(!has(&painted(&ctx, &mut app), "follow"));

        // A drag moves the map off the car and doesn't seek; follow comes back.
        let before = app.player.as_ref().unwrap().position;
        let away = middle + vec2(140.0, 90.0);
        let drag = [vec![InputEvent::PointerMoved(middle)], vec![button(middle, true)], vec![InputEvent::PointerMoved(middle + vec2(20.0, 10.0))], vec![InputEvent::PointerMoved(away)], vec![button(away, false)], vec![]];
        assert!(!has(&painted(&ctx, &mut app), "follow"));
        for events in drag {
            step(&ctx, &mut app, pointer(events));
        }
        let text = painted(&ctx, &mut app);
        assert!(has(&text, "follow"), "{text:?}");
        assert_eq!(app.player.as_ref().unwrap().position, before);

        // Now the middle of the map is off the track: a click there doesn't seek either.
        click(&ctx, &mut app, middle);
        assert_eq!(app.player.as_ref().unwrap().position, before);

        // Another event starts on its car again.
        app.open_now(&ctx, at(&app, Kind::Saved));
        assert!(!has(&painted(&ctx, &mut app), "follow"));

        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
