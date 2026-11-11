//! The update dialog: what the newer release brings, and the choice to take it now or later. It opens
//! by itself once per version; after "later" the top bar's button brings it back.

use super::App;
use super::export::{BTN_H, W, footer, progress, row};
use super::theme::{self, BG, Face, Icon, LINE, LINE2, Look, TEXT, TEXT2, TEXT3};
use crate::update::{self, Release, State};
use eframe::egui::{Color32, Context, Frame, Id, Margin, Modal, OpenUrl, ScrollArea, Ui};
use std::time::Duration;

/// Fits `update now`; design rule 3 gives `later` the same width.
const BTN_W: f32 = 96.0;
const NOTES_H: f32 = 300.0;

enum Step {
    None,
    Close,
    Install,
    Page,
}

pub fn modal(app: &mut App, ctx: &Context) {
    let Some(release) = update::available() else { return };
    // Once per version, and not on top of another dialog.
    if app.settings.update_seen != release.version && !app.export.open && !app.shortcuts_open && !app.onboard.open {
        app.settings.update_seen = release.version.clone();
        app.update_open = true;
    }
    if !app.update_open {
        return;
    }
    let state = update::state();
    let busy = matches!(state, State::Downloading(_) | State::Installing | State::Restarting);
    let frame = Frame::NONE.fill(BG).stroke(theme::line(LINE2)).inner_margin(Margin::same(22));
    let backdrop = Color32::from_rgba_unmultiplied_const(10, 10, 9, 184);
    // An export would be cut off by the restart.
    let exporting = app.export.running.is_some();
    let r = Modal::new(Id::unique("update")).frame(frame).backdrop_color(backdrop).show(ctx, |ui| contents(ui, release, &state, exporting));
    match r.inner {
        Step::Install => update::install(release, ctx.clone()),
        Step::Page => ctx.open_url(OpenUrl::new_tab(update::RELEASE_PAGE)),
        Step::Close => app.update_open = false,
        Step::None if !busy && r.should_close() => app.update_open = false,
        Step::None => {}
    }
    if busy {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

/// Release notes are Markdown; without a renderer, the marks go and the words stay.
fn plain(notes: &str) -> String {
    let line = |l: &str| l.trim_end().trim_start_matches('#').trim_start().replace("**", "").replace('`', "");
    notes.lines().map(line).collect::<Vec<_>>().join("\n").trim().to_owned()
}

fn contents(ui: &mut Ui, release: &Release, state: &State, exporting: bool) -> Step {
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut step = Step::None;
    let busy = matches!(state, State::Downloading(_) | State::Installing | State::Restarting);
    let head = row(ui, BTN_H);
    theme::spaced(ui.painter(), head.left_center(), "UPDATE AVAILABLE", theme::medium(11.0), TEXT3, 1.54, W - 40.0);
    if theme::button(ui, head.with_min_x(head.max.x - BTN_H), Face::Icon(Icon::Close, 14.0), Look::Bare, !busy, "Close").clicked() {
        step = Step::Close;
    }
    ui.add_space(18.0);
    let r = row(ui, 22.0);
    let versions = format!("{} → {}", env!("CARGO_PKG_VERSION"), release.version);
    theme::label(ui.painter(), r.left_center(), &versions, theme::medium(15.0), TEXT, W);
    ui.add_space(14.0);
    let r = row(ui, 1.0);
    ui.painter().hline(r.x_range(), r.min.y + 0.5, theme::line(LINE));
    ui.add_space(18.0);

    ScrollArea::vertical().max_height(NOTES_H).show(ui, |ui| {
        let g = ui.painter().layout(plain(&release.notes), theme::sans(13.0), TEXT2, W - 14.0);
        let r = row(ui, g.size().y);
        ui.painter().galley(r.min, g, TEXT2);
    });
    ui.add_space(18.0);

    if let State::Downloading(f) = state {
        progress(ui, *f);
        ui.add_space(18.0);
    }
    let mut r = footer(ui);
    let installs = update::can_install(release);
    let what = match state {
        State::Downloading(_) => "downloading the installer".to_owned(),
        State::Installing => "installing…".to_owned(),
        State::Restarting => "restarting…".to_owned(),
        State::Failed(e) => format!("update failed: {e}"),
        State::Idle if installs && exporting => "finish the export first".to_owned(),
        State::Idle if installs => "installs here and restarts the app".to_owned(),
        State::Idle => "opens the download page".to_owned(),
    };
    if !busy {
        // After a failure the page is the way that still works.
        let (label, next) = if installs && *state == State::Idle { ("update now", Step::Install) } else { ("download", Step::Page) };
        if theme::button(ui, r.right(BTN_W, 8.0), Face::Text(label), Look::Primary, !(installs && exporting), label).clicked() {
            step = next;
        }
        if theme::text_button(ui, r.right(BTN_W, 8.0), "later", true).clicked() {
            step = Step::Close;
        }
    }
    theme::label(ui.painter(), r.rest.left_center(), &what, theme::mono(11.0), TEXT3, r.rest.width());
    step
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_lose_their_markdown_marks() {
        let notes = "Intro line.\r\n\r\n## What's new\n\n- **Steering:** reads `left` right.\n- Two\n\n";
        assert_eq!(plain(notes), "Intro line.\n\nWhat's new\n\n- Steering: reads left right.\n- Two");
    }
}
