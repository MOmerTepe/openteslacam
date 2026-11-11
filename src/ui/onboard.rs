//! The welcome on first launch: where footage should live, then what's where. Two pages, skippable at
//! any point, and never shown again; the second page is the `?` sheet, so it can always be read later.

use super::App;
use super::export::{BTN_H, W, check, footer, row};
use super::theme::{self, BG, Face, Icon, LINE2, Look, TEXT2, TEXT3};
use super::{import, keys};
use eframe::egui::{Color32, Context, Frame, Id, Margin, Modal, Ui};

/// Fits `next`, `skip`, `back` and `done`; design rule 3 makes them equal.
const BTN_W: f32 = 72.0;
const INTRO: &str = "OpenTeslaCam plays a TeslaCam drive as it is; nothing needs setting up. If you'd rather keep your footage on this PC, choose a folder for it here. You can do it later too, with import in the library panel.";

#[derive(Default)]
pub struct Dialog {
    pub open: bool,
    /// 0: the library, 1: the controls.
    pub(super) page: usize,
}

enum Step {
    None,
    Finish,
    Page(usize),
    Pick,
}

pub fn modal(app: &mut App, ctx: &Context) {
    if !app.onboard.open {
        return;
    }
    let frame = Frame::NONE.fill(BG).stroke(theme::line(LINE2)).inner_margin(Margin::same(22));
    let backdrop = Color32::from_rgba_unmultiplied_const(10, 10, 9, 184);
    let r = Modal::new(Id::unique("welcome")).frame(frame).backdrop_color(backdrop).show(ctx, |ui| contents(ui, app));
    match r.inner {
        Step::Page(p) => app.onboard.page = p,
        Step::Pick => {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                app.settings.library_dir = Some(p);
            }
        }
        Step::None if !r.should_close() => {}
        // Esc and a click outside skip it for good, like the buttons.
        Step::Finish | Step::None => {
            app.onboard.open = false;
            app.settings.onboarded = true;
        }
    }
}

fn contents(ui: &mut Ui, app: &mut App) -> Step {
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut step = Step::None;
    let page = app.onboard.page;
    let head = row(ui, BTN_H);
    let title = if page == 0 { "WELCOME · 1 OF 2" } else { "WELCOME · 2 OF 2" };
    theme::spaced(ui.painter(), head.left_center(), title, theme::medium(11.0), TEXT3, 1.54, W - 40.0);
    if theme::button(ui, head.with_min_x(head.max.x - BTN_H), Face::Icon(Icon::Close, 14.0), Look::Bare, true, "Close").clicked() {
        step = Step::Finish;
    }
    ui.add_space(18.0);

    if page == 0 {
        let g = ui.painter().layout(INTRO.to_owned(), theme::sans(13.0), TEXT2, W);
        let r = row(ui, g.size().y);
        ui.painter().galley(r.min, g, TEXT2);
        ui.add_space(18.0);
        let chosen = app.settings.library_dir.is_some();
        if import::path_row(ui, "keep in", app.settings.library_dir.as_deref(), Some(if chosen { "change" } else { "choose" })) {
            step = Step::Pick;
        }
        ui.add_space(18.0);
        let r = row(ui, 16.0);
        check(ui, r, "import by itself when a Tesla drive is plugged in", &mut app.settings.import_auto);
        ui.add_space(10.0);
        let r = row(ui, 16.0);
        check(ui, r, "clear each file from the drive once its copy is checked", &mut app.settings.import_clear);
    } else {
        keys::guide(ui, W);
    }
    ui.add_space(18.0);

    let mut r = footer(ui);
    let (primary, other, next) = if page == 0 { ("next", "skip", Step::Page(1)) } else { ("done", "back", Step::Finish) };
    if theme::button(ui, r.right(BTN_W, 8.0), Face::Text(primary), Look::Primary, true, primary).clicked() {
        step = next;
    }
    if theme::text_button(ui, r.right(BTN_W, 8.0), other, true).clicked() {
        step = if page == 0 { Step::Finish } else { Step::Page(0) };
    }
    let what = match (page, app.settings.library_dir.is_some()) {
        (0, false) => "no folder: footage plays from the drive",
        (0, true) => "footage is copied here when you import",
        _ => "this page is behind the ? button",
    };
    theme::label(ui.painter(), r.rest.left_center(), what, theme::mono(11.0), TEXT3, r.rest.width());
    step
}
