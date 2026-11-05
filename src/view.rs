//! `--view clip.mp4`: Phase 0 check that decoded frames reach the screen as an egui texture.

use crate::decode::{CamDecoder, to_rgba};
use crate::mp4::{self, Track};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use openh264::decoder::DecodedYUV;
use std::fs::File;
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

pub fn view(path: &Path) -> Result<(), String> {
    let track = mp4::read_track(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let path = path.to_path_buf();
    let title = format!("OpenTeslaCam - {}", path.display());
    eframe::run_native(
        &title,
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            // One frame of slack: the decoder runs ahead by at most one picture.
            let (tx, rx) = sync_channel(1);
            let ctx = cc.egui_ctx.clone();
            thread::spawn(move || {
                if let Err(e) = play(&path, &track, &tx, &ctx) {
                    eprintln!("{}: {e}", path.display());
                }
            });
            Ok(Box::new(Viewer { rx, tex: None }))
        }),
    )
    .map_err(|e| e.to_string())
}

/// Decodes the whole clip, sending each picture at its presentation time.
fn play(path: &Path, track: &Track, tx: &SyncSender<ColorImage>, ctx: &egui::Context) -> Result<(), String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut dec = CamDecoder::new(track)?;
    // Pictures come out in display order, so the k-th one is due at the k-th smallest pts.
    let mut pts: Vec<u64> = track.samples.iter().map(|s| s.pts).collect();
    pts.sort_unstable();
    let mut due = pts.into_iter();
    let (start, mut rgba) = (Instant::now(), Vec::new());
    // Returns false once the window is gone.
    let mut show = |yuv: &DecodedYUV| {
        let (w, h) = to_rgba(yuv, &mut rgba);
        let t = Duration::from_secs_f64(due.next().unwrap_or(0) as f64 / track.timescale as f64);
        thread::sleep((start + t).saturating_duration_since(Instant::now()));
        let sent = tx.send(ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba)).is_ok();
        ctx.request_repaint();
        sent
    };
    for s in &track.samples {
        let data = mp4::read_sample(&mut f, s).map_err(|e| e.to_string())?;
        if let Some(yuv) = dec.decode(&data, s.key)?
            && !show(&yuv)
        {
            return Ok(());
        }
    }
    for yuv in dec.flush()? {
        show(&yuv);
    }
    Ok(())
}

struct Viewer {
    rx: Receiver<ColorImage>,
    tex: Option<TextureHandle>,
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        if let Some(img) = self.rx.try_iter().last() {
            match &mut self.tex {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => self.tex = Some(ui.ctx().load_texture("camera", img, TextureOptions::LINEAR)),
            }
        }
        if let Some(t) = &self.tex {
            ui.centered_and_justified(|ui| ui.add(egui::Image::new((t.id(), t.size_vec2())).shrink_to_fit()));
        }
    }
}
