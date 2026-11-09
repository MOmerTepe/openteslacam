# OpenTeslaCam 2

A local Tesla dashcam viewer. It plays all 6 cameras in sync on one timeline, so you can scrub through an
event and see every angle at the same moment, along with the vehicle data Tesla embeds in the video: speed,
gear, pedals, steering, blinkers, Autopilot state and GPS.

Version 2 is a full rewrite as a native desktop app in Rust. Everything runs on your machine, and there's
nothing else to set up.

> **Heads up.** I built this for my own use and I'm putting it out there for anyone who needs it. I make no
> guarantees. Use it, fork it, fix it.

## ⚠️ Encrypted clips

Newer Tesla firmware **encrypts dashcam footage by default** (an `EncryptedClips` folder). **This viewer can't
decrypt or play those clips.** They aren't real MP4 files, and only the car that recorded them, or
`dashcam.tesla.com` signed in to the linked account, can decrypt them. They show up in the library with a
`locked` badge.

To use your footage here, turn encryption off in the car: **Controls → Safety → Encrypt Dashcam Recordings**.
Clips recorded after that are normal MP4s. Clips already encrypted stay unreadable.

## What's new in 2.0

| | 1.x | 2.0 |
|---|---|---|
| App | Electron, or a browser | Native Rust app, one exe |
| Tesla's tools | Download them and drop them in by hand | Not needed: the app has its own parser, written from Tesla's published format |
| Camera sync | Side cameras corrected against the front | One clock drives all cameras |
| Layouts | Review, Driving, free-form grid | Grid, Center, Driving, and saved Custom layouts |
| Vehicle data | Overlay on the video | Strip below the video: nothing covers the footage |
| Timeline | Scrubber and segments | Adds speed, pedal, steering, blinker and Autopilot lanes, plus event and hard-brake markers |
| Export | One WebM | Lossless MP4 per camera, or one composited MP4, plus telemetry CSV |
| Clock | Time into the clip | Real time of day, with gaps in Recent clips kept |

**Upgrading from 1.x:** uninstall 1.x first. Both use the shortcut name `OpenTeslaCam`, and removing 1.x later
can take the 2.0 shortcut with it. The `dashcam-master/` folder is no longer used, and settings from 1.x don't
carry over.

## Requirements

| | |
|---|---|
| **Operating system** | Windows 10 or 11, 64-bit |
| **Graphics** | A driver with DirectX 12, Vulkan or OpenGL ES support |
| **Footage** | Unencrypted TeslaCam clips (see above) |
| **Vehicle data and map** | Clips from firmware 2025.44.25 or later on HW3 or newer. Older clips still play, and parked cars may record no vehicle data |
| **Internet** | Only for map tiles. Everything else works offline |

Nothing else needs installing. The video decoder and vehicle-data parser are built in, and there's no VC++
Redistributable, FFmpeg, Python or Node involved.

Video is decoded on the GPU through Windows' own H.264 decoder. Without a capable GPU it uses that decoder on the
CPU, and on editions without it (Windows N and some Server editions) a built-in software decoder. HW4 clips have a
much larger front camera: at 4× speed it keeps every frame while the other cameras show keyframes only, and on a
slower machine it may skip between keyframes at 2× too. To measure your machine, run
`openteslacam --bench-decode <folder>` on some clips. If video looks wrong, set the environment variable
`OPENTESLACAM_DECODER` to `software` to stay off the GPU, or `openh264` to use the built-in decoder.

macOS and Linux aren't packaged. The code isn't Windows-specific, but those builds are untested.

## Install

Run `OpenTeslaCam-Setup-2.0.0.exe`. It installs for your user only and doesn't need administrator rights.
Windows SmartScreen may warn you because the installer isn't code-signed yet.

Setup checks your system first and offers anything missing before it installs:

- **Tesla's dashcam tools (optional):** cloned from [Tesla's repo](https://github.com/teslamotors/dashcam) into
  `<install folder>\vendor-dev\dashcam`. The app doesn't use them to play clips; they're there for reference
  and for testing the parser.
- **Git (only for those tools):** if it's missing, setup installs it first with winget (`Git.Git`), or links to
  the download if winget isn't available.

The same check can run on its own, and it changes nothing:

```bash
powershell -ExecutionPolicy Bypass -File installer/detect-deps.ps1
```

To uninstall, use Windows' installed apps. It removes only what setup installed, and leaves your clips,
settings, saved layouts and Git.

Unattended install: `OpenTeslaCam-Setup-2.0.0.exe --silent --dir <folder> [--desktop] [--tesla]`. Unattended
uninstall: `uninstall.exe --uninstall --silent`.

## Usage

Choose `open folder`, or drop a folder on the window, and pick your `TeslaCam` folder or a single clip folder.
The last folder reopens next time.

```
TeslaCam/
├── SavedClips/
│   └── 2024-01-15_12-30-00/
│       ├── 2024-01-15_12-30-00-front.mp4
│       ├── ...-back.mp4  ...-left_repeater.mp4  ...-right_repeater.mp4
│       ├── ...-left_pillar.mp4  ...-right_pillar.mp4
│       ├── event.json
│       └── thumb.png
├── SentryClips/
└── RecentClips/
```

### Layouts

- **Grid:** all six cameras at equal size.
- **Center:** front large in the middle with back below it, and pillar and repeater on each side.
- **Driving:** front as large as possible, with back and both repeaters on the right.
- **Custom:** drag and resize cameras on a snapping grid, hide the ones you don't need, and save layouts by name.

Double-click a camera to focus it. The library panel closes to give the cameras its width.

### Shortcuts

| Key | Action |
|---|---|
| `Space` | Play / pause |
| `←` / `→` | Seek ±5 s |
| `Shift + ←` / `→` | Seek ±30 s |
| `,` / `.` | Step one frame (while paused) |
| `Page Up` / `Page Down`, or `[` / `]` | Previous / next segment |
| `Home` / `End` | Jump to start / end |
| `I` / `O` | Set trim in / out |
| `Esc` | Close a panel, or leave focus |
| `?` | Show all shortcuts |

### Command line

| Flag | What it does |
|---|---|
| `--version` | Print the version |
| `--dump-sei <clip.mp4>` | Print the clip's vehicle data as CSV |
| `--scan <folder>` | Print what the library scan finds |
| `--bench-decode <folder or clips>` | Measure decode speed on real clips, up to 6 at once |
| `--bench-synthetic` | The same on generated clips (indicative only) |

## Privacy

Everything runs locally, and your footage and vehicle data are never uploaded. Map tiles come from OpenStreetMap
and Esri and are cached on your machine. That's the app's only network traffic. Setup goes online only if you
choose to fetch Tesla's tools or install Git.

## Build from source

You need Rust stable and a C++ compiler (MSVC Build Tools on Windows), because `openh264` compiles its bundled
source.

```bash
cargo run --release -- <TeslaCam folder>
```

No clips at hand? This writes a small synthetic TeslaCam tree you can open instead:

```bash
cargo run --release -- --make-fixture target/fixture
```

To build the installer, close any running copy of the app first. The result is in `dist/`:

```bash
powershell -ExecutionPolicy Bypass -File scripts/build-installer.ps1
```

The version lives in one place, `[workspace.package]` in `Cargo.toml`.

## Tests

```bash
cargo test --workspace
```

`tests/oracle.rs` diffs our `--dump-sei` output against Tesla's own `sei_extractor.py`. It prints a skip message
and passes if the tools or clips below are missing. Tesla's tools have no clear license, so they are never
committed or shipped; set them up locally in the git-ignored `vendor-dev/`:

```bash
git clone --depth 1 https://github.com/teslamotors/dashcam vendor-dev/dashcam
python -m venv vendor-dev/.venv
vendor-dev/.venv/Scripts/python -m pip install protobuf grpcio-tools
cd vendor-dev/dashcam && ../.venv/Scripts/python -m grpc_tools.protoc -I. --python_out=. dashcam.proto
```

On Linux and macOS the venv path is `.venv/bin/python`. To include real footage, copy clips into the git-ignored
`testdata/`; every `.mp4` under it gets diffed.

`PLAN.md` holds the architecture, design rules and deviations, and `design/mockup/` is the GUI reference.

## License

MIT for the code in this repo; see [LICENSE.md](LICENSE.md). IBM Plex fonts are under the SIL Open Font License
([assets/fonts/OFL.txt](assets/fonts/OFL.txt)). Tesla's tools are Tesla's: they aren't redistributed here.

---

Not affiliated with Tesla, Inc.
