# OpenTeslaCam

A native Tesla dashcam viewer. It plays all 6 cameras of an unencrypted TeslaCam clip in sync on one
timeline, and shows the vehicle data Tesla embeds in the H.264 stream: a vehicle strip, telemetry lanes
under the scrubber, and a GPS map. Written in Rust with egui. No FFmpeg or other system install needed.

## Install (Windows)

Run `OpenTeslaCam-Setup-<version>.exe`. It installs for your user only and needs no administrator rights.
It checks your system first, then offers what's missing before anything is installed:

- **Required:** only 64-bit Windows 10 or newer. The video decoder and vehicle-data parser are built in, and
  the exes link the C runtime statically, so there's no VC++ Redistributable to install.
- **Optional:** Tesla's [dashcam tools](https://github.com/teslamotors/dashcam), cloned from Tesla into
  `<install folder>\vendor-dev\dashcam`. The app doesn't need them to play clips. They need git; if it's
  missing, setup installs it first with winget (`Git.Git`), or links to the download if winget isn't there.

The same check works on its own and changes nothing:

```bash
powershell -ExecutionPolicy Bypass -File installer/detect-deps.ps1
```

Uninstall from Windows' installed apps. It removes only what setup installed, and leaves your clips, settings,
saved layouts and git.

To build the installer, close any running copy of the app first; the result is in `dist/`:

```bash
powershell -ExecutionPolicy Bypass -File scripts/build-installer.ps1
```

Unattended: `setup.exe --silent --dir <folder> [--desktop] [--tesla]`, and `uninstall.exe --uninstall --silent`.

## Build and run

You need Rust stable and a C++ compiler (MSVC Build Tools on Windows), because `openh264` compiles its
bundled source.

```bash
cargo run --release -- <TeslaCam folder>
```

With no argument, it reopens the last folder. You can also use `open folder` or drop a folder on the
window. Press `?` in the app for the keyboard shortcuts.

No clips at hand? This writes a small synthetic TeslaCam tree you can open instead:

```bash
cargo run --release -- --make-fixture target/fixture
```

Other flags:

| Flag | What it does |
|---|---|
| `--dump-sei <clip.mp4>` | Print the clip's telemetry as CSV |
| `--scan <folder>` | Print what the library scan finds |
| `--bench-decode <folder or clips>` | Measure decode speed on real clips, up to 6 at once |
| `--bench-synthetic` | The same on generated clips (indicative only) |

## Limits

- Encrypted clips can't be played. They are listed with a badge; turn encryption off under
  *Controls → Safety → Encrypt Dashcam Recordings*.
- Vehicle data needs firmware 2025.44.25 or later on HW3 or newer. Without it, or on a parked car, the
  strip shows "no vehicle data" and playback is unaffected.
- The map is the only network traffic: OpenStreetMap and Esri tiles, cached under the user's cache folder.

## Tests

```bash
cargo test
```

`tests/oracle.rs` diffs our `--dump-sei` output against Tesla's own `sei_extractor.py`. It prints a skip
message and passes if the tools or clips below are missing. Tesla's tools have no license, so they are
never committed or shipped; set them up locally in the git-ignored `vendor-dev/`:

```bash
git clone --depth 1 https://github.com/teslamotors/dashcam vendor-dev/dashcam
python -m venv vendor-dev/.venv
vendor-dev/.venv/Scripts/python -m pip install protobuf grpcio-tools
cd vendor-dev/dashcam && ../.venv/Scripts/python -m grpc_tools.protoc -I. --python_out=. dashcam.proto
```

On Linux and macOS the venv path is `.venv/bin/python`. To include real footage, copy clips into the
git-ignored `testdata/`; every `.mp4` under it gets diffed.

## Design

`PLAN.md` holds the architecture, design rules and deviations. `design/mockup/` is the GUI reference.
