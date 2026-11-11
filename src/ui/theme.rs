//! Design rule 2 and 3: fonts, monochrome Visuals, and the shared hand-painted widgets. Every widget
//! takes the rect it fills; `Row` hands those rects out, so rows stay exact in either direction.

use eframe::egui::epaint::Shadow;
use eframe::egui::text::{LayoutJob, TextWrapping};
use eframe::egui::{
    Align2, Color32, Context, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Painter, Pos2, Rect, Response, Role, Sense,
    Shape, Stroke, StrokeKind, Style, TextStyle, Theme, Ui, Visuals, WidgetInfo, pos2, style, vec2,
};
use std::sync::Arc;

pub const BG: Color32 = Color32::from_rgb(0x14, 0x14, 0x13);
pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe6, 0xe1);
pub const TEXT2: Color32 = Color32::from_rgb(0xb0, 0xb0, 0xa8);
pub const TEXT3: Color32 = Color32::from_rgb(0x93, 0x93, 0x8b);
/// Inactive toggles.
pub const OFF: Color32 = Color32::from_rgb(0x83, 0x83, 0x7c);
pub const LINE: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x27);
pub const LINE2: Color32 = Color32::from_rgb(0x4a, 0x4a, 0x45);
pub const VIDEO_BG: Color32 = Color32::from_rgb(0x0c, 0x0c, 0x0b);
pub const HOVER: Color32 = Color32::from_rgb(0x1b, 0x1b, 0x19);
pub const SELECTED: Color32 = Color32::from_rgb(0x20, 0x20, 0x1e);

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// The mockup's 500 and 600 weights.
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("mono-medium".into()))
}

pub fn sans(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn line(color: Color32) -> Stroke {
    Stroke::new(1.0, color)
}

pub fn install(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    let faces: [(&str, &'static [u8]); 3] = [
        ("plex-mono", include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf")),
        ("plex-mono-medium", include_bytes!("../../assets/fonts/IBMPlexMono-Medium.ttf")),
        ("plex-sans", include_bytes!("../../assets/fonts/IBMPlexSans-Regular.ttf")),
    ];
    for (name, bytes) in faces {
        fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }
    let mono_family = fonts.families.entry(FontFamily::Monospace).or_default();
    mono_family.insert(0, "plex-mono".to_owned());
    let mut medium_family = mono_family.clone();
    medium_family.insert(0, "plex-mono-medium".to_owned());
    fonts.families.insert(FontFamily::Name("mono-medium".into()), medium_family);
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "plex-sans".to_owned());
    ctx.set_fonts(fonts);
    ctx.set_theme(Theme::Dark);
    ctx.style_mut_of(Theme::Dark, apply_style);
}

fn apply_style(s: &mut Style) {
    s.visuals = visuals();
    s.text_styles = [
        (TextStyle::Small, mono(10.0)),
        (TextStyle::Body, sans(13.0)),
        (TextStyle::Monospace, mono(12.0)),
        (TextStyle::Button, mono(11.5)),
        (TextStyle::Heading, medium(15.0)),
    ]
    .into();
    s.spacing.scroll = style::ScrollStyle::solid();
    // Hard edges: no gradient over the clipped rows.
    s.spacing.scroll.fade.strength = 0.0;
}

/// Only grays, no radius, no shadows.
pub fn visuals() -> Visuals {
    let w = |bg: Color32, border: Color32, fg: Color32| style::WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: line(border),
        corner_radius: CornerRadius::ZERO,
        fg_stroke: line(fg),
        expansion: 0.0,
    };
    let mut v = Visuals::dark();
    v.widgets = style::Widgets {
        noninteractive: w(BG, LINE, TEXT2),
        inactive: w(LINE2, LINE2, TEXT),
        hovered: w(TEXT3, TEXT3, TEXT),
        active: w(TEXT2, TEXT, TEXT),
        open: w(LINE, LINE2, TEXT),
    };
    v.selection = style::Selection { bg_fill: LINE2, stroke: line(TEXT) };
    v.hyperlink_color = TEXT;
    v.warn_fg_color = TEXT;
    v.error_fg_color = TEXT;
    v.panel_fill = BG;
    v.window_fill = BG;
    v.faint_bg_color = HOVER;
    v.extreme_bg_color = VIDEO_BG;
    v.code_bg_color = HOVER;
    v.window_stroke = line(LINE2);
    v.window_corner_radius = CornerRadius::ZERO;
    v.menu_corner_radius = CornerRadius::ZERO;
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow::NONE;
    v.window_highlight_topmost = false;
    v.text_cursor.stroke.color = TEXT;
    v
}

/// Takes fixed-width rects from both ends of a row; `rest` is what's left between them.
pub struct Row {
    pub rest: Rect,
}

impl Row {
    pub fn new(rect: Rect) -> Row {
        Row { rest: rect }
    }

    /// `w` from the left end, then `gap` skipped.
    pub fn left(&mut self, w: f32, gap: f32) -> Rect {
        let r = self.rest.with_max_x(self.rest.min.x + w);
        self.rest.min.x += w + gap;
        r
    }

    /// `w` from the right end, then `gap` skipped.
    pub fn right(&mut self, w: f32, gap: f32) -> Rect {
        let r = self.rest.with_min_x(self.rest.max.x - w);
        self.rest.max.x -= w + gap;
        r
    }
}

/// One line of text, vertically centered on `left_center`, cut with … at `max_w`. Returns its width.
pub fn label(p: &Painter, left_center: Pos2, s: &str, font: FontId, color: Color32, max_w: f32) -> f32 {
    spaced(p, left_center, s, font, color, 0.0, max_w)
}

/// `label` with extra letter spacing (the uppercase captions).
pub fn spaced(p: &Painter, left_center: Pos2, s: &str, font: FontId, color: Color32, spacing: f32, max_w: f32) -> f32 {
    let mut job = LayoutJob::simple_singleline(s.to_owned(), font, color);
    job.sections[0].format.extra_letter_spacing = spacing;
    job.wrap = TextWrapping::truncate_at_width(max_w.max(1.0));
    let g = p.layout_job(job);
    let width = g.size().x;
    p.galley(pos2(left_center.x, left_center.y - g.size().y / 2.0), g, color);
    width
}

pub fn text_width(ui: &Ui, s: &str, font: FontId) -> f32 {
    ui.painter().layout_no_wrap(s.to_owned(), font, TEXT).size().x
}

#[derive(Clone, Copy, PartialEq)]
pub enum Look {
    Normal,
    /// Filled: the one primary action of a row. Same size as the others (design rule 3).
    Primary,
    /// No border: panel chrome like the collapse button.
    Bare,
    /// Something that can be added back.
    Dashed,
}

#[derive(Clone, Copy)]
pub enum Face<'a> {
    /// With its size in px.
    Icon(Icon, f32),
    Text(&'a str),
}

pub fn button(ui: &mut Ui, rect: Rect, face: Face, look: Look, enabled: bool, name: &str) -> Response {
    let r = ui.allocate_rect(rect, if enabled { Sense::click() } else { Sense::hover() });
    r.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, name));
    let hot = enabled && r.hovered();
    let down = hot && r.is_pointer_button_down_on();
    let (fill, border, fg) = match look {
        _ if !enabled => (Color32::TRANSPARENT, Some(LINE), LINE2),
        Look::Primary => (if hot { TEXT2 } else { TEXT }, None, BG),
        Look::Bare => (if down { LINE } else { Color32::TRANSPARENT }, None, if hot { TEXT } else { TEXT3 }),
        Look::Dashed => (if hot { HOVER } else { Color32::TRANSPARENT }, None, TEXT),
        Look::Normal => (
            if down {
                LINE
            } else if hot {
                HOVER
            } else {
                Color32::TRANSPARENT
            },
            Some(if hot { TEXT3 } else { LINE2 }),
            TEXT,
        ),
    };
    let p = ui.painter();
    p.rect_filled(rect, 0.0, fill);
    if let Some(b) = border {
        p.rect_stroke(rect, 0.0, line(b), StrokeKind::Inside);
    }
    if enabled && look == Look::Dashed {
        dashed_rect(p, rect, LINE2);
    }
    match face {
        Face::Icon(i, size) => paint_icon(p, rect.center(), i, size, fg),
        Face::Text(t) => {
            let font = if look == Look::Primary { medium(11.5) } else { mono(11.5) };
            p.text(rect.center(), Align2::CENTER_CENTER, t, font, fg);
        }
    }
    if enabled { r.on_hover_cursor(CursorIcon::PointingHand) } else { r }
}

pub fn text_button(ui: &mut Ui, rect: Rect, text: &str, enabled: bool) -> Response {
    button(ui, rect, Face::Text(text), Look::Normal, enabled, text)
}

/// Width for a group of equal toggle cells.
pub fn toggles_width(ui: &Ui, labels: &[&str], gap: f32) -> f32 {
    let widest = labels.iter().map(|l| text_width(ui, l, mono(11.5))).fold(0.0, f32::max).ceil();
    widest * labels.len() as f32 + gap * (labels.len().saturating_sub(1)) as f32
}

/// Equal cells across `rect`, the active one underlined. Returns the clicked index.
pub fn toggles<S: AsRef<str>>(ui: &mut Ui, rect: Rect, labels: &[S], active: Option<usize>, gap: f32) -> Option<usize> {
    let n = labels.len() as f32;
    let w = (rect.width() - gap * (n - 1.0)) / n;
    let mut row = Row::new(rect);
    let mut clicked = None;
    for (i, l) in labels.iter().enumerate() {
        let l = l.as_ref();
        let cell = row.left(w, gap);
        let on = active == Some(i);
        let r = ui.allocate_rect(cell, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        r.widget_info(|| WidgetInfo::selected(Role::Button, true, on, l));
        let color = if on {
            TEXT
        } else if r.hovered() {
            TEXT2
        } else {
            OFF
        };
        let p = ui.painter();
        p.text(cell.center(), Align2::CENTER_CENTER, l, mono(11.5), color);
        if on {
            p.hline(cell.x_range(), (cell.center().y + 10.0).round() - 0.5, line(TEXT));
        }
        if r.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// A 1 px dashed border inside `rect`.
pub fn dashed_rect(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(0.5);
    p.extend(Shape::dashed_line(&[r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()], line(color), 3.0, 3.0));
}

/// Width of what it drew; `left_center` is the left edge of the border.
pub fn badge(p: &Painter, left_center: Pos2, text: &str, opacity: f32) -> f32 {
    let fg = TEXT2.gamma_multiply(opacity);
    let g = p.layout_no_wrap(text.to_owned(), mono(10.5), fg);
    let rect = Rect::from_center_size(pos2(left_center.x + g.size().x / 2.0 + 6.0, left_center.y), vec2(g.size().x + 12.0, 18.0));
    p.rect_stroke(rect, 0.0, line(LINE2.gamma_multiply(opacity)), StrokeKind::Inside);
    p.galley(pos2(rect.min.x + 6.0, left_center.y - g.size().y / 2.0), g, fg);
    rect.width()
}

#[derive(Clone, Copy)]
pub enum Icon {
    Library,
    ChevronLeft,
    ChevronRight,
    PrevSeg,
    NextSeg,
    Play,
    Pause,
    Lock,
    Focus,
    Crop,
    Trim,
    DropCameras,
    Close,
}

/// The mockup's 24-unit SVG icons, `size` px on screen.
pub fn paint_icon(p: &Painter, center: Pos2, icon: Icon, size: f32, color: Color32) {
    let at = |x: f32, y: f32| center + vec2(x - 12.0, y - 12.0) * (size / 24.0);
    let points = |pts: &[(f32, f32)]| pts.iter().map(|&(x, y)| at(x, y)).collect::<Vec<_>>();
    let st = Stroke::new(1.25, color);
    let open = |pts: &[(f32, f32)]| {
        p.add(Shape::line(points(pts), st));
    };
    let closed = |pts: &[(f32, f32)]| {
        p.add(Shape::closed_line(points(pts), st));
    };
    let filled = |pts: &[(f32, f32)]| {
        p.add(Shape::convex_polygon(points(pts), color, Stroke::NONE));
    };
    match icon {
        Icon::Library => {
            closed(&[(3.0, 4.0), (21.0, 4.0), (21.0, 20.0), (3.0, 20.0)]);
            open(&[(9.0, 4.0), (9.0, 20.0)]);
        }
        Icon::ChevronLeft => open(&[(15.0, 18.0), (9.0, 12.0), (15.0, 6.0)]),
        Icon::ChevronRight => open(&[(9.0, 6.0), (15.0, 12.0), (9.0, 18.0)]),
        Icon::PrevSeg => {
            open(&[(6.0, 5.0), (6.0, 19.0)]);
            closed(&[(18.0, 6.0), (10.0, 12.0), (18.0, 18.0)]);
        }
        Icon::NextSeg => {
            open(&[(18.0, 5.0), (18.0, 19.0)]);
            closed(&[(6.0, 6.0), (14.0, 12.0), (6.0, 18.0)]);
        }
        Icon::Play => filled(&[(7.0, 4.0), (20.0, 12.0), (7.0, 20.0)]),
        Icon::Pause => {
            filled(&[(6.0, 4.0), (10.0, 4.0), (10.0, 20.0), (6.0, 20.0)]);
            filled(&[(14.0, 4.0), (18.0, 4.0), (18.0, 20.0), (14.0, 20.0)]);
        }
        Icon::Lock => {
            closed(&[(5.0, 11.0), (19.0, 11.0), (19.0, 21.0), (5.0, 21.0)]);
            open(&[(8.0, 11.0), (8.0, 7.0), (9.2, 4.2), (12.0, 3.0), (14.8, 4.2), (16.0, 7.0), (16.0, 11.0)]);
        }
        Icon::Focus => {
            open(&[(4.0, 9.0), (4.0, 4.0), (9.0, 4.0)]);
            open(&[(20.0, 9.0), (20.0, 4.0), (15.0, 4.0)]);
            open(&[(4.0, 15.0), (4.0, 20.0), (9.0, 20.0)]);
            open(&[(20.0, 15.0), (20.0, 20.0), (15.0, 20.0)]);
        }
        Icon::Crop => {
            open(&[(7.0, 3.0), (7.0, 17.0), (21.0, 17.0)]);
            open(&[(3.0, 7.0), (17.0, 7.0), (17.0, 21.0)]);
        }
        // In and Out brackets with what's kept between them.
        Icon::Trim => {
            open(&[(8.0, 5.0), (4.0, 5.0), (4.0, 19.0), (8.0, 19.0)]);
            open(&[(16.0, 5.0), (20.0, 5.0), (20.0, 19.0), (16.0, 19.0)]);
            filled(&[(8.0, 10.0), (16.0, 10.0), (16.0, 14.0), (8.0, 14.0)]);
        }
        // A camera, struck through.
        Icon::DropCameras => {
            closed(&[(3.0, 8.0), (14.0, 8.0), (14.0, 17.0), (3.0, 17.0)]);
            closed(&[(14.0, 11.0), (21.0, 8.0), (21.0, 17.0), (14.0, 14.0)]);
            open(&[(4.0, 21.0), (20.0, 4.0)]);
        }
        Icon::Close => {
            open(&[(6.0, 6.0), (18.0, 18.0)]);
            open(&[(18.0, 6.0), (6.0, 18.0)]);
        }
    }
}

/// The middle `h` px of `rect`.
pub fn centered_strip(rect: Rect, h: f32) -> Rect {
    Rect::from_center_size(rect.center(), vec2(rect.width(), h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_hands_out_from_both_ends() {
        let mut row = Row::new(Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 32.0)));
        let a = row.left(32.0, 4.0);
        let b = row.left(10.0, 0.0);
        let c = row.right(20.0, 6.0);
        assert_eq!((a.min.x, a.max.x, b.min.x, b.max.x), (0.0, 32.0, 36.0, 46.0));
        assert_eq!((c.min.x, c.max.x), (80.0, 100.0));
        assert_eq!((row.rest.min.x, row.rest.max.x), (46.0, 74.0));
        assert_eq!(c.height(), 32.0);
    }

    #[test]
    fn visuals_are_hard_edged_and_gray() {
        let v = visuals();
        let w = &v.widgets;
        for x in [&w.noninteractive, &w.inactive, &w.hovered, &w.active, &w.open] {
            assert_eq!(x.corner_radius, CornerRadius::ZERO);
            for c in [x.bg_fill, x.fg_stroke.color, x.bg_stroke.color] {
                let spread = c.r().max(c.g()).max(c.b()) - c.r().min(c.g()).min(c.b());
                assert!(spread <= 8, "{c:?} isn't gray");
            }
        }
        assert_eq!((v.window_corner_radius, v.menu_corner_radius), (CornerRadius::ZERO, CornerRadius::ZERO));
        assert_eq!(v.window_shadow, Shadow::NONE);
        let mut s = Style::default();
        apply_style(&mut s);
        assert_eq!(s.spacing.scroll.fade.strength, 0.0);
    }
}
