//! `--view clip.mp4`: one camera through the real pipeline (Player clock -> CamWorker -> texture).
//! Space toggles play; the full viewer replaces this.

use crate::decode::{CamWorker, Mode};
use crate::library::{Event, Kind, Segment};
use crate::player::Player;
use eframe::egui::{self, TextureHandle, TextureOptions};
use std::path::Path;
use std::time::Instant;

pub fn view(path: &Path) -> Result<(), String> {
    crate::mp4::read_track(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut files: [Option<_>; 6] = Default::default();
    files[0] = Some(path.to_path_buf());
    let ev = Event {
        kind: Kind::Folder,
        name: path.display().to_string(),
        dir: path.parent().unwrap_or(path).to_path_buf(),
        segments: vec![Segment { start: 0, files }],
        info: None,
        thumb: None,
        encrypted: false,
    };
    let title = format!("OpenTeslaCam - {}", path.display());
    eframe::run_native(
        &title,
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            let cam = CamWorker::spawn("view", cc.egui_ctx.clone());
            cam.open(vec![ev.segments[0].files[0].clone()]);
            let mut player = Player::open(&ev);
            player.play();
            Ok(Box::new(Viewer { cam, player, tex: None }))
        }),
    )
    .map_err(|e| e.to_string())
}

struct Viewer {
    cam: CamWorker,
    player: Player,
    tex: Option<TextureHandle>,
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        if ui.input(|i| i.key_pressed(egui::Key::Space)) {
            self.player.toggle();
        }
        self.player.tick(Instant::now());
        if let Some((seg, local)) = self.player.target() {
            self.cam.seek(seg, local, if self.player.playing { Mode::Play } else { Mode::Exact });
        }
        if self.player.playing {
            ui.ctx().request_repaint();
        }
        match self.cam.take() {
            Some(Ok(f)) => match &mut self.tex {
                Some(t) => t.set(f.image, TextureOptions::LINEAR),
                None => self.tex = Some(ui.ctx().load_texture("camera", f.image, TextureOptions::LINEAR)),
            },
            Some(Err(e)) => eprintln!("{e}"),
            None => {}
        }
        if let Some(t) = &self.tex {
            ui.centered_and_justified(|ui| ui.add(egui::Image::new((t.id(), t.size_vec2())).shrink_to_fit()));
        }
    }
}
