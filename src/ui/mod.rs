//! The viewer window: app state and the per-frame order of panels.

mod bars;
mod events;
mod grid;
mod keys;
mod map;
mod settings;
mod theme;
mod timeline;
mod vehicle;

use crate::decode::{CamWorker, Mode};
use crate::library::{self, CAMERAS, Event};
use crate::player::Player;
use eframe::egui::{self, Align2, CentralPanel, ColorImage, Context, Frame, TextureHandle, TextureOptions, Ui};
use events::Filter;
use grid::Tile;
use keys::Action;
use settings::Settings;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::Instant;

pub fn run(folder: Option<PathBuf>) -> Result<(), String> {
    // shortcut: rows don't wrap like the mockup's flex-wrap, the minimum width keeps them from colliding; wrap if people want smaller windows.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_app_id("openteslacam").with_inner_size([1440.0, 960.0]).with_min_inner_size([1280.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "openteslacam",
        options,
        Box::new(move |cc| {
            let settings = cc.storage.and_then(|s| s.get_string(settings::KEY)).map_or_else(Settings::default, |s| Settings::from_json(&s));
            Ok(Box::new(App::new(&cc.egui_ctx, settings, folder)))
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
    /// Indexed like `CAMERAS`.
    workers: Vec<CamWorker>,
    tiles: [Tile; 6],
    /// The pointer holds the scrubber.
    scrubbing: bool,
    focus: Option<usize>,
    editor: grid::Editor,
    shortcuts_open: bool,
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
            workers: CAMERAS.iter().map(|c| CamWorker::spawn(c, ctx.clone())).collect(),
            tiles: Default::default(),
            scrubbing: false,
            focus: None,
            editor: grid::Editor::default(),
            shortcuts_open: false,
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

    fn close_event(&mut self) {
        for w in &self.workers {
            w.open(Vec::new());
        }
        self.tiles = Default::default();
        (self.current, self.player, self.focus) = (None, None, None);
    }

    fn open_event(&mut self, i: usize) {
        let ev = &self.events[i];
        if ev.encrypted {
            return;
        }
        let mut player = Player::open(ev);
        player.seek(events::start_position(ev));
        if let Some(old) = &self.player {
            player.rate = old.rate;
        }
        for (cam, w) in self.workers.iter().enumerate() {
            w.open(ev.segments.iter().map(|s| s.files[cam].clone()).collect());
        }
        self.tiles = Default::default();
        self.focus = None;
        self.current = Some(i);
        self.player = Some(player);
    }

    /// A dropped folder (a stray file must not close what's open, as in the legacy app), the
    /// finished scan, and thumbnails that have arrived.
    fn poll(&mut self, ctx: &Context) {
        if let Some(p) = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).find(|p| p.is_dir())) {
            self.open_folder(ctx, p);
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
                        self.open_event(i);
                    }
                }
                Err(e) => self.scan_error = Some(e.to_string()),
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
        if ctx.text_edit_focused() {
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
            if !self.shortcuts_open {
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
            // Phase 4: set the trim range.
            Action::TrimIn | Action::TrimOut => {}
            a => {
                if let Some(p) = &mut self.player {
                    a.on_player(p);
                }
            }
        }
    }

    /// Runs after the panels that can move the clock, so a seek reaches the decoders this frame.
    // shortcut: hidden and unfocused cameras decode too; skip them if HW4 at 4x can't keep up (PLAN Phase 0).
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
        for (cam, (w, tile)) in self.workers.iter().zip(&mut self.tiles).enumerate() {
            if let Some((seg, local)) = target {
                w.seek(seg, local, mode);
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
    use std::time::{Duration, Instant};

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
        app.open_event(locked);
        assert_eq!(app.current, current);
        let saved = app.events.iter().position(|e| e.kind == Kind::Saved).expect("the fixture has one");
        app.open_event(saved);
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
}
