//! Camera area: every layout is a cell list on a 12 × 8 grid. A caption strip sits above each tile,
//! the video letterboxed below it, nothing over the video. The custom layout editor lives here too.

use super::settings::{Layout, Settings};
use super::theme::{self, BG, Face, HOVER, Icon, LINE, Look, TEXT, TEXT2, TEXT3, VIDEO_BG};
use crate::decode::FrameOut;
use crate::library::CAMERAS;
use eframe::egui::{
    Color32, Context, CursorIcon, Id, Rect, Role, Sense, Shape, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2, WidgetInfo, pos2, vec2,
};
use eframe::egui::epaint::{Mesh, Vertex};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Indexed like `CAMERAS`.
pub const LABELS: [&str; 6] = ["FRONT", "BACK", "LEFT REPEATER", "RIGHT REPEATER", "LEFT PILLAR", "RIGHT PILLAR"];
const CAPTION: f32 = 24.0;
const GAP: f32 = 2.0;
const HANDLE: f32 = 14.0;
const COLS: f32 = 12.0;
const ROWS: f32 = 8.0;
const FULL: Cell = [0.0, 0.0, COLS, ROWS];
const HINT: &str = "drag captions to move · drag ◢ to resize · snaps to 12 × 8";

/// `[x, y, w, h]` in grid units.
pub type Cell = [f32; 4];
/// Indexed like `CAMERAS`; `None` is hidden.
pub type Placed = [Option<Cell>; 6];
// shortcut: names sort alphabetically ("layout 10" before "layout 2") because the saved JSON map has no order; store a list if people keep many.
pub type Profiles = BTreeMap<String, Placed>;

const GRID: Placed = [
    Some([4.0, 0.0, 4.0, 4.0]),
    Some([4.0, 4.0, 4.0, 4.0]),
    Some([0.0, 4.0, 4.0, 4.0]),
    Some([8.0, 4.0, 4.0, 4.0]),
    Some([0.0, 0.0, 4.0, 4.0]),
    Some([8.0, 0.0, 4.0, 4.0]),
];
// The mockup's tracks are 3fr 4fr 3fr by 3fr 1fr 2fr and 4fr 1fr by thirds, which fall between grid
// lines. Only custom cells snap; presets keep the mockup's proportions.
const CENTER: Placed = [
    Some([3.6, 0.0, 4.8, 16.0 / 3.0]),
    Some([4.8, 16.0 / 3.0, 2.4, 8.0 / 3.0]),
    Some([0.0, 4.0, 3.6, 4.0]),
    Some([8.4, 4.0, 3.6, 4.0]),
    Some([0.0, 0.0, 3.6, 4.0]),
    Some([8.4, 0.0, 3.6, 4.0]),
];
const DRIVING: Placed = [
    Some([0.0, 0.0, 9.6, 8.0]),
    Some([9.6, 0.0, 2.4, 8.0 / 3.0]),
    Some([9.6, 8.0 / 3.0, 2.4, 8.0 / 3.0]),
    Some([9.6, 16.0 / 3.0, 2.4, 8.0 / 3.0]),
    None,
    None,
];
const MY_LAYOUT: Placed = [
    Some([0.0, 0.0, 8.0, 8.0]),
    Some([8.0, 0.0, 4.0, 4.0]),
    Some([8.0, 4.0, 2.0, 2.0]),
    Some([10.0, 4.0, 2.0, 2.0]),
    Some([8.0, 6.0, 2.0, 2.0]),
    Some([10.0, 6.0, 2.0, 2.0]),
];
const REAR_FOCUS: Placed = [MY_LAYOUT[1], MY_LAYOUT[0], MY_LAYOUT[2], MY_LAYOUT[3], MY_LAYOUT[4], MY_LAYOUT[5]];

pub fn default_profiles() -> Profiles {
    Profiles::from([("my layout".to_owned(), MY_LAYOUT), ("rear focus".to_owned(), REAR_FOCUS)])
}

/// The cell list of the current layout.
fn placed(s: &Settings) -> Placed {
    match s.layout {
        Layout::Grid => GRID,
        Layout::Center => CENTER,
        Layout::Driving => DRIVING,
        Layout::Custom => s.custom_layouts.get(&s.custom_active).copied().unwrap_or([None; 6]),
    }
}

/// What to draw, bottom tile first.
fn cells(placed: &Placed, z: &[usize; 6], focus: Option<usize>) -> Vec<(usize, Cell)> {
    if let Some(cam) = focus {
        return vec![(cam, FULL)];
    }
    z.iter().filter_map(|&cam| Some((cam, placed[cam]?))).collect()
}

/// The current layout's tiles, bottom first: what the composited export draws.
pub fn current_cells(s: &Settings, focus: Option<usize>) -> Vec<(usize, Cell)> {
    cells(&placed(s), &s.custom_z, focus)
}

/// Pixels per grid unit. A tile's right and bottom edges give up the gap, so outer edges stay flush.
fn unit(area: Rect) -> Vec2 {
    ((area.size() + Vec2::splat(GAP)) / vec2(COLS, ROWS)).max(Vec2::splat(1.0))
}

pub fn cell_rect(area: Rect, [x, y, w, h]: Cell) -> Rect {
    let u = unit(area);
    let min = area.min + vec2(x * u.x, y * u.y);
    let max = min + vec2(w * u.x - GAP, h * u.y - GAP);
    Rect::from_min_max(min.round(), max.round())
}

/// `dx`, `dy` in grid units, snapped; the whole cell stays inside the grid.
fn moved([x, y, w, h]: Cell, dx: f32, dy: f32) -> Cell {
    [(x + dx.round()).clamp(0.0, COLS - w), (y + dy.round()).clamp(0.0, ROWS - h), w, h]
}

/// `dx`, `dy` in grid units, snapped; at least 1 × 1 and inside the grid.
fn resized([x, y, w, h]: Cell, dx: f32, dy: f32) -> Cell {
    [x, y, (w + dx.round()).clamp(1.0, COLS - x), (h + dy.round()).clamp(1.0, ROWS - y)]
}

/// Puts `cam` last in the back-to-front list.
pub fn raise(z: &mut [usize; 6], cam: usize) {
    if let Some(i) = z.iter().position(|&c| c == cam) {
        z[i..].rotate_left(1);
    }
}

fn new_name(p: &Profiles) -> String {
    (2..).map(|n| format!("layout {n}")).find(|n| !p.contains_key(n)).unwrap_or_default()
}

pub fn profiles_to_json(p: &Profiles) -> Value {
    let cams = |placed: &Placed| -> Map<String, Value> {
        CAMERAS.iter().zip(placed).map(|(c, cell)| (c.to_string(), json!(cell.map(|c| c.map(|v| v as i32))))).collect()
    };
    Value::Object(p.iter().map(|(name, placed)| (name.clone(), Value::Object(cams(placed)))).collect())
}

/// Nothing loaded here can break the editor: a broken camera is hidden, a broken profile is dropped,
/// and if nothing is left it's the defaults.
pub fn profiles_from_json(v: &Value) -> Profiles {
    let mut out = Profiles::new();
    for (name, p) in v.as_object().into_iter().flatten() {
        if let Some(cams) = p.as_object() {
            out.insert(name.clone(), std::array::from_fn(|i| cams.get(CAMERAS[i]).and_then(cell_from_json)));
        }
    }
    if out.is_empty() { default_profiles() } else { out }
}

fn cell_from_json(v: &Value) -> Option<Cell> {
    let a = v.as_array().filter(|a| a.len() == 4)?;
    let n = |i: usize| a[i].as_f64().map(|x| x.round() as f32);
    let (x, y, w, h) = (n(0)?, n(1)?, n(2)?, n(3)?);
    let (w, h) = (w.clamp(1.0, COLS), h.clamp(1.0, ROWS));
    Some([x.clamp(0.0, COLS - w), y.clamp(0.0, ROWS - h), w, h])
}

/// Per-item (row, x): rows wrap when full, and items from `split` on sit at their row's right end.
pub fn flow(widths: &[f32], split: usize, avail: f32, gap: f32) -> Vec<(usize, f32)> {
    let mut out = Vec::with_capacity(widths.len());
    let (mut row, mut used) = (0, 0.0);
    for &w in widths {
        if used > 0.0 && used + w > avail {
            (row, used) = (row + 1, 0.0);
        }
        out.push((row, used));
        used += w + gap;
    }
    let mut shift = (usize::MAX, 0.0);
    for i in (split..widths.len()).rev() {
        if out[i].0 != shift.0 {
            shift = (out[i].0, (avail - out[i].1 - widths[i]).max(0.0));
        }
        out[i].1 += shift.1;
    }
    out
}

#[derive(Clone, Copy)]
struct Drag {
    cam: usize,
    resize: bool,
    orig: Cell,
}

/// Editor state that doesn't survive a restart.
#[derive(Default)]
pub struct Editor {
    pub editing: bool,
    /// Shows the `saved “name”` note until the next change.
    saved: bool,
    drag: Option<Drag>,
}

/// What one camera currently shows.
#[derive(Default)]
pub struct Tile {
    pub tex: Option<TextureHandle>,
    /// (segment, sample) on the texture.
    pub shown: Option<(usize, usize)>,
    /// Why there's no picture ("no clip", a read error).
    pub err: Option<String>,
}

impl Tile {
    /// Uploads only when the frame changed.
    pub fn show_frame(&mut self, ctx: &Context, cam: usize, f: FrameOut) {
        self.err = None;
        if self.shown == Some((f.seg, f.sample)) {
            return;
        }
        self.shown = Some((f.seg, f.sample));
        match &mut self.tex {
            Some(t) => t.set(f.image, TextureOptions::LINEAR),
            None => self.tex = Some(ctx.load_texture(format!("cam{cam}"), f.image, TextureOptions::LINEAR)),
        }
    }

    pub fn show_error(&mut self, e: String) {
        *self = Tile { err: Some(e), ..Tile::default() };
    }
}

/// The largest rect of aspect `size` centered in `outer`.
pub fn fit(outer: Rect, size: Vec2) -> Rect {
    if size.x <= 0.0 || size.y <= 0.0 {
        return outer;
    }
    let scale = (outer.width() / size.x).min(outer.height() / size.y);
    Rect::from_center_size(outer.center(), size * scale)
}

/// The back camera's cropped view: where its point (`u`, `v`) reads the source picture, both 0..1.
/// `aspect` is height / width. Flattens the fisheye to a 110° wide view, which leaves the trunk lid,
/// the plate light cover and the bumper outside.
// shortcut: the lens is taken as equidistant and the three numbers were fitted by eye to one HW4 frame; measure per model if another car looks bent or still shows its bumper.
pub fn back_crop(u: f32, v: f32, aspect: f32) -> (f32, f32) {
    const OUT_HALF: f32 = 55.0;
    const SRC_HALF: f32 = 85.0;
    const CENTER_V: f32 = 0.48;
    let (x, y) = (2.0 * u - 1.0, (2.0 * v - 1.0) * aspect);
    let r = x.hypot(y).max(1e-6);
    let k = (r * OUT_HALF.to_radians().tan()).atan() / SRC_HALF.to_radians() / r;
    (0.5 + x * k / 2.0, CENTER_V + y * k / (2.0 * aspect))
}

/// The custom toolbar when it applies, then the tiles. `panel` is the whole free rect.
pub fn show(ui: &mut Ui, panel: Rect, s: &mut Settings, ed: &mut Editor, tiles: &[Tile; 6], focus: &mut Option<usize>) {
    let custom = s.layout == Layout::Custom;
    let mut area = panel.shrink(8.0);
    if custom {
        area.min.y = toolbar(ui, panel, s, ed, focus) + 8.0;
    }
    let editing = custom && ed.editing && focus.is_none();
    if editing {
        canvas(ui, area);
    }
    for (cam, cell) in cells(&placed(s), &s.custom_z, *focus) {
        let rect = cell_rect(area, cell);
        let dragged = ed.drag.is_some_and(|d| d.cam == cam);
        if tile(ui, rect, cam, &tiles[cam], editing.then_some(dragged), &mut s.back_crop) {
            *focus = if *focus == Some(cam) { None } else { Some(cam) };
        }
        if editing {
            handles(ui, area, rect, cam, s, ed);
        }
    }
}

/// The snap grid behind the tiles while editing.
fn canvas(ui: &Ui, area: Rect) {
    let p = ui.painter_at(area);
    let u = unit(area);
    for i in 0..=COLS as i32 {
        p.vline((area.min.x + i as f32 * u.x).round() + 0.5, area.y_range(), theme::line(LINE));
    }
    for i in 0..=ROWS as i32 {
        p.hline(area.x_range(), (area.min.y + i as f32 * u.y).round() + 0.5, theme::line(LINE));
    }
    p.rect_stroke(area, 0.0, theme::line(LINE), StrokeKind::Inside);
}

/// `edit` is `Some(being dragged)` in edit mode. `crop` applies to the back camera only. True when focus was asked for.
fn tile(ui: &mut Ui, rect: Rect, cam: usize, t: &Tile, edit: Option<bool>, crop: &mut bool) -> bool {
    let back = cam == 1;
    let (caption, video) = rect.split_top_bottom_at_y(rect.min.y + CAPTION);
    let p = ui.painter_at(rect);
    // Opaque: custom tiles can overlap.
    p.rect_filled(caption, 0.0, if edit.is_some() { HOVER } else { BG });
    p.hline(caption.x_range(), caption.min.y + 0.5, theme::line(LINE));
    p.vline(caption.min.x + 0.5, caption.y_range(), theme::line(LINE));
    p.vline(caption.max.x - 0.5, caption.y_range(), theme::line(LINE));
    let name = LABELS[cam];
    let mut text_x = caption.min.x + 8.0;
    if edit.is_some() {
        // The mockup's ⠿ as dots: the embedded fonts don't have that glyph.
        for (i, j) in [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2)] {
            let dot = pos2(text_x + i as f32 * 4.0, caption.center().y - 5.0 + j as f32 * 4.0);
            p.rect_filled(Rect::from_min_size(dot, Vec2::splat(2.0)), 0.0, TEXT3);
        }
        text_x += 12.0;
    }
    theme::spaced(&p, pos2(text_x, caption.center().y), name, theme::medium(10.0), TEXT2, 1.4, caption.max.x - text_x - if back { 56.0 } else { 32.0 });

    p.rect_filled(video, 0.0, VIDEO_BG);
    match (&t.tex, &t.err) {
        (Some(tex), _) => {
            let (r, size) = (fit(video, tex.size_vec2()), tex.size_vec2());
            if back && *crop && size.x > 0.0 {
                // A grid of quads bends the picture without a shader.
                const N: u32 = 24;
                let mut mesh = Mesh::with_texture(tex.id());
                for j in 0..=N {
                    for i in 0..=N {
                        let (u, v) = (i as f32 / N as f32, j as f32 / N as f32);
                        let (su, sv) = back_crop(u, v, size.y / size.x);
                        mesh.vertices.push(Vertex { pos: r.min + r.size() * vec2(u, v), uv: pos2(su, sv), color: Color32::WHITE });
                    }
                }
                for j in 0..N {
                    for i in 0..N {
                        let a = j * (N + 1) + i;
                        mesh.add_triangle(a, a + 1, a + N + 1);
                        mesh.add_triangle(a + 1, a + N + 2, a + N + 1);
                    }
                }
                p.add(Shape::mesh(mesh));
            } else {
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                p.image(tex.id(), r, uv, Color32::WHITE);
            }
        }
        (None, Some(e)) => {
            theme::label(&p, pos2(video.min.x + 8.0, video.center().y), e, theme::mono(10.5), TEXT3, video.width() - 16.0);
        }
        (None, None) => {}
    }

    if let Some(dragged) = edit {
        if dragged {
            p.rect_stroke(rect, 0.0, theme::line(TEXT), StrokeKind::Inside);
        } else {
            theme::dashed_rect(&p, rect, TEXT3);
        }
        return false;
    }
    // The whole tile, caption included; the focus button registers later and keeps its own clicks.
    let double = ui.interact(rect, Id::unique(("tile", cam)), Sense::click()).double_clicked();
    if back {
        let b = Rect::from_min_max(pos2(caption.max.x - 48.0, caption.min.y), pos2(caption.max.x - 24.0, caption.max.y));
        let hint = if *crop { "Show the whole back camera picture" } else { "Crop the back camera to the road and straighten it" };
        if caption_button(ui, b, cam, Icon::Crop, 11.0, hint) {
            *crop = !*crop;
        }
        if *crop {
            ui.painter().hline(b.shrink(6.0).x_range(), b.max.y - 3.5, theme::line(TEXT));
        }
    }
    caption_button(ui, caption.with_min_x(caption.max.x - 24.0), cam, Icon::Focus, 11.0, &format!("Focus {name} (double-click)")) || double
}

/// Edit mode: the caption moves, the corner resizes, × hides.
fn handles(ui: &mut Ui, area: Rect, rect: Rect, cam: usize, s: &mut Settings, ed: &mut Editor) {
    let Some(placed) = s.custom_layouts.get_mut(&s.custom_active) else { return };
    let Some(cell) = placed[cam] else { return };
    let name = LABELS[cam];
    let caption = rect.with_max_y(rect.min.y + CAPTION);
    let corner = Rect::from_min_max(rect.max - Vec2::splat(HANDLE), rect.max);
    let u = unit(area);
    for (resize, handle, cursor, verb) in [(false, caption, CursorIcon::Move, "Move"), (true, corner, CursorIcon::ResizeNwSe, "Resize")] {
        let r = ui.interact(handle, Id::unique(("tile-drag", cam, resize)), Sense::drag()).on_hover_cursor(cursor);
        r.widget_info(|| WidgetInfo::labeled(Role::Button, true, format!("{verb} {name}")));
        if r.drag_started() {
            ed.drag = Some(Drag { cam, resize, orig: cell });
            ed.saved = false;
            raise(&mut s.custom_z, cam);
        }
        if let (Some(d), Some(delta)) = (ed.drag.filter(|d| d.cam == cam && d.resize == resize), r.total_drag_delta()) {
            let (dx, dy) = (delta.x / u.x, delta.y / u.y);
            placed[cam] = Some(if resize { resized(d.orig, dx, dy) } else { moved(d.orig, dx, dy) });
        }
        if r.drag_stopped() {
            ed.drag = None;
        }
    }
    ui.painter().rect_filled(corner, 0.0, TEXT);
    if caption_button(ui, caption.with_min_x(caption.max.x - 24.0), cam, Icon::Close, 10.0, &format!("Hide {name}")) {
        placed[cam] = None;
        ed.saved = false;
    }
}

/// A stable id: raising a tile mustn't turn a press into a different button.
fn caption_button(ui: &mut Ui, rect: Rect, cam: usize, icon: Icon, size: f32, name: &str) -> bool {
    let r = ui.interact(rect, Id::unique(("tile-button", cam, icon as u8)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    r.widget_info(|| WidgetInfo::labeled(Role::Button, true, name));
    let p = ui.painter();
    p.vline(rect.min.x + 0.5, rect.y_range(), theme::line(LINE));
    theme::paint_icon(p, rect.center(), icon, size, if r.hovered() { TEXT } else { TEXT3 });
    r.clicked()
}

enum Item {
    Title,
    Note(Color32),
    Profile,
    New,
    Add(usize),
    Reset,
    Save,
    Edit,
}

/// Returns the y where the camera area may start.
fn toolbar(ui: &mut Ui, panel: Rect, s: &mut Settings, ed: &mut Editor, focus: &mut Option<usize>) -> f32 {
    let mut items = vec![(Item::Title, "CUSTOM".to_owned())];
    items.extend(s.custom_layouts.keys().map(|n| (Item::Profile, n.clone())));
    items.push((Item::New, "+ new".to_owned()));
    let split = items.len();
    if ed.editing {
        let current = s.custom_layouts.get(&s.custom_active).copied().unwrap_or([None; 6]);
        let hidden: Vec<usize> = (0..6).filter(|&c| current[c].is_none()).collect();
        if !hidden.is_empty() {
            items.push((Item::Note(TEXT3), "hidden".to_owned()));
        }
        items.extend(hidden.into_iter().map(|c| (Item::Add(c), format!("+ {}", CAMERAS[c]))));
        items.push((Item::Note(TEXT3), HINT.to_owned()));
        items.push((Item::Reset, "reset".to_owned()));
        items.push((Item::Save, "save".to_owned()));
    } else if ed.saved {
        items.push((Item::Note(TEXT2), format!("saved “{}”", s.custom_active)));
    }
    items.push((Item::Edit, if ed.editing { "done" } else { "edit layout" }.to_owned()));

    // Design rule 3: same-kind controls match, and "edit layout" turning into "done" moves nothing.
    let action_w = theme::text_width(ui, "edit layout", theme::mono(11.5)) + 24.0;
    let profile_w = s.custom_layouts.keys().map(|n| theme::text_width(ui, n, theme::mono(11.5))).fold(0.0, f32::max);
    let widths: Vec<f32> = items
        .iter()
        .map(|(item, text)| {
            let w = |font| theme::text_width(ui, text, font);
            let width = match item {
                Item::Title => w(theme::medium(11.0)) + text.len() as f32 * 1.54,
                Item::Note(_) => w(theme::mono(11.0)),
                Item::Profile => profile_w,
                Item::New => w(theme::mono(11.5)),
                Item::Add(_) => w(theme::mono(11.5)) + 20.0,
                Item::Reset | Item::Save | Item::Edit => action_w,
            };
            width.ceil()
        })
        .collect();

    let inner = panel.shrink2(vec2(16.0, 8.0));
    let spots = flow(&widths, split, inner.width(), 12.0);
    let row_y = |row: usize| inner.min.y + row as f32 * 40.0;
    for ((item, text), (&(row, x), &w)) in items.iter().zip(spots.iter().zip(&widths)) {
        let rect = Rect::from_min_size(pos2(inner.min.x + x, row_y(row)), vec2(w, 32.0));
        let mut changed = false;
        match item {
            Item::Title => {
                theme::spaced(ui.painter(), rect.left_center(), text, theme::medium(11.0), TEXT3, 1.54, w + 1.0);
            }
            Item::Note(color) => {
                theme::label(ui.painter(), rect.left_center(), text, theme::mono(11.0), *color, w + 1.0);
            }
            Item::Profile => {
                if theme::toggles(ui, rect, &[text], (*text == s.custom_active).then_some(0), 0.0).is_some() {
                    s.custom_active = text.clone();
                    changed = true;
                }
            }
            Item::New => {
                if theme::toggles(ui, rect, &[text], None, 0.0).is_some() {
                    let name = new_name(&s.custom_layouts);
                    let copy = s.custom_layouts.get(&s.custom_active).copied().unwrap_or(MY_LAYOUT);
                    s.custom_layouts.insert(name.clone(), copy);
                    s.custom_active = name;
                    ed.editing = true;
                    *focus = None;
                    changed = true;
                }
            }
            Item::Add(cam) => {
                if theme::button(ui, rect, Face::Text(text), Look::Dashed, true, text).clicked() {
                    if let Some(p) = s.custom_layouts.get_mut(&s.custom_active) {
                        p[*cam] = Some([0.0, 0.0, 4.0, 4.0]);
                    }
                    raise(&mut s.custom_z, *cam);
                    changed = true;
                }
            }
            Item::Reset => {
                if theme::text_button(ui, rect, text, true).clicked() {
                    let original = if s.custom_active == "rear focus" { REAR_FOCUS } else { MY_LAYOUT };
                    s.custom_layouts.insert(s.custom_active.clone(), original);
                    changed = true;
                }
            }
            Item::Save => {
                if theme::button(ui, rect, Face::Text(text), Look::Primary, true, text).clicked() {
                    ed.editing = false;
                    ed.saved = true;
                }
            }
            Item::Edit => {
                if theme::text_button(ui, rect, text, true).clicked() {
                    ed.editing = !ed.editing;
                    *focus = None;
                    changed = true;
                }
            }
        }
        if changed {
            ed.saved = false;
        }
    }
    let rows = spots.last().map_or(1, |s| s.0 + 1);
    let bottom = row_y(rows - 1) + 32.0 + 8.0;
    ui.painter().hline(panel.x_range(), bottom - 0.5, theme::line(LINE));
    bottom
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Event, Modifiers, PointerButton, Pos2, RawInput};

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(10.0, 20.0), vec2(w, h))
    }

    #[test]
    fn letterbox_keeps_aspect_and_centers() {
        // Wide cell, 4:3 video: bars left and right.
        let f = fit(rect(800.0, 300.0), vec2(1280.0, 960.0));
        assert_eq!((f.width(), f.height()), (400.0, 300.0));
        assert_eq!(f.center(), rect(800.0, 300.0).center());
        // Tall cell: bars above and below.
        let f = fit(rect(400.0, 900.0), vec2(1280.0, 960.0));
        assert_eq!((f.width(), f.height()), (400.0, 300.0));
        assert_eq!(fit(rect(40.0, 30.0), vec2(0.0, 0.0)), rect(40.0, 30.0));
    }

    #[test]
    fn back_crop_reads_inside_the_picture_and_keeps_the_middle() {
        let a = 938.0 / 1448.0;
        let (cu, cv) = back_crop(0.5, 0.5, a);
        assert!((cu - 0.5).abs() < 1e-4 && (cv - 0.48).abs() < 1e-4);
        for (u, v) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.0, 0.5), (0.5, 1.0)] {
            let (su, sv) = back_crop(u, v, a);
            assert!((0.05..=0.95).contains(&su) && (0.05..=0.95).contains(&sv), "{u},{v} -> {su},{sv}");
        }
        // Mirror images left and right, and the edge reads further out than halfway to it.
        let ((l, _), (r, _), (h, _)) = (back_crop(0.0, 0.3, a), back_crop(1.0, 0.3, a), back_crop(0.75, 0.3, a));
        assert!((l + r - 1.0).abs() < 1e-4 && r > h && h > 0.5);
    }

    #[test]
    fn the_crop_button_is_on_the_back_tile_only() {
        let mut r = Rig::new(false);
        let at = |r: &Rig, cam: usize| r.rect_of(cam).right_top() + vec2(-36.0, 12.0);
        let p = at(&r, 1);
        r.click(p);
        assert!(r.s.back_crop && r.focus.is_none());
        r.click(p);
        assert!(!r.s.back_crop);
        let p = at(&r, 0);
        r.click(p);
        assert!(!r.s.back_crop);
    }

    #[test]
    fn presets_are_in_bounds_and_show_the_right_cameras() {
        let shown = |p: &Placed| (0..6).filter(|&c| p[c].is_some()).collect::<Vec<_>>();
        for (p, cams) in [(GRID, vec![0, 1, 2, 3, 4, 5]), (CENTER, vec![0, 1, 2, 3, 4, 5]), (DRIVING, vec![0, 1, 2, 3])] {
            assert_eq!(shown(&p), cams);
            for [x, y, w, h] in p.into_iter().flatten() {
                assert!(x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0);
                assert!(x + w <= COLS + 1e-4 && y + h <= ROWS + 1e-4, "{x} {y} {w} {h}");
            }
        }
        assert_eq!(shown(&MY_LAYOUT).len(), 6);
        assert_eq!((REAR_FOCUS[0], REAR_FOCUS[1]), (MY_LAYOUT[1], MY_LAYOUT[0]));
    }

    #[test]
    fn center_back_is_half_the_front_and_centered_under_it() {
        let ([fx, fy, fw, fh], [bx, by, bw, bh]) = (CENTER[0].unwrap(), CENTER[1].unwrap());
        assert!((bw - fw / 2.0).abs() < 1e-4);
        assert!((bx + bw / 2.0 - (fx + fw / 2.0)).abs() < 1e-4);
        assert!((by - (fy + fh)).abs() < 1e-4 && (by + bh - ROWS).abs() < 1e-4);
    }

    #[test]
    fn grid_units_map_to_pixels_with_two_px_gaps_and_flush_edges() {
        let a = rect(904.0, 602.0);
        let c = cells(&GRID, &[4, 0, 5, 2, 1, 3], None);
        assert_eq!(c.iter().map(|x| x.0).collect::<Vec<_>>(), [4, 0, 5, 2, 1, 3]);
        let r: Vec<Rect> = c.iter().map(|x| cell_rect(a, x.1)).collect();
        assert_eq!((r[0].min, r[5].max), (a.min, a.max));
        assert_eq!((r[1].width(), r[1].height()), (300.0, 300.0));
        assert_eq!(r[1].min.x - r[0].max.x, GAP);
        assert_eq!(r[3].min.y - r[0].max.y, GAP);
        // Closing the library only widens the area.
        assert_eq!(cell_rect(rect(1204.0, 602.0), GRID[0].unwrap()).width(), 400.0);
        // Focus fills the area, even for a camera the layout hides.
        assert_eq!(cells(&DRIVING, &[0, 1, 2, 3, 4, 5], Some(5)), vec![(5, FULL)]);
        assert_eq!(cell_rect(a, FULL), a);
        assert_eq!(cells(&DRIVING, &[5, 4, 3, 2, 1, 0], None).len(), 4);
    }

    #[test]
    fn move_snaps_and_stays_inside() {
        let c = [2.0, 2.0, 4.0, 4.0];
        assert_eq!(moved(c, 0.4, -0.4), c);
        assert_eq!(moved(c, 0.6, 1.4), [3.0, 3.0, 4.0, 4.0]);
        assert_eq!(moved(c, -9.0, -9.0), [0.0, 0.0, 4.0, 4.0]);
        assert_eq!(moved(c, 99.0, 99.0), [8.0, 4.0, 4.0, 4.0]);
        assert_eq!(moved(FULL, 3.0, 3.0), FULL);
    }

    #[test]
    fn resize_snaps_stays_inside_and_keeps_one_cell() {
        let c = [2.0, 2.0, 4.0, 4.0];
        assert_eq!(resized(c, 0.6, -0.6), [2.0, 2.0, 5.0, 3.0]);
        assert_eq!(resized(c, -99.0, -99.0), [2.0, 2.0, 1.0, 1.0]);
        assert_eq!(resized(c, 99.0, 99.0), [2.0, 2.0, 10.0, 6.0]);
        assert_eq!(resized([11.0, 7.0, 1.0, 1.0], 5.0, 5.0), [11.0, 7.0, 1.0, 1.0]);
    }

    #[test]
    fn raising_puts_a_camera_on_top_and_keeps_the_rest() {
        let mut z = [0, 1, 2, 3, 4, 5];
        raise(&mut z, 2);
        assert_eq!(z, [0, 1, 3, 4, 5, 2]);
        raise(&mut z, 2);
        assert_eq!(z, [0, 1, 3, 4, 5, 2]);
        raise(&mut z, 0);
        assert_eq!(z, [1, 3, 4, 5, 2, 0]);
    }

    #[test]
    fn new_names_are_unique() {
        let mut p = default_profiles();
        assert_eq!(new_name(&p), "layout 2");
        p.insert("layout 2".to_owned(), MY_LAYOUT);
        p.insert("layout 4".to_owned(), MY_LAYOUT);
        assert_eq!(new_name(&p), "layout 3");
    }

    #[test]
    fn profiles_round_trip_in_the_documented_shape() {
        let mut p = default_profiles();
        p.get_mut("my layout").unwrap()[1] = None;
        let v = profiles_to_json(&p);
        assert_eq!(v["my layout"]["front"], json!([0, 0, 8, 8]));
        assert_eq!(v["my layout"]["back"], Value::Null);
        assert_eq!(v["rear focus"]["left_pillar"], json!([8, 6, 2, 2]));
        assert_eq!(profiles_from_json(&v), p);
    }

    #[test]
    fn corrupted_profiles_fall_back_piece_by_piece() {
        for bad in [json!("nope"), json!({}), json!({"a": 3})] {
            assert_eq!(profiles_from_json(&bad), default_profiles());
        }
        let p = profiles_from_json(&json!({
            "bad": 3,
            "a": {
                "front": [50, -3, 99, 0],
                "back": [1.4, 2.6, 3, 3],
                "left_repeater": [1, 2, 3],
                "right_repeater": ["x", 0, 1, 1],
                "left_pillar": "x",
                "bumper": [0, 0, 1, 1],
            },
        }));
        assert_eq!(p.keys().collect::<Vec<_>>(), ["a"]);
        let a = p["a"];
        assert_eq!(a[0], Some([0.0, 0.0, 12.0, 1.0]));
        assert_eq!(a[1], Some([1.0, 3.0, 3.0, 3.0]));
        assert_eq!(a[2..], [None; 4]);
        // Whatever loaded is safe to drag.
        assert_eq!(moved(a[0].unwrap(), 5.0, 50.0), [0.0, 7.0, 12.0, 1.0]);
    }

    #[test]
    fn toolbar_wraps_and_keeps_the_right_group_right() {
        // Everything fits: the right group ends at the edge.
        assert_eq!(flow(&[10.0, 10.0, 20.0], 2, 100.0, 5.0), vec![(0, 0.0), (0, 15.0), (0, 80.0)]);
        // The last two wrap and stay right-aligned on their own row.
        assert_eq!(flow(&[50.0, 40.0, 30.0, 20.0], 2, 100.0, 5.0), vec![(0, 0.0), (0, 55.0), (1, 45.0), (1, 80.0)]);
        // An item wider than the row starts at 0.
        assert_eq!(flow(&[200.0], 0, 100.0, 5.0), vec![(0, 0.0)]);
        assert_eq!(flow(&[200.0, 10.0], 1, 100.0, 5.0), vec![(0, 0.0), (1, 90.0)]);
    }

    /// The editor driven through real egui frames, without a window. 1400 wide keeps the toolbar on one row.
    struct Rig {
        ctx: Context,
        s: Settings,
        ed: Editor,
        tiles: [Tile; 6],
        focus: Option<usize>,
    }

    impl Rig {
        const AREA: Rect = Rect { min: pos2(8.0, 56.0), max: pos2(1392.0, 792.0) };

        fn new(editing: bool) -> Rig {
            let ctx = Context::default();
            theme::install(&ctx);
            let s = Settings { layout: Layout::Custom, ..Settings::default() };
            let mut rig = Rig { ctx, s, ed: Editor { editing, ..Editor::default() }, tiles: Default::default(), focus: None };
            rig.frame(vec![]);
            rig
        }

        fn frame(&mut self, events: Vec<Event>) {
            let input = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1400.0, 800.0))), events, ..Default::default() };
            let (s, ed, tiles, focus) = (&mut self.s, &mut self.ed, &self.tiles, &mut self.focus);
            self.ctx.run_ui(input, |ui| show(ui, ui.max_rect(), s, ed, tiles, focus)).textures_delta.clear();
        }

        fn button(&mut self, pos: Pos2, pressed: bool) {
            self.frame(vec![Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }]);
        }

        fn drag(&mut self, from: Pos2, to: Pos2) {
            self.frame(vec![Event::PointerMoved(from)]);
            self.button(from, true);
            self.frame(vec![Event::PointerMoved(to)]);
            self.button(to, false);
            self.frame(vec![]);
        }

        fn click(&mut self, pos: Pos2) {
            self.frame(vec![Event::PointerMoved(pos)]);
            self.button(pos, true);
            self.button(pos, false);
            self.frame(vec![]);
        }

        fn cell(&self, cam: usize) -> Option<Cell> {
            self.s.custom_layouts[&self.s.custom_active][cam]
        }

        fn rect_of(&self, cam: usize) -> Rect {
            cell_rect(Rig::AREA, self.cell(cam).unwrap())
        }
    }

    #[test]
    fn dragging_a_caption_moves_snapped_and_raises() {
        let mut r = Rig::new(true);
        let u = unit(Rig::AREA);
        let from = r.rect_of(1).min + vec2(60.0, 12.0);
        r.drag(from, from + vec2(-3.3 * u.x, 1.2 * u.y));
        assert_eq!(r.cell(1), Some([5.0, 1.0, 4.0, 4.0]));
        assert_eq!(r.s.custom_z[5], 1);
        assert!(r.ed.drag.is_none());
    }

    #[test]
    fn dragging_the_corner_resizes_snapped() {
        let mut r = Rig::new(true);
        let u = unit(Rig::AREA);
        let from = r.rect_of(2).max - vec2(5.0, 5.0);
        r.drag(from, from + vec2(1.2 * u.x, -5.0 * u.y));
        assert_eq!(r.cell(2), Some([8.0, 4.0, 3.0, 1.0]));
        assert_eq!(r.s.custom_z[5], 2);
    }

    #[test]
    fn the_x_hides_and_view_mode_cannot_edit() {
        let mut r = Rig::new(true);
        let x = r.rect_of(1).right_top() + vec2(-12.0, 12.0);
        r.click(x);
        assert_eq!(r.cell(1), None);

        let mut r = Rig::new(false);
        let from = r.rect_of(1).min + vec2(60.0, 12.0);
        r.drag(from, from + vec2(-300.0, 100.0));
        assert_eq!(r.cell(1), Some([8.0, 0.0, 4.0, 4.0]));
    }

    #[test]
    fn focus_by_button_and_double_click_but_not_while_editing() {
        let mut r = Rig::new(false);
        let button = r.rect_of(1).right_top() + vec2(-12.0, 12.0);
        r.click(button);
        assert_eq!(r.focus, Some(1));
        r.click(Rig::AREA.right_top() + vec2(-12.0, 12.0));
        assert_eq!(r.focus, None);

        // A fresh rig: egui would count these two right after the others as a triple click.
        let mut r = Rig::new(false);
        let video = r.rect_of(1).center();
        r.click(video);
        r.click(video);
        assert_eq!(r.focus, Some(1));

        // The caption is part of the tile.
        let mut r = Rig::new(false);
        let name = r.rect_of(1).min + vec2(40.0, 12.0);
        r.click(name);
        r.click(name);
        assert_eq!(r.focus, Some(1));

        let mut r = Rig::new(true);
        let video = r.rect_of(1).center();
        r.click(video);
        r.click(video);
        assert_eq!(r.focus, None);
    }

    #[test]
    fn toolbar_keeps_its_buttons_in_place_when_edit_mode_changes() {
        // "edit layout" is 11 characters at 6.9 px, plus padding; "done" takes the same room.
        let edit = pos2(1400.0 - 16.0 - 50.0, 24.0);
        let mut r = Rig::new(false);
        r.click(edit);
        assert!(r.ed.editing);
        r.click(edit);
        assert!(!r.ed.editing);
    }

    #[test]
    fn toolbar_adds_back_resets_saves_and_copies() {
        // Button centers at 1400 wide, from the mono font's 6.9 px advance; the row is laid out from both ends.
        let at = |x: f32| pos2(x, 24.0);
        let (new, add_back, reset, save) = (at(256.0), at(622.0), at(1110.0), at(1222.0));

        let mut r = Rig::new(true);
        let hide = r.rect_of(1).right_top() + vec2(-12.0, 12.0);
        r.click(hide);
        assert_eq!(r.cell(1), None);
        r.click(add_back);
        assert_eq!(r.cell(1), Some([0.0, 0.0, 4.0, 4.0]));
        assert_eq!(r.s.custom_z[5], 1);
        r.click(reset);
        assert_eq!(r.cell(1), MY_LAYOUT[1]);

        r.click(save);
        assert!(!r.ed.editing && r.ed.saved);

        r.s.custom_layouts.get_mut("my layout").unwrap()[2] = None;
        r.click(new);
        assert_eq!(r.s.custom_active, "layout 2");
        assert_eq!(r.cell(2), None);
        assert!(r.ed.editing && !r.ed.saved);
    }

    #[test]
    fn the_toolbar_text_can_be_drawn() {
        let r = Rig::new(false);
        r.ctx.fonts_mut(|f| {
            assert!(f.has_glyphs(&theme::mono(11.0), HINT), "hint");
            assert!(f.has_glyphs(&theme::mono(11.0), "saved “x”"), "note");
        });
    }
}
