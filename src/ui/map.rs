//! The bottom-left map cell: walkers tiles, the GPS track and the car.
//!
//! walkers can't rotate, so heading-up is done after the fact: the map is laid out on a square that
//! covers the cell at any angle, clipped to the cell, and its tile meshes are rotated in the paint
//! list before the track goes on top. Its gestures would then act on the unrotated map, so drag and
//! zoom are handled here instead.

use super::settings::Settings;
use super::theme::{self, BG, LINE, OFF, Row, TEXT, TEXT2, TEXT3};
use crate::sei::Telemetry;
use crate::telemetry::{HOLD, Record, Store};
use eframe::egui::emath::Rot2;
use eframe::egui::layers::ShapeIdx;
use eframe::egui::{Align2, Color32, Context, CursorIcon, DragPanButtons, FontId, ImageData, LayerId, Mesh, OpenUrl, Painter, Pos2, Rect, Role, Sense, Shape, Stroke, TextureHandle, TextureId, Ui, UiBuilder, Vec2, WidgetInfo, pos2, vec2};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use walkers::sources::{Attribution, OpenStreetMap, TileSource};
use walkers::{HeaderValue, HttpOptions, HttpTiles, Map, MapMemory, MaxParallelDownloads, Position, Stats, Tile, TileId, TilePiece, Tiles, lat_lon};

pub const WIDTH: f32 = 288.0;
const HEADER: f32 = 36.0;
const FOOTER: f32 = 27.0;
const BODY_MIN: f32 = 110.0;
/// Header + the mockup's minimum body + coordinates line.
pub const MIN_HEIGHT: f32 = HEADER + BODY_MIN + FOOTER;
const BODY_BG: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x18);

/// OSM's tile policy asks for a User-Agent that identifies the app.
const USER_AGENT: &str = concat!("OpenTeslaCam/", env!("CARGO_PKG_VERSION"));
const STREET_ALPHA: f32 = 0.4;
const SAT_ALPHA: f32 = 0.7;
const ZOOM_MIN: f64 = 2.0;
const ZOOM_MAX: f64 = 19.0;
/// A click this close to the track (px) seeks.
const PICK_RADIUS: f32 = 10.0;
/// Seconds between track points at least: SEI comes at ~36/s, far denser than a car moves on this
/// map, and a long RecentClips event is projected on every frame.
const THIN: f64 = 0.25;

const LAYERS: [&str; 2] = ["map", "sat"];
const ORIENTATIONS: [&str; 2] = ["north", "heading"];

struct Esri;

impl TileSource for Esri {
    fn tile_url(&self, t: TileId) -> String {
        format!("https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{}/{}/{}", t.zoom, t.y, t.x)
    }

    fn attribution(&self) -> Attribution {
        Attribution { text: "Esri, Maxar, Earthstar Geographics", url: "https://www.esri.com/", logo_light: None, logo_dark: None }
    }
}

fn source_credit(sat: bool) -> Attribution {
    if sat { Esri.attribution() } else { OpenStreetMap.attribution() }
}

fn attribution(sat: bool) -> String {
    format!("© {}", source_credit(sat).text)
}

/// Luma, in the gamma space the pixels are stored in. Premultiplied alpha stays valid: it's never
/// above the largest channel.
fn grey(c: Color32) -> Color32 {
    let [r, g, b, a] = c.to_array().map(u32::from);
    let l = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
    Color32::from_rgba_premultiplied(l, l, l, a as u8)
}

/// Tiles in greyscale (design rule 2). walkers only hands out finished textures, so it's given a
/// `Context` of its own to upload them to. Those uploads are the only place the decoded pixels can
/// be read; each one goes on to the window's `Context` in grey.
struct Grey<T> {
    tiles: T,
    decoded: Context,
    window: Context,
    /// By the id in `decoded`.
    shown: HashMap<TextureId, TextureHandle>,
}

impl<T: Tiles> Grey<T> {
    fn new(window: Context, tiles: impl FnOnce(Context) -> T) -> Grey<T> {
        let decoded = Context::default();
        let wake = window.clone();
        // An observer, as walkers wakes "its" Context per tile and this one never runs a pass, which
        // is what would re-arm a repaint callback.
        decoded.set_repaint_observer(move |_| wake.request_repaint());
        Grey { tiles: tiles(decoded.clone()), decoded, window, shown: HashMap::new() }
    }

    /// Passes on what walkers uploaded or dropped since the last call.
    fn sync(&mut self) {
        let mut delta = self.decoded.tex_manager().write().take_delta();
        // The default id is the font atlas every Context starts with.
        for (id, images) in delta.set.drain().filter(|d| d.0 != TextureId::default()) {
            // walkers only ever uploads whole images, one per texture.
            for d in images.into_iter().filter(|d| d.pos.is_none()) {
                let ImageData::Color(image) = d.image;
                let mut image = Arc::unwrap_or_clone(image);
                image.pixels.iter_mut().for_each(|p| *p = grey(*p));
                self.shown.insert(id, self.window.load_texture("tile", image, d.options));
            }
        }
        for id in delta.free.drain() {
            self.shown.remove(&id);
        }
    }
}

impl<T: Tiles> Tiles for Grey<T> {
    fn at(&mut self, id: TileId) -> Option<TilePiece> {
        let piece = self.tiles.at(id);
        self.sync();
        let piece = piece?;
        let Tile::Raster(decoded) = &piece.tile;
        Some(TilePiece::new(Tile::Raster(self.shown.get(&decoded.id())?.clone()), piece.uv))
    }

    fn attribution(&self) -> Attribution {
        self.tiles.attribution()
    }

    fn tile_size(&self) -> u32 {
        self.tiles.tile_size()
    }

    fn stats(&self) -> Stats {
        self.tiles.stats()
    }
}

/// The per-user cache folder; `var` reads the environment.
fn cache_dir(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(var("LOCALAPPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(var("HOME")?).join("Library/Caches")
    } else {
        var("XDG_CACHE_HOME").map(PathBuf::from).or_else(|| Some(PathBuf::from(var("HOME")?).join(".cache")))?
    };
    Some(base.join("openteslacam").join("tiles"))
}

fn new_tiles(sat: bool, window: Context) -> Grey<HttpTiles> {
    let options = HttpOptions {
        // shortcut: the disk cache has no size cap (walkers offers none), prune it at startup if it ever gets big.
        cache: cache_dir(|k| std::env::var(k).ok().filter(|s| !s.is_empty())),
        user_agent: Some(HeaderValue::from_static(USER_AGENT)),
        // The cell shows a handful of tiles; no need for walkers' browser-like default of 6.
        max_parallel_downloads: MaxParallelDownloads::value_manually_confirmed_with_provider_limits(2),
        ..Default::default()
    };
    Grey::new(window, |ctx| if sat { HttpTiles::with_options(Esri, options, ctx) } else { HttpTiles::with_options(OpenStreetMap, options, ctx) })
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    t: f64,
    lat: f64,
    lon: f64,
}

/// An unbroken stretch of track; nothing is drawn or picked between runs.
type Run = Vec<Point>;

/// No fix reads as 0, 0. The range check also turns away NaN.
fn valid_fix(lat: f64, lon: f64) -> bool {
    (lat, lon) != (0.0, 0.0) && lat.abs() <= 90.0 && lon.abs() <= 180.0
}

/// The track of `records` (sorted by time), broken at bad fixes and at holes in the data.
fn extract(records: &[Record]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut prev = None;
    for r in records {
        let (lat, lon) = (r.tel.lat, r.tel.lon);
        if !valid_fix(lat, lon) {
            prev = None;
            continue;
        }
        if prev.is_none_or(|t| r.t - t > HOLD) {
            runs.push(Vec::new());
        }
        prev = Some(r.t);
        let run = runs.last_mut().expect("pushed above, or prev would be None");
        if run.last().is_none_or(|p| r.t - p.t >= THIN) {
            run.push(Point { t: r.t, lat, lon });
        }
    }
    runs
}

/// The moment on the track nearest to `at` on screen, within `radius`. Picks along segments, since
/// at street zoom the points are far apart.
fn nearest_t(track: &[Run], screen: impl Fn(&Point) -> Pos2, at: Pos2, radius: f32) -> Option<f64> {
    let mut best = None;
    let mut best_d = radius;
    for run in track {
        let mut prev: Option<(f64, Pos2)> = None;
        for p in run {
            let b = screen(p);
            let (t, d) = match prev {
                None => (p.t, at.distance(b)),
                Some((ta, a)) => {
                    let ab = b - a;
                    let u = if ab.length_sq() > 0.0 { ((at - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
                    (ta + (p.t - ta) * f64::from(u), at.distance(a + ab * u))
                }
            };
            if d <= best_d {
                (best, best_d) = (Some(t), d);
            }
            prev = Some((p.t, b));
        }
    }
    best
}

/// Where the car was nearest to `t`: what the map holds on while the playhead is in a hole.
fn nearest_in_time(track: &[Run], t: f64) -> Option<(f64, f64)> {
    track.iter().flatten().min_by(|a, b| (a.t - t).abs().total_cmp(&(b.t - t).abs())).map(|p| (p.lat, p.lon))
}

/// Screen rotation that puts `heading` (compass degrees) up. egui's y points down, so a positive angle is clockwise.
fn map_angle(heading_up: bool, heading: f64) -> f32 {
    if heading_up { -(heading.to_radians() as f32) } else { 0.0 }
}

pub fn coords_text(coords: Option<(f64, f64)>) -> String {
    coords.map_or("no position".to_owned(), |(lat, lon)| format!("{lat:.4}, {lon:.4}").replace('-', "−"))
}

fn footer_text(coords: Option<(f64, f64)>, heading: Option<f64>) -> String {
    match heading.filter(|h| h.is_finite()) {
        Some(h) => format!("{} · heading {}°", coords_text(coords), h.rem_euclid(360.0).round() as i32 % 360),
        None => coords_text(coords),
    }
}

/// Lives as long as the window: tiles are expensive, the zoom is the user's.
#[derive(Default)]
pub struct State {
    memory: MapMemory,
    /// Street, satellite. Built on first use, and never in tests: no test may touch the network.
    tiles: [Option<Grey<HttpTiles>>; 2],
    /// Of the current store, rebuilt on demand.
    track: Option<Vec<Run>>,
    /// The last good one, so the map doesn't spin back to north in a hole.
    heading: f64,
}

impl State {
    /// More vehicle data has arrived.
    pub fn new_data(&mut self) {
        self.track = None;
    }

    /// Another event: back on the car, wherever the last one was panned to.
    pub fn new_event(&mut self) {
        self.track = None;
        self.heading = 0.0;
        self.memory.follow_my_position();
    }

    #[cfg(test)]
    pub fn look_at(&mut self, (lat, lon): (f64, f64)) {
        self.memory.center_at(lat_lon(lat, lon));
    }
}

pub struct View<'a> {
    /// The playhead; None without an open event.
    pub now: Option<f64>,
    pub store: Option<&'a Store>,
    pub loading: bool,
    /// event.json's estimate: where to center without GPS.
    pub est: Option<(f64, f64)>,
}

/// `rect` is the whole cell, as tall as the bottom panel. Returns a time to seek to.
pub fn cell(ui: &mut Ui, rect: Rect, settings: &mut Settings, state: &mut State, view: View) -> Option<f64> {
    // Its own child, so the follow button coming and going doesn't renumber the panel's widgets.
    let ui = &mut ui.new_child(UiBuilder::new().max_rect(rect).id_salt("map_cell"));
    let (header, rest) = rect.split_top_bottom_at_y(rect.min.y + HEADER);
    let (body, footer) = rest.split_top_bottom_at_y(rect.max.y - FOOTER);

    let mut row = Row::new(header.shrink2(vec2(16.0, 0.0)));
    let w = theme::toggles_width(ui, &LAYERS, 10.0);
    if let Some(i) = theme::toggles(ui, row.left(w, 0.0), &LAYERS, Some((settings.map_mode == LAYERS[1]) as usize), 10.0) {
        settings.map_mode = LAYERS[i].to_owned();
    }
    let w = theme::toggles_width(ui, &ORIENTATIONS, 10.0);
    if let Some(i) = theme::toggles(ui, row.right(w, 0.0), &ORIENTATIONS, Some(settings.map_heading as usize), 10.0) {
        settings.map_heading = i == 1;
    }
    let sat = settings.map_mode == LAYERS[1];

    let p = ui.painter_at(rect);
    p.hline(rect.x_range(), header.max.y - 0.5, theme::line(LINE));
    p.rect_filled(body, 0.0, BODY_BG);

    let track: &[Run] = state.track.get_or_insert_with(|| view.store.map_or(Vec::new(), |s| extract(&s.records)));
    let car = view.now.and_then(|t| view.store?.at(t)).map(|r| r.tel).filter(|t| valid_fix(t.lat, t.lon));
    if let Some(h) = car.map(|c| c.heading).filter(|h| h.is_finite()) {
        state.heading = h;
    }
    let held = view.now.and_then(|t| nearest_in_time(track, t));
    let center = car.map(|c| (c.lat, c.lon)).or(held).or(view.est.filter(|&(lat, lon)| valid_fix(lat, lon)));

    let mut seek = None;
    if let Some((lat, lon)) = center.filter(|_| body.is_positive()) {
        let tiles: Option<(&mut dyn Tiles, f32)> = if cfg!(test) {
            None
        } else {
            let ctx = ui.ctx().clone();
            Some((state.tiles[sat as usize].get_or_insert_with(|| new_tiles(sat, ctx)), if sat { SAT_ALPHA } else { STREET_ALPHA }))
        };
        let scene = Scene { me: lat_lon(lat, lon), track, now: view.now.unwrap_or(0.0), car, heading: state.heading, heading_up: settings.map_heading };
        seek = map_view(ui, body, tiles, &mut state.memory, scene);
        let credit = boxed(&ui.painter_at(body), Align2::RIGHT_BOTTOM, body.right_bottom(), &attribution(sat), theme::mono(9.0), TEXT2);
        // After the map, so a click here doesn't reach the track under it.
        let link = ui.interact(credit, ui.scope_id().with("attribution"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        link.widget_info(|| WidgetInfo::labeled(Role::Link, true, attribution(sat)));
        if link.clicked() {
            ui.ctx().open_url(OpenUrl::new_tab(source_credit(sat).url));
        }
        if state.memory.detached().is_some() {
            let r = Rect::from_min_size(pos2(body.max.x - 62.0, body.min.y + 6.0), vec2(56.0, 22.0));
            ui.painter().rect_filled(r, 0.0, BG);
            if theme::text_button(ui, r, "follow", true).clicked() {
                state.memory.follow_my_position();
            }
        }
    }
    if view.now.is_some() && track.is_empty() {
        let msg = if view.loading { "reading gps data…" } else { "no gps data" };
        boxed(&ui.painter_at(body), Align2::CENTER_CENTER, body.center(), msg, theme::mono(10.5), TEXT3);
    }

    p.hline(rect.x_range(), footer.min.y + 0.5, theme::line(LINE));
    let text = footer_text(center, car.map(|c| c.heading));
    theme::label(&p, pos2(rect.min.x + 16.0, footer.center().y), &text, theme::mono(10.5), TEXT3, WIDTH - 32.0);
    seek
}

/// What goes on top of the tiles.
struct Scene<'a> {
    /// The center while following: the car, or the best stand-in for it.
    me: Position,
    track: &'a [Run],
    now: f64,
    car: Option<Telemetry>,
    heading: f64,
    heading_up: bool,
}

/// Returns a time to seek to.
fn map_view(ui: &mut Ui, body: Rect, tiles: Option<(&mut dyn Tiles, f32)>, memory: &mut MapMemory, scene: Scene) -> Option<f64> {
    let angle = map_angle(scene.heading_up, scene.heading);
    let view = if scene.heading_up { Rect::from_center_size(body.center(), Vec2::splat(body.size().length().ceil())) } else { body };
    let child = &mut ui.new_child(UiBuilder::new().max_rect(view).id_salt("map"));
    // Also keeps the oversized map's input to the cell: egui intersects a widget's hit rect with this.
    child.set_clip_rect(body);
    let layer = child.layer_id();
    let first = child.ctx().graphics_mut(|g| g.entry(layer).next_idx());

    let mut map = Map::new(None, memory, scene.me).zoom_gesture(false).drag_pan_buttons(DragPanButtons::empty()).panning(false);
    if let Some((tiles, alpha)) = tiles {
        map = map.with_layer(tiles, alpha);
    }
    let out = map.show(child, |ui, response, projector, _| {
        let c = response.rect.center();
        rotate_shapes(ui.ctx(), layer, first, c, angle);
        let rot = Rot2::from_angle(angle);
        let to_screen = |lat: f64, lon: f64| c + rot * (projector.project(lat_lon(lat, lon)) - c.to_vec2());
        let screen = |p: &Point| to_screen(p.lat, p.lon);

        let painter = ui.painter();
        paint_track(painter, scene.track, screen, scene.now, body);
        if let Some(car) = scene.car {
            paint_car(painter, to_screen(car.lat, car.lon), Rot2::from_angle(scene.heading.to_radians() as f32 + angle));
        }

        let hit = response.hover_pos().and_then(|at| nearest_t(scene.track, screen, at, PICK_RADIUS));
        if hit.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        // The pointer moves on screen, the center moves on the unrotated map.
        let pan = response.dragged().then(|| projector.unproject(c.to_vec2() - rot.inverse() * response.drag_delta()));
        (hit.filter(|_| response.clicked()), pan)
    });
    let (seek, pan) = out.inner;
    if let Some(center) = pan {
        memory.center_at(center);
    }
    if out.response.hovered() {
        let dz = ui.input(|i| f64::from(i.zoom_delta()).log2() + f64::from(i.smooth_scroll_delta.y) / 100.0);
        // shortcut: zooms about the cell's center (the car, while following), not the pointer; do the rotated math if panning a long track gets tedious.
        let _ = memory.set_zoom((memory.zoom() + dz).clamp(ZOOM_MIN, ZOOM_MAX));
    }
    seek
}

/// Turns the meshes painted on `layer` since `first` about `origin`. walkers paints each tile as its own mesh.
fn rotate_shapes(ctx: &Context, layer: LayerId, first: ShapeIdx, origin: Pos2, angle: f32) {
    if angle == 0.0 {
        return;
    }
    let rot = Rot2::from_angle(angle);
    ctx.graphics_mut(|g| {
        let list = g.entry(layer);
        for i in first.0..list.next_idx().0 {
            list.mutate_shape(ShapeIdx(i), |c| {
                if let Shape::Mesh(m) = &mut c.shape {
                    Arc::make_mut(m).rotate(rot, origin);
                }
            });
        }
    });
}

/// Driven solid up to the playhead, still to come dashed, as in the mockup. Both have a dark casing,
/// since the tiles under them may be light. Off-screen stretches are skipped: at street zoom a long
/// track would otherwise be thousands of dashes per frame.
fn paint_track(p: &Painter, track: &[Run], screen: impl Fn(&Point) -> Pos2, now: f64, body: Rect) {
    let bounds = body.expand(8.0);
    let mut coming: Vec<Vec<Pos2>> = Vec::new();
    let mut driven: Vec<Vec<Pos2>> = Vec::new();
    for run in track {
        let mut prev: Option<(Pos2, Option<bool>)> = None;
        for q in run {
            let b = screen(q);
            let Some((a, last)) = prev else {
                prev = Some((b, None));
                continue;
            };
            let class = bounds.intersects(Rect::from_two_pos(a, b)).then_some(q.t <= now);
            if let Some(is_driven) = class {
                let lines = if is_driven { &mut driven } else { &mut coming };
                match lines.last_mut().filter(|_| last == class) {
                    Some(line) => line.push(b),
                    None => lines.push(vec![a, b]),
                }
            }
            prev = Some((b, class));
        }
    }
    for line in coming {
        p.add(Shape::line(line.clone(), Stroke::new(3.5, BG)));
        p.extend(Shape::dashed_line(&line, Stroke::new(1.5, OFF), 4.0, 4.0));
    }
    for line in driven {
        p.add(Shape::line(line.clone(), Stroke::new(4.5, BG)));
        p.add(Shape::line(line, Stroke::new(2.5, TEXT)));
    }
}

/// The track fitted to `rect` with the car on it, for the composited export.
// shortcut: no tiles under the track, the export worker can't reach walkers' cache; hand it finished tile images if people want the street map in the video.
pub fn bare_track(p: &Painter, records: &[Record], now: f64, heading: Option<f64>, rect: Rect) {
    let p = p.with_clip_rect(p.clip_rect().intersect(rect));
    p.rect_filled(rect, 0.0, BODY_BG);
    let track = extract(records);
    let Some(first) = track.iter().flatten().next() else {
        p.text(rect.center(), Align2::CENTER_CENTER, "no gps data", theme::mono(11.0), TEXT3);
        return;
    };
    let (mut lat, mut lon) = ((first.lat, first.lat), (first.lon, first.lon));
    for q in track.iter().flatten() {
        (lat, lon) = ((lat.0.min(q.lat), lat.1.max(q.lat)), (lon.0.min(q.lon), lon.1.max(q.lon)));
    }
    // A degree of longitude shrinks with latitude.
    let (mid_lat, mid_lon) = ((lat.0 + lat.1) / 2.0, (lon.0 + lon.1) / 2.0);
    let squeeze = mid_lat.to_radians().cos();
    let inner = rect.shrink(12.0);
    let per_px = ((lon.1 - lon.0) * squeeze / inner.width() as f64).max((lat.1 - lat.0) / inner.height() as f64).max(1e-9);
    let screen = |la: f64, lo: f64| rect.center() + vec2(((lo - mid_lon) * squeeze / per_px) as f32, ((mid_lat - la) / per_px) as f32);
    paint_track(&p, &track, |q| screen(q.lat, q.lon), now, rect);
    if let Some((la, lo)) = nearest_in_time(&track, now) {
        paint_car(&p, screen(la, lo), Rot2::from_angle(heading.unwrap_or(0.0).to_radians() as f32));
    }
}

/// The mockup's notched arrow, pointing along `rot`. A mesh, as it isn't convex.
fn paint_car(p: &Painter, at: Pos2, rot: Rot2) {
    let points = [vec2(0.0, -9.0), vec2(7.0, 7.0), vec2(0.0, 3.0), vec2(-7.0, 7.0)].map(|v| at + rot * v);
    let mut mesh = Mesh::default();
    for q in points {
        mesh.colored_vertex(q, Color32::WHITE);
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(Shape::mesh(mesh));
    p.add(Shape::closed_line(points.to_vec(), Stroke::new(1.5, BG)));
}

/// Text on a background patch, to stay legible on any tile. Returns the patch.
fn boxed(p: &Painter, anchor: Align2, at: Pos2, s: &str, font: FontId, color: Color32) -> Rect {
    let galley = p.layout_no_wrap(s.to_owned(), font, color);
    let rect = anchor.anchor_size(at, galley.size() + vec2(10.0, 2.0));
    p.rect_filled(rect, 0.0, BG);
    p.galley(rect.min + vec2(5.0, 1.0), galley, color);
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{ColorImage, Event, Modifiers, MouseWheelSource, MouseWheelUnit, PointerButton, RawInput, TouchPhase};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use walkers::lon_lat;

    fn rec(t: f64, lat: f64, lon: f64) -> Record {
        Record { t, g: 0.0, tel: Telemetry { lat, lon, ..Default::default() } }
    }

    /// Points on the x axis; a point's lon is its x, so `flat` is the screen.
    fn run(points: &[(f64, f64)]) -> Run {
        points.iter().map(|&(t, x)| Point { t, lat: 1.0, lon: x }).collect()
    }

    fn flat(p: &Point) -> Pos2 {
        pos2(p.lon as f32, 0.0)
    }

    #[test]
    fn the_track_drops_bad_fixes_and_breaks_at_them() {
        let mut records: Vec<Record> = (0..4).map(|i| rec(f64::from(i) * 0.3, 52.0, 13.0)).collect();
        records.push(rec(1.2, 0.0, 0.0));
        records.extend((5..8).map(|i| rec(f64::from(i) * 0.3, 52.0, 13.0)));
        records.push(rec(2.4, f64::NAN, 13.0));
        records.push(rec(2.7, 95.0, 13.0));
        let runs = extract(&records);
        assert_eq!(runs.iter().map(Vec::len).collect::<Vec<_>>(), [4, 3]);
        assert!(runs.iter().flatten().all(|p| p.lat == 52.0));
        assert_eq!(extract(&[rec(0.0, 0.0, 0.0), rec(0.1, 0.0, 0.0)]), Vec::<Run>::new());
        // One coordinate at zero is a real place: the equator or the prime meridian.
        assert_eq!(extract(&[rec(0.0, 0.0, 13.0), rec(0.3, 51.5, 0.0)]).len(), 1);
    }

    #[test]
    fn the_track_breaks_at_holes_in_time_and_thins_dense_samples() {
        let mut records: Vec<Record> = (0..100).map(|i| rec(f64::from(i) * 0.03, 52.0, 13.0)).collect();
        records.extend((0..3).map(|i| rec(10.0 + f64::from(i) * 0.03, 52.0, 13.0)));
        let runs = extract(&records);
        assert_eq!(runs.len(), 2, "more than HOLD apart");
        assert!((10..=13).contains(&runs[0].len()), "3 s at ≥ 0.25 s spacing, got {}", runs[0].len());
        assert_eq!(runs[1].len(), 1);
        assert!(runs[0].windows(2).all(|w| w[1].t - w[0].t >= THIN));
    }

    #[test]
    fn picks_along_segments_within_the_radius() {
        let track = [run(&[(10.0, 0.0), (20.0, 100.0)])];
        let t = nearest_t(&track, flat, pos2(25.0, 5.0), 10.0).unwrap();
        assert!((t - 12.5).abs() < 1e-3, "{t}");
        assert_eq!(nearest_t(&track, flat, pos2(25.0, 11.0), 10.0), None);
        // Past the end of the line: only within the radius of the end point.
        assert_eq!(nearest_t(&track, flat, pos2(105.0, 0.0), 10.0), Some(20.0));
        assert_eq!(nearest_t(&track, flat, pos2(115.0, 0.0), 10.0), None);
        assert_eq!(nearest_t(&[], flat, pos2(0.0, 0.0), 10.0), None);
    }

    #[test]
    fn picking_does_not_bridge_gaps() {
        let track = [run(&[(10.0, 0.0), (11.0, 10.0)]), run(&[(50.0, 100.0), (51.0, 110.0)])];
        assert_eq!(nearest_t(&track, flat, pos2(55.0, 0.0), 10.0), None, "the hole between the runs");
        assert_eq!(nearest_t(&track, flat, pos2(98.0, 0.0), 10.0), Some(50.0));
        // A one-point run can still be picked.
        let lone = [run(&[(7.0, 50.0)])];
        assert_eq!(nearest_t(&lone, flat, pos2(50.0, 4.0), 10.0), Some(7.0));
    }

    #[test]
    fn picking_finds_the_closest_stretch() {
        let track = [run(&[(1.0, 0.0), (2.0, 10.0)]), run(&[(5.0, 16.0), (6.0, 30.0)])];
        assert_eq!(nearest_t(&track, flat, pos2(15.0, 0.0), 10.0), Some(5.0));
        assert_eq!(nearest_t(&track, flat, pos2(11.0, 0.0), 10.0), Some(2.0));
    }

    #[test]
    fn heading_up_puts_the_heading_up() {
        for (heading, ahead) in [(0.0, vec2(0.0, -1.0)), (90.0, vec2(1.0, 0.0)), (180.0, vec2(0.0, 1.0)), (270.0, vec2(-1.0, 0.0))] {
            let up = Rot2::from_angle(map_angle(true, heading)) * ahead;
            assert!((up - vec2(0.0, -1.0)).length() < 1e-5, "{heading}: {up:?}");
        }
        assert_eq!(map_angle(false, 123.0), 0.0);
    }

    #[test]
    fn picking_works_on_a_rotated_map() {
        // A track running east of the center, on a map heading east: on screen it runs straight up.
        let c = pos2(100.0, 100.0);
        let rot = Rot2::from_angle(map_angle(true, 90.0));
        let screen = |p: &Point| c + rot * vec2(p.lon as f32, 0.0);
        let track = [run(&[(0.0, 0.0), (10.0, 50.0)])];
        let t = nearest_t(&track, screen, pos2(100.0, 75.0), 5.0).unwrap();
        assert!((t - 5.0).abs() < 1e-3, "{t}");
        assert_eq!(nearest_t(&track, screen, pos2(125.0, 100.0), 5.0), None, "where it would be on a north-up map");
    }

    #[test]
    fn coordinates_line() {
        assert_eq!(coords_text(Some((37.80441, -122.27119))), "37.8044, −122.2712");
        assert_eq!(coords_text(None), "no position");
        let at = Some((37.8044, -122.2712));
        assert_eq!(footer_text(at, Some(352.0)), "37.8044, −122.2712 · heading 352°");
        assert_eq!(footer_text(at, Some(359.7)), "37.8044, −122.2712 · heading 0°");
        assert_eq!(footer_text(at, Some(-8.0)), "37.8044, −122.2712 · heading 352°");
        assert_eq!(footer_text(at, Some(f64::NAN)), "37.8044, −122.2712");
        assert_eq!(footer_text(at, None), "37.8044, −122.2712");
    }

    #[test]
    fn nearest_in_time_holds_the_closest_point() {
        let track = [vec![Point { t: 1.0, lat: 1.0, lon: 1.0 }], vec![Point { t: 9.0, lat: 2.0, lon: 2.0 }]];
        assert_eq!(nearest_in_time(&track, 3.0), Some((1.0, 1.0)));
        assert_eq!(nearest_in_time(&track, 8.0), Some((2.0, 2.0)));
        assert_eq!(nearest_in_time(&[], 8.0), None);
    }

    #[test]
    fn tile_policy_strings() {
        assert_eq!(USER_AGENT, format!("OpenTeslaCam/{}", env!("CARGO_PKG_VERSION")));
        assert_eq!(attribution(false), "© OpenStreetMap contributors");
        assert!(attribution(true).contains("Esri"));
        assert_eq!(source_credit(false).url, "https://www.openstreetmap.org/copyright");
        // Esri's URLs go z/y/x, unlike OSM's z/x/y.
        let url = Esri.tile_url(TileId { x: 1, y: 2, zoom: 3 });
        assert!(url.ends_with("/World_Imagery/MapServer/tile/3/2/1"), "{url}");
    }

    /// Building the clients must not panic. Nothing is fetched until a tile is asked for, which no test does.
    #[test]
    fn tile_clients_build() {
        new_tiles(false, Context::default());
        new_tiles(true, Context::default());
    }

    #[test]
    fn cache_folder_comes_from_the_environment() {
        assert_eq!(cache_dir(|_| None), None);
        let dir = cache_dir(|k| Some(format!("/base/{k}"))).unwrap();
        assert!(dir.ends_with("openteslacam/tiles"), "{dir:?}");
        assert!(dir.starts_with("/base"), "{dir:?}");
    }

    /// Every tile is ready, and comes from nowhere.
    struct Solid(Context);

    impl Tiles for Solid {
        fn at(&mut self, _: TileId) -> Option<TilePiece> {
            let texture = self.0.load_texture("tile", ColorImage::filled([1, 1], Color32::WHITE), Default::default());
            Some(TilePiece::new(Tile::Raster(texture), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))))
        }

        fn attribution(&self) -> Attribution {
            Esri.attribution()
        }

        fn tile_size(&self) -> u32 {
            256
        }
    }

    /// Uploads a new tile per call to the `Context` it's given, as walkers does.
    struct Colorful(Context, Vec<TextureHandle>);

    impl Tiles for Colorful {
        fn at(&mut self, _: TileId) -> Option<TilePiece> {
            let texture = self.0.load_texture("image", ColorImage::filled([2, 2], Color32::from_rgb(200, 40, 10)), Default::default());
            self.1.push(texture.clone());
            Some(TilePiece::new(Tile::Raster(texture), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 0.5))))
        }

        fn attribution(&self) -> Attribution {
            Esri.attribution()
        }

        fn tile_size(&self) -> u32 {
            256
        }
    }

    #[test]
    fn grey_keeps_brightness_and_alpha() {
        assert_eq!(grey(Color32::WHITE), Color32::WHITE);
        assert_eq!(grey(Color32::BLACK), Color32::BLACK);
        assert_eq!(grey(Color32::TRANSPARENT), Color32::TRANSPARENT);
        let [r, g, b, a] = grey(Color32::from_rgb(0, 200, 0)).to_array();
        assert!(r == g && g == b && a == 255);
        assert!(r > grey(Color32::from_rgb(0, 0, 200)).r(), "green reads brighter than blue");
        // Half-transparent white, premultiplied.
        assert_eq!(grey(Color32::from_rgba_premultiplied(128, 128, 128, 128)).a(), 128);
    }

    #[test]
    fn tiles_reach_the_window_in_grey_and_go_when_walkers_drops_them() {
        let id = TileId { x: 0, y: 0, zoom: 1 };
        let window = Context::default();
        let mut tiles = Grey::new(window.clone(), |c| Colorful(c, Vec::new()));
        let piece = tiles.at(id).unwrap();
        assert_eq!(piece.uv.max.y, 0.5, "the piece of it walkers asked for");
        let Tile::Raster(shown) = &piece.tile;
        let mut delta = window.tex_manager().write().take_delta();
        let ImageData::Color(image) = &delta.set[&shown.id()][0].image;
        assert_eq!(image.size, [2, 2]);
        assert!(image.pixels.iter().all(|p| *p == grey(Color32::from_rgb(200, 40, 10))), "{:?}", image.pixels[0]);
        delta.clear();

        // A dropped tile lets go of the window's texture, once the piece painted with it is gone too.
        let shown_id = shown.id();
        drop(piece);
        tiles.tiles.1.clear();
        tiles.at(id).unwrap();
        assert_eq!(tiles.shown.len(), 1);
        let mut delta = window.tex_manager().write().take_delta();
        assert!(delta.free.contains(&shown_id));
        delta.clear();
    }

    /// A tile finishing downloads must wake a sleeping window, every time.
    #[test]
    fn tile_arrivals_repaint_the_window() {
        let window = Context::default();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        window.set_repaint_observer(move |_| {
            seen.fetch_add(1, Ordering::Relaxed);
        });
        let tiles = Grey::new(window.clone(), |c| Colorful(c, Vec::new()));
        tiles.decoded.request_repaint();
        tiles.decoded.request_repaint();
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    /// Clip rect and mesh of each tile.
    fn painted_tiles(heading_up: bool, heading: f64, body: Rect) -> Vec<(Rect, Mesh)> {
        let ctx = Context::default();
        let mut tiles = Solid(ctx.clone());
        let mut memory = MapMemory::default();
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 1000.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            let scene = Scene { me: lon_lat(13.0, 52.0), track: &[], now: 0.0, car: None, heading, heading_up };
            map_view(ui, body, Some((&mut tiles, 1.0)), &mut memory, scene);
        });
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|c| match &c.shape {
                Shape::Mesh(m) if m.texture_id != TextureId::default() => Some((c.clip_rect, (**m).clone())),
                _ => None,
            })
            .collect()
    }

    /// Runs the map through one frame per entry, and returns its memory.
    fn gesture(heading: f64, frames: Vec<Vec<Event>>) -> MapMemory {
        let ctx = Context::default();
        let mut memory = MapMemory::default();
        let body = Rect::from_min_size(pos2(100.0, 300.0), vec2(288.0, 200.0));
        for events in frames {
            let input = RawInput { events, screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 1000.0))), ..Default::default() };
            ctx.run_ui(input, |ui| {
                let scene = Scene { me: lat_lon(52.0, 13.0), track: &[], now: 0.0, car: None, heading, heading_up: true };
                map_view(ui, body, None, &mut memory, scene);
            })
            .textures_delta
            .clear();
        }
        memory
    }

    #[test]
    fn a_drag_follows_the_pointer_on_the_rotated_map() {
        let (a, b) = (pos2(200.0, 400.0), pos2(280.0, 400.0));
        let button = |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        let drag = |heading| {
            let frames = vec![vec![Event::PointerMoved(a)], vec![button(a, true)], vec![Event::PointerMoved(a + vec2(40.0, 0.0))], vec![Event::PointerMoved(b)], vec![button(b, false)]];
            gesture(heading, frames).detached().expect("a drag detaches")
        };
        // Heading east, screen right is south: pulling the map that way brings up what's north of the car.
        let east = drag(90.0);
        assert!(east.y() > 52.0, "{east:?}");
        assert!((east.x() - 13.0).abs() < 1e-6, "{east:?}");
        // Heading north, as on a plain map: pulling it right shows what's west.
        let north = drag(0.0);
        assert!(north.x() < 13.0, "{north:?}");
        assert!((north.y() - 52.0).abs() < 1e-6, "{north:?}");
    }

    #[test]
    fn the_wheel_zooms_in_steps_within_limits_and_keeps_following() {
        let at = pos2(200.0, 400.0);
        let wheel = |y| Event::MouseWheel { unit: MouseWheelUnit::Point, delta: vec2(0.0, y), phase: TouchPhase::Move, modifiers: Modifiers::NONE, source: MouseWheelSource::Wheel };
        let frames = |y| {
            let mut f = vec![vec![Event::PointerMoved(at)], vec![Event::PointerMoved(at)]];
            f.extend((0..200).map(|_| vec![wheel(y)]));
            f
        };
        let start = MapMemory::default().zoom();
        let zoomed = gesture(0.0, frames(100.0));
        assert_eq!(zoomed.zoom(), ZOOM_MAX, "from {start}");
        assert!(zoomed.detached().is_none(), "still on the car");
        assert_eq!(gesture(0.0, frames(-100.0)).zoom(), ZOOM_MIN);
    }

    /// The real walkers drawing path, with the rotation that can't be looked at in a test.
    #[test]
    fn tiles_rotate_and_stay_in_the_cell() {
        let body = Rect::from_min_size(pos2(100.0, 300.0), vec2(288.0, 200.0));
        let north = painted_tiles(false, 30.0, body);
        assert!(!north.is_empty());
        for (clip, mesh) in &north {
            assert_eq!(*clip, body);
            assert_eq!(mesh.vertices[0].pos.x, mesh.vertices[2].pos.x, "axis-aligned");
        }

        let tiles = painted_tiles(true, 30.0, body);
        assert!(tiles.len() >= north.len());
        let back = Rot2::from_angle(30_f32.to_radians());
        let mut covered = Rect::NOTHING;
        for (clip, mesh) in tiles {
            assert_eq!(clip, body, "the oversized map is clipped to the cell");
            assert!((mesh.vertices[0].pos.x - mesh.vertices[2].pos.x).abs() > 1.0, "turned");
            let mut unturned = mesh;
            unturned.rotate(back, body.center());
            covered = covered.union(unturned.calc_bounds());
        }
        // Whatever the angle, tiles reach every corner of the cell.
        for corner in [body.left_top(), body.right_top(), body.left_bottom(), body.right_bottom()] {
            let c = body.center() + back * (corner - body.center());
            assert!(covered.contains(c), "{corner:?} not under {covered:?}");
        }
    }
}
