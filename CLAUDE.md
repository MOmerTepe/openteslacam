# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

OpenTeslaCam 2: a native Rust (egui/eframe) Tesla dashcam viewer for Windows. It plays 6 cameras in sync on one
wall-clock timeline, with the vehicle data Tesla embeds in the H.264 stream. Workspace of two crates: the app
(root) and `installer/` (`openteslacam-setup`).

## Commands

```bash
cargo run --release -- <TeslaCam folder>              # run the viewer
cargo run --release -- --make-fixture target/fixture   # write a synthetic TeslaCam tree to open (hidden flag)
cargo test --workspace                                 # all tests
cargo test <name>                                      # one unit test (tests live in each module)
cargo test --test oracle                               # SEI parser vs Tesla's sei_extractor.py
powershell -ExecutionPolicy Bypass -File scripts/build-installer.ps1   # dist/OpenTeslaCam-Setup-<version>.exe
```

- Other CLI flags (`--dump-sei`, `--scan`, `--bench-decode`, `--bench-synthetic`) are in `USAGE` in `src/main.rs`.
- `tests/oracle.rs` skips (and passes) unless `vendor-dev/dashcam` and its venv exist; it also diffs every `.mp4`
  under `testdata/`. Setup steps are in the README. Both folders are git-ignored: Tesla's tools have no clear
  license and must never be committed or shipped.
- Dev builds compile dependencies at `opt-level = 3` (openh264 is unusable unoptimized). Building needs a C++
  compiler, since openh264 builds its bundled source.
- The version lives only in `[workspace.package]` in `Cargo.toml`; the installer script reads it from there.
- Close any running copy of the app before building the installer.
- There is no rustfmt/clippy config. CI (`.github/workflows/test.yml`) runs `cargo test --workspace` on Windows.

## Architecture

Hand-written on purpose, to keep dependencies few and avoid FFmpeg: the MP4 demuxer (`mp4.rs`), MP4 muxer
(`mux.rs`), SEI parser and protobuf decoder (`sei.rs`). Tesla clips are plain non-fragmented single-track MP4s, and
one sample table serves both decoding and telemetry. Don't add `prost`, `image`, serde derive or similar for what
these already do.

**Data flow.** `library.rs` scans a folder into events (cheap: directory entries, `event.json`, one MP4 header per
event; encrypted clips are flagged, not failed). `player.rs` owns the single master clock: one position on the
event's wall-clock timeline (segment start from the filename, length from `stts`), mapped to (segment, local
seconds). Each camera has a decoder thread (`decode.rs`) that follows that clock and returns the latest frame with
`pts ≤ t`, so all views show the same instant by construction; nothing is slaved to the front camera. Gaps in
RecentClips stay as gaps on the timeline. `telemetry.rs` builds one event's vehicle data off the UI thread: lookup
at a time plus the precomputed timeline lanes.

**Decoders.** `decode.rs` picks per clip: Media Foundation on the GPU (`mf.rs`, D3D11) → Media Foundation on the
CPU → openh264. openh264 is also used for clips with B-frames and clips the MF decoder reports as damaged.
`OPENTESLACAM_DECODER=software|openh264` forces a lower tier. `mfplat.dll`/`d3d11.dll` are delay-loaded
(`build.rs`) because Windows N lacks them; `mf.rs` must check before touching them. Decoder modes: exact,
keyframes-only (scrubbing), and `Play` (a lagging camera shows its newest frame, then jumps to a keyframe). At 4×
on HW4 clips only the focused camera decodes in full (`cam_mode` in `src/ui/mod.rs`).

**Export** (`export.rs`, dialog in `ui/export.rs`). Lossless copies H.264 samples per camera from the keyframe at
or before In, starting a new file at a gap or a changed avcC/timescale/size. Composited decodes with openh264,
paints cameras plus the vehicle band, and encodes on the GPU (`MfEncoder`) or openh264. Output is written as
`.part` and renamed on success.

**Library on this PC** (`import.rs`, `manage.rs`, dialogs in `ui/`). Import copies a drive's SavedClips,
SentryClips and RecentClips into `settings.library_dir` in Tesla's layout, so `library::scan` reads it unchanged;
a file counts only after its copy was read back and its SHA-256 matched, and only then may the drive's copy be
deleted. Manage acts on library events only (never on a drive): trim drops whole segments (cutting inside one
would start each camera at its own keyframe), removed files go to the Recycle Bin. Marks live in settings as
wall-clock seconds keyed by event folder name, so they survive import and trimming.

**Update** (`update.rs`, dialog in `ui/update.rs`). One GitHub request at startup. An installed copy (one with
`uninstall.exe` beside it) downloads the release's setup, checks size and SHA-256, renames its own running exe
aside, runs `setup --silent --dir <its folder>` and starts the new exe. Things that must not run in tests
(update check, drive watch, welcome) start in `ui::run`, not `App::new`.

**UI** (`src/ui/`). `mod.rs` holds app state and the per-frame panel order; the other files are one region each.
Layouts are cell lists on a 12 × 8 grid (presets use fractional units, only custom cells snap). `settings.rs`
persists one JSON string through eframe storage where every field falls back on its own, so an older settings file
never loses the rest.

**Installer.** `installer/src/main.rs` includes the app's theme with `#[path = "../../src/ui/theme.rs"]`, so
`theme.rs` must compile standalone in both crates. The release setup exe is the stub with the app appended:
stub, app, app length (u64 LE), magic `OTCSETUP`. `split_payload` in the installer and
`scripts/build-installer.ps1` must agree on that layout. The bare stub is installed as `uninstall.exe`.

**Windows specifics.** Static C runtime (`.cargo/config.toml`), so no VC++ Redistributable. Release builds use the
windows subsystem; CLI flags attach to the parent console (`attach_console`), so debug and release differ there.

## Design rules (UI changes must hold these)

1. Nothing is drawn over the video. Captions sit in a strip above each tile; only modal dialogs may cover video.
2. Monochrome, hard edges: zero rounding, IBM Plex Mono for labels/controls/numbers, Plex Sans for prose. Colors
   and fonts come from `ui/theme.rs`. The only colors are its `RED`, `YELLOW` and `GREEN` (the macOS window
   buttons), used sparingly, always as a fill and only where the color itself says something: red removes
   footage or brakes, yellow blinks, green is a step forward (an update). Never for decoration or plain text.
3. All controls in a row are the same size. The primary action is filled, never larger; an active toggle is
   underlined. Widgets are painted into exact rects handed out by `theme::Row`, not egui's stock widgets.
4. Each signal is drawn in the form that fits its data (area for speed, blocks for brake, line for steering,
   labeled bands for Autopilot). Lit vs. unlit is brightness; color only where rule 2 allows it (brake, blinkers).

Sign conventions (measured on one HW4 car, Nov 2026): `steering_deg` > 0 is right; `accel_y` is longitudinal and
negative when speeding up, so forward g is `-accel_y` and braking is negative g; `accel_x` is lateral (unused).

## Versions and release notes

**Version numbers are `x.y.z`.**

- `x`: a major revision or remake (2.0 was the Rust rewrite). Settings or layouts may not carry over.
- `y`: a feature update. It may include bug fixes. `z` goes back to 0.
- `z`: bug fixes and hotfixes only. No features, no new settings, no new dialogs, no behaviour a user has to learn.
  A fix that needs a feature to be done properly waits for the next `y`, or ships as the smallest fix alone.
- If unsure between `y` and `z`: would the README's feature list change? Then it is `y`.
- The version lives only in `[workspace.package]` in `Cargo.toml`. The tag is `vx.y.z`, the commit subject is
  `Release x.y.z: <what changed, in a few words>`, and the setup is `OpenTeslaCam-Setup-x.y.z.exe`.

**Release notes** (the GitHub release body, and the matching `What's new in x.y.z` section in the README):

1. Title `OpenTeslaCam x.y.z`. Add `(urgent fix)` only when the release fixes something that removes or damages
   footage, loses settings, or stops the app from starting or updating.
2. Start with one sentence saying what kind of release it is. An urgent one starts with a bold line telling
   people to update and which action to avoid until they have.
3. Sections, in this order, leaving out the empty ones: `What's new` (features, `y` and `x` only), `Fixed`,
   `If you were affected` (whenever a bug touched footage or settings: say whether anything is lost and how to
   get it back), `Updating`.
4. One bullet per change: a short bold name, then what the user sees, in plain words. Name buttons and keys as
   they appear on screen, in backticks. No file, function or module names.
5. For a fix, say what went wrong, when it happened and what happens now. Don't write only "fixed a bug".
6. Say what is not known: a thing measured on one car, a path not tried on a real install.
7. The app shows these notes as plain text with the Markdown marks stripped, so use only `##` headings, `-`
   bullets, bold and backticks. No tables, images, nested lists or `[text](url)` links; write a URL out.
8. The notes and the README section say the same thing. Past sections of the README are never rewritten.

**Releasing:** tests pass, then `scripts/build-installer.ps1`, then commit, push, tag, and a GitHub release with
the setup attached. The in-app updater reads GitHub's latest release, so a release is what users are offered.

`PLAN.md` (git-ignored, local only) has the full plan, design table and recorded deviations, if present.
