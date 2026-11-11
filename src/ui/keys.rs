//! Keyboard shortcuts (legacy parity) as a pure key → action map, plus the `?` modal.

use super::theme::{self, BG, Icon, LINE2, TEXT, TEXT2, TEXT3};
use crate::player::Player;
use eframe::egui::{Color32, Context, Frame, Id, Key, Margin, Modal, Modifiers, Sense, Ui, pos2, vec2};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    TogglePlay,
    Seek(f64),
    Frame(i32),
    PrevSegment,
    NextSegment,
    Home,
    End,
    ExitFocus,
    Shortcuts,
    TrimIn,
    TrimOut,
    Mark,
}

impl Action {
    /// Held-down keys repeat only where repeating means something.
    pub fn repeats(self) -> bool {
        matches!(self, Action::Seek(_) | Action::Frame(_))
    }

    /// The part that acts on the clock.
    pub fn on_player(self, p: &mut Player) {
        match self {
            Action::TogglePlay => p.toggle(),
            Action::Seek(dt) => p.seek_by(dt),
            Action::Frame(dir) => p.step_frame(dir),
            Action::PrevSegment => p.prev_segment(),
            Action::NextSegment => p.next_segment(),
            Action::Home => p.home(),
            Action::End => p.end(),
            Action::ExitFocus | Action::Shortcuts | Action::TrimIn | Action::TrimOut | Action::Mark => {}
        }
    }
}

pub fn action_for(key: Key, m: Modifiers) -> Option<Action> {
    // Ctrl/Alt combinations belong to the OS and to text fields, except that some Mac layouts type
    // these characters with Option.
    let typed_with_alt = matches!(key, Key::OpenBracket | Key::CloseBracket | Key::Questionmark);
    if m.ctrl || m.command || m.mac_cmd || (m.alt && !typed_with_alt) {
        return None;
    }
    let seek = if m.shift { 30.0 } else { 5.0 };
    Some(match key {
        Key::Space => Action::TogglePlay,
        Key::ArrowLeft => Action::Seek(-seek),
        Key::ArrowRight => Action::Seek(seek),
        Key::Comma => Action::Frame(-1),
        Key::Period => Action::Frame(1),
        Key::OpenBracket | Key::PageUp => Action::PrevSegment,
        Key::CloseBracket | Key::PageDown => Action::NextSegment,
        Key::Home => Action::Home,
        Key::End => Action::End,
        Key::Escape => Action::ExitFocus,
        Key::Questionmark => Action::Shortcuts,
        Key::I => Action::TrimIn,
        Key::O => Action::TrimOut,
        Key::M => Action::Mark,
        _ => return None,
    })
}

const SHORTCUTS: [(&str, &str); 11] = [
    ("Space", "Play / pause"),
    ("← →", "Seek ±5 seconds"),
    ("Shift + ← →", "Seek ±30 seconds"),
    (", .", "Step one frame (when paused)"),
    ("[ ] or PgUp PgDn", "Previous / next segment"),
    ("Home", "Jump to start"),
    ("End", "Jump to end"),
    ("I O", "Set In / Out (again to clear)"),
    ("M", "Mark where it happened: In–Out, or here"),
    ("?", "Show / hide this list"),
    ("Esc", "Close this list / exit focus"),
];

pub fn shortcuts_modal(ctx: &Context, open: &mut bool) {
    if !*open {
        return;
    }
    let frame = Frame::NONE.fill(BG).stroke(theme::line(LINE2)).inner_margin(Margin::same(22));
    let backdrop = Color32::from_rgba_unmultiplied_const(10, 10, 9, 184);
    let r = Modal::new(Id::unique("shortcuts")).frame(frame).backdrop_color(backdrop).show(ctx, contents);
    if r.inner || r.should_close() {
        *open = false;
    }
}

/// What's where, in the order a first session meets it.
const TIPS: [(&str, &str); 5] = [
    ("timeline", "Click or drag it to move through the event. Its lanes show speed, pedals, steering and Autopilot."),
    ("cameras", "The top bar switches layouts. Each camera has a button to show it alone; Esc brings the others back."),
    ("mark", "Press M where it happened. The event opens there next time."),
    ("export", "Set In and Out with I and O, then export that part."),
    ("library", "import copies a Tesla drive to this PC. Right-click an event there to archive or delete it; the buttons beside Out trim it or drop cameras."),
];

/// The tips and the shortcuts, `w` wide: the `?` sheet, and the welcome's second page.
pub(super) fn guide(ui: &mut Ui, w: f32) {
    for (word, what) in TIPS {
        let g = ui.painter().layout(what.to_owned(), theme::sans(13.0), TEXT2, w - 160.0);
        let (row, _) = ui.allocate_exact_size(vec2(w, g.size().y + 8.0), Sense::hover());
        theme::label(ui.painter(), pos2(row.min.x, row.min.y + 13.0), word, theme::mono(12.0), TEXT, 150.0);
        ui.painter().galley(pos2(row.min.x + 160.0, row.min.y + 4.0), g, TEXT2);
    }
    ui.add_space(10.0);
    for (keys, what) in SHORTCUTS {
        let (row, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
        let y = row.center().y;
        theme::label(ui.painter(), pos2(row.min.x, y), keys, theme::mono(12.0), TEXT, 150.0);
        theme::label(ui.painter(), pos2(row.min.x + 160.0, y), what, theme::sans(13.0), TEXT2, w - 160.0);
    }
}

/// True when the close button was clicked.
fn contents(ui: &mut Ui) -> bool {
    const W: f32 = 480.0;
    let (head, _) = ui.allocate_exact_size(vec2(W, 32.0), Sense::hover());
    theme::spaced(ui.painter(), head.left_center(), "CONTROLS", theme::medium(11.0), TEXT3, 1.54, W - 40.0);
    let close = head.with_min_x(head.max.x - 32.0);
    let closed = theme::button(ui, close, theme::Face::Icon(Icon::Close, 14.0), theme::Look::Bare, true, "Close").clicked();
    ui.add_space(6.0);
    guide(ui, W);
    closed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::SegTime;

    #[test]
    fn keys_map_like_the_legacy_app() {
        let none = Modifiers::NONE;
        let shift = Modifiers::SHIFT;
        let cases = [
            (Key::Space, none, Action::TogglePlay),
            (Key::ArrowLeft, none, Action::Seek(-5.0)),
            (Key::ArrowRight, none, Action::Seek(5.0)),
            (Key::ArrowLeft, shift, Action::Seek(-30.0)),
            (Key::ArrowRight, shift, Action::Seek(30.0)),
            (Key::Comma, none, Action::Frame(-1)),
            (Key::Period, none, Action::Frame(1)),
            (Key::OpenBracket, none, Action::PrevSegment),
            (Key::PageUp, none, Action::PrevSegment),
            (Key::CloseBracket, none, Action::NextSegment),
            (Key::PageDown, none, Action::NextSegment),
            (Key::Home, none, Action::Home),
            (Key::End, none, Action::End),
            (Key::Escape, none, Action::ExitFocus),
            (Key::Questionmark, shift, Action::Shortcuts),
            (Key::I, none, Action::TrimIn),
            (Key::O, none, Action::TrimOut),
            (Key::M, none, Action::Mark),
        ];
        for (k, m, a) in cases {
            assert_eq!(action_for(k, m), Some(a), "{k:?}");
        }
        assert_eq!(action_for(Key::Space, Modifiers::CTRL), None);
        assert_eq!(action_for(Key::I, Modifiers::ALT), None);
        assert_eq!(action_for(Key::OpenBracket, Modifiers::ALT), Some(Action::PrevSegment));
        assert_eq!(action_for(Key::CloseBracket, Modifiers::ALT), Some(Action::NextSegment));
        assert_eq!(action_for(Key::OpenBracket, Modifiers::CTRL), None);
        assert_eq!(action_for(Key::A, none), None);
    }

    #[test]
    fn only_seeks_and_steps_repeat() {
        assert!(Action::Seek(5.0).repeats() && Action::Frame(1).repeats());
        assert!(!Action::TogglePlay.repeats() && !Action::Shortcuts.repeats() && !Action::NextSegment.repeats());
    }

    #[test]
    fn actions_drive_the_clock() {
        let seg = |start: f64| SegTime { start, dur: 60.0, frames: vec![] };
        let mut p = Player::new(vec![seg(0.0), seg(60.0)], 0);
        Action::Seek(30.0).on_player(&mut p);
        assert_eq!(p.position, 30.0);
        Action::NextSegment.on_player(&mut p);
        assert_eq!(p.position, 60.0);
        Action::PrevSegment.on_player(&mut p);
        assert_eq!(p.position, 0.0);
        Action::End.on_player(&mut p);
        assert_eq!(p.position, 119.0);
        Action::Home.on_player(&mut p);
        assert_eq!(p.position, 0.0);
        Action::TogglePlay.on_player(&mut p);
        assert!(p.playing);
    }
}
