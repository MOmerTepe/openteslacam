# OpenTeslaCam (Rust): plan

A native port of `openteslacam-legacy/` (Electron + Leaflet + GridStack + Tesla's JS parser).
It does the same job: open a `TeslaCam` folder, play all 6 cameras in sync on one timeline,
and overlay the vehicle data that Tesla embeds in the H.264 stream.

## Key decisions

### 1. Tesla's dashcam tools are the spec and the test oracle; they don't ship with the app
[teslamotors/dashcam](https://github.com/teslamotors/dashcam) gives us two things:

- `dashcam.proto`: the `SeiMetadata` schema (16 fields).
- `sei_extractor.py` / `dashcam-mp4.js`: how the data is wrapped inside the MP4.

The format is small, so we reimplement it in about 150 lines of Rust instead of running
Tesla's JS:

```
MP4 sample ─► 4-byte big-endian NAL length ─► NAL type (b0 & 0x1F) == 6 (SEI)
            ─► payload type (b1) == 5 (user_data_unregistered)
            ─► from byte 3: skip 0x42…, expect 0x69 ─► drop trailing byte
            ─► strip emulation-prevention 0x03 (after 00 00) ─► protobuf SeiMetadata
```

The protobuf decoder is written by hand: 16 fields and three wire types (varint, fixed32,
fixed64). That means no `prost`, no `protoc` and no build script.

| # | field | type | # | field | type |
|---|---|---|---|---|---|
| 1 | version | u32 | 9 | brake_applied | bool |
| 2 | gear_state | enum P/D/R/N | 10 | autopilot_state | enum None/FSD/Autosteer/TACC |
| 3 | frame_seq_no | u64 | 11 | latitude_deg | f64 |
| 4 | vehicle_speed_mps | f32 | 12 | longitude_deg | f64 |
| 5 | accelerator_pedal_position | f32 (%) | 13 | heading_deg | f64 |
| 6 | steering_wheel_angle | f32 (deg) | 14–16 | linear_acceleration_mps2_x/y/z | f64 |
| 7–8 | blinker_on_left/right | bool | | | |

Why this approach:
- The "download `dashcam-master/` and drop it in" setup step goes away. The HUD and map just work.
- None of Tesla's unlicensed files are redistributed, so release binaries can be published on GitHub.
- It's more accurate than Tesla's Python tool. That tool scans the raw `mdat` and has no frame
  times. We walk the sample table, so every SEI record gets the exact timestamp of the frame it
  belongs to. The flip side: a recording cut short before its `moov` was written (Tesla writes
  it last, so power loss does this) has no sample table. Tesla's tool still prints its SEI rows; we
  reject the clip as truncated. The oracle test reports that case as a known divergence.

Tesla's tools stay in the dev loop. A test runs `sei_extractor.py` on sample clips and diffs its
CSV output against ours (see Phase 0).

### 2. Dependencies (5 direct)

| Crate | Why |
|---|---|
| `eframe` / `egui` | Native window and GPU rendering on Win/macOS/Linux. Immediate mode suits custom widgets (timeline, HUD, telemetry lanes). Also handles drag-and-drop and settings persistence. |
| `openh264` | H.264 decode (later also encode). Builds from bundled source, so there's no system FFmpeg to install or link. |
| `rfd` | Native "Open Folder" dialog. |
| `serde_json` | Reads `event.json`. |
| `walkers` | Slippy-map widget for egui, with tile fetch and cache (Phase 3). |

Written by hand: the MP4 demuxer (`moov/trak/stbl`: `stsd/avcC`, `stts`, `stss`, `stsz`,
`stco/co64`, `mdhd`), the SEI parser and the protobuf decoder. Tesla files are plain,
non-fragmented, single-track MP4. One sample table serves both decoding and SEI.

FFmpeg was ruled out: it's heavy to link on Windows and adds licensing trouble for packaging.
GStreamer and libmpv need a system install.

### 3. Sync model: one master clock
The legacy app slaved 5 `<video>` elements to the front camera and corrected drift above 100 ms,
which makes the side cameras jitter. The native app works differently:

- `Player` owns a single clock (`Instant`-based, with a rate of ¼×–4×).
- Each camera has a **decoder thread**. Given a target time `t`, it returns the latest frame with
  `pts ≤ t`. A seek decodes forward from the preceding keyframe (from `stss`).
- The UI uploads a texture only when a camera's frame index changes.
- While the scrubber is dragged, decoders show keyframes only (fast). On release they decode
  the exact frame.

All 6 views show the same instant by construction.

### 4. Timeline = wall clock
A segment's start time comes from its filename (`YYYY-MM-DD_HH-MM-SS`) and its length from
`stts`. The timeline shows real time of day. Gaps in `RecentClips` show up as gaps instead of
being silently glued together.

## Architecture

```
src/
  main.rs        eframe bootstrap, app state, keyboard shortcuts
  library.rs     scan TeslaCam/ → Vec<Event { kind, name, segments, info: Option<EventJson>, thumb }>
                 (SavedClips/<evt>/, SentryClips/<evt>/, RecentClips/ flat, or one clip folder)
                 detects encrypted clips (no ftyp/moov) and marks them instead of failing
  mp4.rs         box walker → Track { timescale, avcc, samples: Vec<Sample{offset,size,pts,key}> }
  sei.rs         samples → Vec<Telemetry> (frame-indexed) + hand-rolled protobuf decode
  decode.rs      per-camera worker thread: openh264, seek/next, RGBA out via channel
  player.rs      master clock, global↔(segment, local) time mapping, segment preload
  ui/
    grid.rs      camera tiles, layout presets (grid/center/driving), custom layout editor, focus mode
    vehicle.rs   vehicle strip at the top of the bottom panel: speed, gear, brake, accel,
                 steering, blinkers, hazards, AP, g
    timeline.rs  scrubber + segment strip + telemetry lanes + event marker + trim range
    map.rs       walkers map, track polyline, heading arrow (bottom-left cell, under the library)
    events.rs    left sidebar: clip list, filters, thumbnails
  export.rs      Phase 4
```

There's a single binary and no `resources/` folder.

## GUI

Mockup: https://claude.ai/artifact/XmxzHbWfca7151k4BvtMrK (each of the 4 layouts with the library open and closed, plus the Export dialog).
### Design rules
1. **Nothing is drawn over the video, and the video gets the space.** The cameras span the full width between the library panel and the window edge; there is no right-hand panel. Camera names and the focus button sit in a caption strip above each tile. The window has two rows. **Top row:** the library panel (closable; its event list scrolls) and the cameras. Closing the library gives its width to the cameras. **Bottom row:** the map in a 288px cell on the left, always visible and exactly as tall as the bottom panel, then the bottom panel itself: a vehicle strip on top, then transport controls and the timeline. The only things that may cover video are modal dialogs.
2. **Monochrome, hard edges.** Follows omertepe.com: ground `#141413`, text `#e6e6e1` / `#b0b0a8` / `#93938b`, lines `#2a2a27` / `#4a4a45`. No accent color, `Rounding::ZERO` everywhere. IBM Plex Mono for labels, controls and numbers; IBM Plex Sans for prose (both embedded as egui fonts).
3. **All controls in a row are the same size.** The primary action is set apart by color (filled `#e6e6e1` with dark text), never by size. An active toggle is shown by an underline.
4. **Each signal is drawn in the form that fits its data.**

   | Signal | Data | Timeline lane | Vehicle strip |
   |---|---|---|---|
   | speed | continuous ≥ 0 | area chart | big number |
   | accelerator | continuous 0–100 % | area **up** from the center line of the *pedals* lane | `%` |
   | brake | on/off | solid blocks **down** from the same center line | `on` (filled) / `off` |
   | steering angle | signed continuous | line around zero (left up, right down) | `16° left` |
   | blinkers | on/off + direction | `‹` / `›` chevrons, one per flash, on the steering lane (left above, right below) | two arrows, lit or unlit |
   | hazards | derived: both blinkers on | chevrons on both sides at once | triangle, lit or unlit |
   | autopilot | state enum | labeled band (`autosteer`, `tacc`, `fsd`) | state name (filled when engaged) |
   | gear | state enum | (none) | P R N D with the current gear filled |
   | g-force | derived | "hard brake / accel" marker above the threshold | signed value, filled above the threshold |
5. **Lit vs. unlit** is bright `#e6e6e1` against dim `#4a4a45`. With no color available, brightness carries the signal.

| Region | egui building block |
|---|---|
| Top bar: event title, layout (grid · center · driving · custom), km/h·mph, Open folder, `?` | `TopBottomPanel::top` + `selectable_value` segmented buttons |
| Library: filters with counts, a scrolling list of event rows (thumb, badge, reason, duration; Encrypted rows disabled). Closable from the header; when closed it is gone entirely and the cameras take its width | `SidePanel::left` (toggled with `show_animated`) + `ScrollArea`, declared *after* the bottom panel so it only spans the top row |
| Camera area: one of 4 layouts (see Layouts below); caption strip per tile; double-click to focus | `CentralPanel`; each layout is a list of cell rects in 12 × 8 grid units, scaled to the free space; video is letterboxed inside its cell (keeps 4:3); `Image` from per-camera `TextureHandle` |
| Custom toolbar (custom layout only): saved layouts, `+ new`, `edit layout` / `done`; while editing: hidden cameras to re-add, `reset`, `save` | A row above the camera area; while editing, captions are drag handles, each tile has a resize handle and a hide button, cells snap to the 12 × 8 grid |
| Bottom panel, vehicle strip: one row of cells (speed, gear, brake, accelerator, steering, blinkers, hazards, autopilot, g-force) separated by 1px rules | `TopBottomPanel::bottom`, a `ui.horizontal` of fixed-width cells painted with `Painter` |
| Map (bottom-left cell): Map/Sat and North/Heading toggles, track, coordinates and heading, OSM attribution; height follows the bottom panel | Left column inside `TopBottomPanel::bottom` holding a `walkers::Map` |
| Timeline: transport, wall-clock time, speed ¼–4×, In/Out, Export, telemetry toggle | `TopBottomPanel::bottom` |
| Scrubber: time ticks, segment strip, lanes for speed, pedals (gas up / brake down), steering with blinker chevrons, and autopilot bands; trim range, event marker, hard-brake marker, playhead | One custom widget: `allocate_response` + `Painter`. Click or drag to seek; the lanes are precomputed `Shape::line` / rects per event |
| Export dialog: lossless per camera or composited, camera picks, resolution, HUD/map toggles, CSV, destination | Modal `Window` + `rfd` save dialog |

### Layouts
Every layout is a list of `(camera, x, y, w, h)` cells on a 12 × 8 grid. The presets are just
built-in lists, so all four go through the same layout code.

| Layout | Arrangement |
|---|---|
| **grid** | All six equal: left pillar · front · right pillar over left repeater · back · right repeater. |
| **center** | Front large in the middle with a smaller back camera under it. Left pillar and left repeater stacked on the left, right pillar and right repeater on the right. |
| **driving** | Front as large as possible on the left. Back, left repeater and right repeater stacked on the right; the pillars are hidden for maximum front detail. |
| **custom** | User-defined. In edit mode you drag a caption to move a camera, drag the corner handle to resize, `×` to hide, and re-add hidden cameras from the toolbar. `save` stores it, `+ new` saves a copy under a new name. Overlaps are allowed; the last-moved tile is on top. |

Custom layouts are saved with the other settings (eframe persistence) as
`{ name: { camera: [x, y, w, h] | null } }`. Closing the library gives its width to the camera
area in every layout.

## Phases

**Phase 0: spike (go/no-go on software decode)**
- `mp4.rs` and `sei.rs`, plus a `--dump-sei clip.mp4` CLI that prints CSV in the same column
  order as `sei_extractor.py`.
- Test: for each clip in `testdata/` (git-ignored real clips), our CSV must equal Tesla's CSV.
- Decode one camera with openh264 into an egui window.
- **Measure** decode speed with 6 streams on an HW3 clip set (6 × 1280×960) and an HW4 set
  (front 2896×1876 plus 1448×938 side cameras) at 1× and 4×.
  - If HW4 at 4× can't keep up: decode only the visible or focused cameras at full rate and
    keyframes-only for small tiles. Hardware decode (Media Foundation / VideoToolbox) behind a
    cargo feature is the later upgrade.

**Phase 1: viewer parity (core)**
- Open Folder, drag-and-drop a folder, scan the library.
- Event sidebar with All / Saved / Sentry / Recent filters and `thumb.png` thumbnails.
- Camera layouts (see Layouts). Double-click a tile to focus it, `Esc` to go back.
- Play/pause, speed ¼–4×, seek ±5 s and ±30 s, frame step `,` `.`, Prev/Next segment, Home/End.
- Segment strip for multi-part events.
- Settings persisted: units, layout, hidden cameras, map mode, last folder.

**Phase 2: vehicle data**
- **Vehicle strip** (top of the bottom panel; replaces the legacy HUD overlay): speed (km/h or
  mph), gear, brake, accelerator %, blinkers, hazards, Autopilot state, g-force, and
  **steering-wheel angle** (new).
- **Telemetry lanes** under the scrubber (new), each drawn in the form from design rule 4: speed
  area chart, a pedals lane (gas up, brake down), a steering trace with blinker chevrons, and
  labeled Autopilot bands. You can see where the hard stop happened before you play anything.
- **Event marker** (new): `event.json` timestamp and reason (`sentry_aware_object_detection`,
  `user_interaction_honk`, …) drawn on the timeline. Opening a Sentry or Saved event starts
  playback 10 s before the trigger.
- **G-force** readout from `linear_acceleration_*`. Peaks above a threshold become "hard brake"
  and "hard acceleration" markers.
- Export telemetry as CSV (trivial: same rows as the HUD).

**Phase 3: map**
- `walkers` minimap with Street (OSM) and Satellite (Esri) layers, North-up or heading-up.
- The whole GPS track is drawn as a polyline, with the car position and heading marker on top.
  Click the track to seek to that moment.
- Must follow OSM's tile policy: a real `User-Agent` and on-map attribution. This is the only
  outbound traffic, same as the legacy app.

**Phase 4: trim & export** (replaces the legacy WebM `MediaRecorder` export)
- Set In/Out with `I` / `O`. The trim range is drawn on the timeline.
- 4a, **lossless clip export**: copy the H.264 samples from the keyframe at or before In through
  Out into a new MP4 for each selected camera. A minimal MP4 muxer (`ftyp/moov/mdat`, about
  200 lines) does this with no re-encode, so it's fast and keeps full quality.
- 4b, **composited export**: the current grid, with a vehicle-data strip and map placed beside it
  (design rule 1 applies here too), rendered into one frame, encoded with the
  openh264 encoder and muxed to MP4 at 1080p, 720p or 4K. This is the "share one video" option.

**Phase 5: release**
- GitHub Actions builds Win/macOS/Linux binaries on tags.
- Replace `openteslacam-legacy/` README sections: no more `dashcam-master/` setup step.

## Kept vs dropped from legacy
- Kept: everything in the legacy feature list and the shortcuts table.
- Changed: the free-form GridStack grid becomes 3 presets plus a **custom** layout. Custom
  keeps drag-and-resize but snaps to a 12 × 8 grid, and it can be saved under a name.
- Changed: the WebM export becomes MP4 (lossless cut, or composited).

## Risks
- **Software decode load**: 6 streams, or HW4 at 4×. Phase 0 measures this before anything else
  is built.
- **Firmware drift**: Tesla could change the SEI wrapper or add proto fields. Unknown fields are
  skipped by the decoder, and the oracle test catches wrapper changes.
- **No SEI**: older firmware (before 2025.44.25), HW2.5, or a parked car have no SEI. The vehicle
  strip shows "no vehicle data", the lanes stay empty, and playback is unaffected.
- **Encrypted clips**: they can't be played. They are listed with an "Encrypted" badge and a
  hint pointing to *Controls → Safety → Encrypt Dashcam Recordings*.
