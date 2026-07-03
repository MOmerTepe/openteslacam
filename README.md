# OpenTeslaCam

A local Tesla dashcam viewer. It plays all camera feeds at once and overlays the
vehicle data (speed, gear, pedals, steering, turn signals, autopilot state, GPS)
that Tesla embeds in the video files, plus a minimap.

> **Heads up, this is vibe-coded.** I built it for my own use and I'm putting it
> out there for anyone who needs it. It works for me; it's not polished software,
> there are no tests, and I make no guarantees. Use it, fork it, fix it.

> **Not hosted online.** Tesla's dashcam tools (required, see Setup) don't ship
> with a clear open-source license, so this isn't deployed anywhere. You run it
> yourself locally.

## ⚠️ Encrypted clips

Newer Tesla firmware **encrypts dashcam footage by default** (an `EncryptedClips`
folder). **This viewer cannot decrypt or play those clips**, they're not real MP4
files, just ciphertext, and only the vehicle that recorded them (or
`dashcam.tesla.com` signed into the linked account) can decrypt them.

If you want to use your footage here, turn encryption off in the car:
**Controls → Safety → Encrypt Dashcam Recordings** (off). Clips recorded after that
are normal MP4s and will work. Clips already encrypted stay unreadable to this app.

## Features

- All 6 cameras (front, back, left/right repeater, left/right pillar) in sync
- Draggable / resizable grid, plus two presets:
  - **Review** - left cameras left, right cameras right, front + rear in the middle
  - **Driving** - big front view with a rear + side-repeater rail
- Vehicle-data HUD from the embedded SEI metadata: speed, gear, brake, accelerator,
  steering wheel / autopilot state, blinking turn signals
- Minimap with GPS position + heading (street / satellite, north / follow)
- Clip browser (Saved / Sentry / Recent) with category filters
- Segment timeline for multi-part clips
- Trim & export a clip to WebM
- Keyboard shortcuts

## Setup

### 1. Get Tesla's dashcam tools (needed for the vehicle-data HUD & map)

The vehicle-data overlay is decoded with Tesla's own parsing library, which is
**not included in this repo** (it's their code). Without it the app still works —
clips play normally, you just get no HUD and no map. To get the full experience,
download it and drop it in:

1. Get it from **https://github.com/teslamotors/dashcam**
2. Put the contents in a `dashcam-master/` folder in the project root:

```
openteslacam/
├── index.html
├── viewer.js
├── viewer.css
└── dashcam-master/          ← Tesla's dashcam tools go here
    ├── dashcam-mp4.js
    ├── dashcam.proto
    └── vendor/
        └── protobuf.min.js
```

> SEI vehicle data only exists on clips from firmware **2025.44.25+** on **HW3 or
> newer**. Older clips still play, just without the data overlay.

### 2. Run it

- **Desktop app (Electron):** `npm install` then `npm start`
- **Browser:** run `python serve.py` and open the printed URL, or just open
  `index.html`. Needs a Chromium browser (Chrome/Edge) for the folder picker;
  drag-and-drop works elsewhere.
- **Packaged build (installer / portable):** if you're using a built release
  instead of running from source, put `dashcam-master/` inside the app's
  `resources/app/` folder (e.g.
  `C:\Users\you\AppData\Local\Programs\OpenTeslaCam\resources\app\dashcam-master\`).

## Usage

- Click **Open Folder** (or drag the folder in) and select your `TeslaCam` folder
  or a single clip folder.
- Expected layout:

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

### Shortcuts

| Key | Action |
|-----|--------|
| `Space` | Play / pause |
| `←` / `→` | Seek ±5s |
| `Shift + ←` / `→` | Seek ±30s |
| `Page Up` / `Page Down` | Previous / next segment |
| `Home` / `End` | Jump to start / end |
| `I` / `O` | Set trim in / out |

## Privacy

Everything runs locally. Your footage and vehicle data are never uploaded. Map
tiles are fetched from public tile servers (that's the only outbound traffic).

## Dependencies

- [Tesla Dashcam Tools](https://github.com/teslamotors/dashcam) — SEI decoding (see Setup)
- [Leaflet](https://leafletjs.com/) — maps
- [GridStack.js](https://gridstackjs.com/) — grid layout
- Electron (desktop build)

## License

MIT for the code in this repo. Tesla's dashcam tools in `dashcam-master/` are
theirs - get them from Tesla, they're not redistributed here.

---

Not affiliated with Tesla, Inc.
