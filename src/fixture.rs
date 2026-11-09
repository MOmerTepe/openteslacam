//! Synthetic clips: the openh264 encode + mux helper (also used by the bench) and the
//! `--make-fixture` TeslaCam tree for testing the viewer without real footage.

use crate::library::{CAMERAS, fmt_stamp, stamp};
use crate::mux::Muxer;
use crate::sei::{self, AP_AUTOSTEER, AP_FSD, AP_NONE, AP_TACC, GEAR_DRIVE, GEAR_PARK, Telemetry};
use eframe::egui::IconData;
use eframe::icon_data::IconDataExt;
use openh264::OpenH264API;
use openh264::encoder::{BitRate, EncodedBitStream, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode};
use openh264::formats::YUVBuffer;
use std::fs;
use std::path::Path;
use std::thread;

pub const FPS: u32 = 36;

/// xorshift64: deterministic noise without a rand dependency.
pub fn rng(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// `bps` at about `FPS`, a keyframe every second.
pub fn new_encoder(bps: u32) -> Result<Encoder, openh264::Error> {
    let cfg = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bps))
        .max_frame_rate(FrameRate::from_hz(FPS as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .skip_frames(false)
        .intra_frame_period(IntraFramePeriod::from_num_frames(FPS));
    Encoder::with_api_config(OpenH264API::from_source(), cfg)
}

/// Appends one encoded picture to `sample` as AVCC; the parameter sets go to `sps` and `pps` instead.
pub fn take_nals(bs: &EncodedBitStream, sample: &mut Vec<u8>, sps: &mut Vec<u8>, pps: &mut Vec<u8>) {
    for layer in (0..bs.num_layers()).filter_map(|l| bs.layer(l)) {
        for nal in (0..layer.nal_count()).filter_map(|n| layer.nal_unit(n)) {
            // Annex-B start code (00 00 01 or 00 00 00 01) off.
            put_nal(&nal[nal.iter().position(|&b| b != 0).unwrap_or(0) + 1..], sample, sps, pps);
        }
    }
}

/// Appends one NAL (no start code) to `sample` as AVCC; parameter sets go to avcC instead.
pub fn put_nal(nal: &[u8], sample: &mut Vec<u8>, sps: &mut Vec<u8>, pps: &mut Vec<u8>) {
    match nal.first().map(|b| b & 0x1F) {
        Some(7) => *sps = nal.to_vec(),
        Some(8) => *pps = nal.to_vec(),
        _ => {
            sample.extend((nal.len() as u32).to_be_bytes());
            sample.extend(nal);
        }
    }
}

/// The NALs of an Annex-B access unit, without start codes or trailing zeros.
pub fn annex_b_nals(data: &[u8]) -> Vec<&[u8]> {
    // Emulation prevention keeps 00 00 01 out of a NAL's payload.
    let starts: Vec<usize> = data.windows(3).enumerate().filter(|(_, w)| *w == [0, 0, 1]).map(|(i, _)| i + 3).collect();
    let ends = starts.iter().skip(1).map(|s| s - 3).chain([data.len()]);
    let trim = |n: &'_ [u8]| -> usize { n.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1) };
    starts.iter().zip(ends).map(|(&s, e)| &data[s..e.max(s)]).map(|n| &n[..trim(n)]).filter(|n| !n.is_empty()).collect()
}

/// The avcC payload for one SPS (at least 4 bytes) and one PPS.
pub fn avcc_of(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
    avcc.extend((sps.len() as u16).to_be_bytes());
    avcc.extend(sps);
    avcc.push(1);
    avcc.extend((pps.len() as u16).to_be_bytes());
    avcc.extend(pps);
    avcc
}

/// Encodes `frames` pictures at `FPS` (a keyframe every second) and muxes them to `path`.
/// `frame(t, yuv)` appends picture `t` as I420 to the empty `yuv` and may return one extra NAL
/// (e.g. SEI) to put first in that sample.
pub fn encode_clip(
    path: &Path,
    w: usize,
    h: usize,
    bps: u32,
    frames: usize,
    mut frame: impl FnMut(usize, &mut Vec<u8>) -> Option<Vec<u8>>,
) -> Result<(), String> {
    let enc_err = |e: openh264::Error| format!("encoder {w}x{h}: {e}");
    let mut enc = new_encoder(bps).map_err(enc_err)?;
    let (mut sps, mut pps) = (Vec::new(), Vec::new());
    let mut samples = Vec::with_capacity(frames);
    let mut yuv = Vec::with_capacity(w * h * 3 / 2);
    for t in 0..frames {
        yuv.clear();
        let extra = frame(t, &mut yuv);
        let bs = enc.encode(&YUVBuffer::from_vec(yuv.clone(), w, h)).map_err(enc_err)?;
        let key = bs.frame_type() == FrameType::IDR;
        let mut sample = Vec::new();
        for nal in extra.iter() {
            sample.extend((nal.len() as u32).to_be_bytes());
            sample.extend(nal);
        }
        take_nals(&bs, &mut sample, &mut sps, &mut pps);
        if sample.len() == extra.map_or(0, |n| n.len() + 4) {
            return Err(format!("encoder {w}x{h} produced no picture for frame {t}"));
        }
        samples.push((sample, key));
    }
    if sps.len() < 4 || pps.is_empty() {
        return Err(format!("encoder {w}x{h} produced no SPS/PPS"));
    }
    let avcc = avcc_of(&sps, &pps);
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut m = Muxer::create(path, FPS * 1000, w as u32, h as u32, &avcc).map_err(io)?;
    for (data, key) in &samples {
        m.push(data, 1000, 0, *key).map_err(io)?;
    }
    m.finish().map_err(io)
}

/// 3x5 pixel glyphs, one row per byte (bit 2 = left column).
fn glyph(c: char) -> [u8; 5] {
    match c {
        '0' => [7, 5, 5, 5, 7],
        '1' => [2, 6, 2, 2, 7],
        '2' => [7, 1, 7, 4, 7],
        '3' => [7, 1, 7, 1, 7],
        '4' => [5, 5, 7, 1, 1],
        '5' => [7, 4, 7, 1, 7],
        '6' => [7, 4, 7, 5, 7],
        '7' => [7, 1, 1, 1, 1],
        '8' => [7, 5, 7, 5, 7],
        '9' => [7, 5, 7, 1, 7],
        'A' => [2, 5, 7, 5, 5],
        'B' => [6, 5, 6, 5, 6],
        'C' => [3, 4, 4, 4, 3],
        'E' => [7, 4, 6, 4, 7],
        'F' => [7, 4, 6, 4, 4],
        'G' => [3, 4, 5, 5, 3],
        'H' => [5, 5, 7, 5, 5],
        'I' => [7, 2, 2, 2, 7],
        'K' => [5, 5, 6, 5, 5],
        'L' => [4, 4, 4, 4, 7],
        'N' => [6, 5, 5, 5, 5],
        'O' => [2, 5, 5, 5, 2],
        'P' => [6, 5, 6, 4, 4],
        'R' => [6, 5, 6, 5, 5],
        'S' => [3, 4, 2, 1, 6],
        'T' => [7, 2, 2, 2, 2],
        '_' => [0, 0, 0, 0, 7],
        _ => [0; 5],
    }
}

/// Fills a rectangle of the luma plane, clipped to the picture.
fn rect(y: &mut [u8], w: usize, (x0, y0, rw, rh): (usize, usize, usize, usize), v: u8) {
    let h = y.len() / w;
    for row in y0.min(h)..(y0 + rh).min(h) {
        y[row * w + x0.min(w)..row * w + (x0 + rw).min(w)].fill(v);
    }
}

fn text(y: &mut [u8], w: usize, s: &str, x0: usize, y0: usize, px: usize) {
    for (i, c) in s.chars().enumerate() {
        for (r, bits) in glyph(c).iter().enumerate() {
            for col in 0..3 {
                if bits >> (2 - col) & 1 == 1 {
                    rect(y, w, (x0 + (i * 4 + col) * px, y0 + r * px, px, px), 235);
                }
            }
        }
    }
}

/// One picture of camera `cam` as I420: the camera's name, the segment and frame number (equal in
/// all six views at the same instant, so sync can be checked by eye), a box sweeping
/// left to right once a second, and a per-camera colour tint.
fn paint(cam: usize, seg: usize, t: usize, w: usize, h: usize, out: &mut Vec<u8>) {
    let base = out.len();
    out.extend((0..w * h).map(|i| (24 + (i % w + i / w) * 40 / (w + h) + 8 * cam) as u8));
    let y = &mut out[base..];
    let px = (w / 64).max(1);
    text(y, w, &CAMERAS[cam].to_ascii_uppercase(), px * 2, px * 2, px);
    text(y, w, &format!("S{seg} F{t:04}"), px * 2, px * 9, px);
    let bw = w / 8;
    let x = (t % FPS as usize) * (w - bw) / (FPS as usize - 1);
    rect(y, w, (x, h - bw - px * 2, bw, bw), 200);
    // Grey for front, then five distinct tints.
    let (u, v) = [(128, 128), (100, 160), (160, 100), (90, 110), (170, 150), (128, 80)][cam];
    out.extend(std::iter::repeat_n(u, w * h / 4));
    out.extend(std::iter::repeat_n(v, w * h / 4));
}

/// A plausible drive of `n` frames: pull away, cruise with TACC → Autosteer → FSD bands and a
/// lane change (right blinker), a hard brake to a stop, a hazard stretch, then a left turn.
/// GPS integrates speed and heading from (lat, lon, heading).
fn drive(n: usize, (mut lat, mut lon, mut heading): (f64, f64, f64)) -> Vec<Telemetry> {
    let dt = 1.0 / FPS as f64;
    let mut v: f64 = 0.0;
    let mut braking = false;
    (0..n)
        .map(|i| {
            let t = i as f64 * dt;
            let u = i as f64 / n as f64;
            let flash = (t * 1.5).fract() < 0.5;
            let mut s = Telemetry { version: 1, frame_seq_no: 40_000 + i as u64, gear: GEAR_DRIVE, ..Default::default() };
            let mut steer = 3.0 * (t * 0.7).sin();
            let a = if u < 0.08 {
                s.gear = if u < 0.04 { GEAR_PARK } else { GEAR_DRIVE };
                s.brake = true;
                0.0
            } else if u < 0.30 {
                s.accel_pct = 45.0;
                if v < 15.0 { 2.5 } else { 0.0 }
            } else if u < 0.62 {
                s.autopilot = match u {
                    u if u < 0.36 => AP_NONE,
                    u if u < 0.44 => AP_TACC,
                    u if u < 0.53 => AP_AUTOSTEER,
                    u if u < 0.60 => AP_FSD,
                    _ => AP_NONE,
                };
                if s.autopilot == AP_NONE {
                    s.accel_pct = 18.0;
                }
                if (0.46..0.52).contains(&u) {
                    s.blink_right = flash;
                    steer = -10.0 * ((u - 0.46) / 0.06 * std::f64::consts::TAU).sin();
                }
                0.5 * (15.0 + (t * 0.4).sin() - v)
            } else if u < 0.80 {
                braking |= v > 0.0;
                s.brake = true;
                if u >= 0.68 {
                    s.blink_left = flash;
                    s.blink_right = flash;
                }
                if braking && v > 0.0 { -8.0 } else { 0.0 }
            } else {
                s.accel_pct = 30.0;
                if u < 0.88 {
                    s.blink_left = flash;
                    steer = 220.0 * ((u - 0.80) / 0.08 * std::f64::consts::PI).sin();
                }
                if v < 8.0 { 1.5 } else { 0.0 }
            };
            v = (v + a * dt).max(0.0);
            // Bicycle model: 15:1 steering ratio, 2.9 m wheelbase; positive steering turns left.
            let yaw = v * (steer.to_radians() / 15.0).tan() / 2.9;
            heading = (heading - yaw.to_degrees() * dt).rem_euclid(360.0);
            lat += v * heading.to_radians().cos() * dt / 111_320.0;
            lon += v * heading.to_radians().sin() * dt / (111_320.0 * lat.to_radians().cos());
            s.speed_mps = v as f32;
            s.steering_deg = steer as f32;
            (s.lat, s.lon, s.heading) = (lat, lon, heading);
            (s.accel_x, s.accel_y, s.accel_z) = (if v > 0.0 || a > 0.0 { a } else { 0.0 }, v * yaw, 0.15 * (t * 9.0).sin());
            s
        })
        .collect()
}

/// Writes one segment's six clips in parallel; the front gets SEI from `telemetry` if given.
fn segment(dir: &Path, start: i64, seg: usize, size: (usize, usize, usize), telemetry: Option<&[Telemetry]>) -> Result<(), String> {
    let (w, h, frames) = size;
    thread::scope(|sc| {
        let jobs: Vec<_> = (0..6)
            .map(|cam| {
                let path = dir.join(format!("{}-{}.mp4", fmt_stamp(start), CAMERAS[cam]));
                sc.spawn(move || {
                    let sei = telemetry.filter(|_| cam == 0);
                    encode_clip(&path, w, h, (w * h * 2) as u32, frames, |t, yuv| {
                        paint(cam, seg, t, w, h, yuv);
                        sei.and_then(|s| s.get(t)).map(|s| sei::sei_nal(&sei::encode_metadata(s)))
                    })
                })
            })
            .collect();
        jobs.into_iter().try_for_each(|j| j.join().map_err(|_| "encoder thread panicked".to_string())?)
    })
}

fn event_json(dir: &Path, at: i64, reason: &str, (lat, lon): (f64, f64), camera: &str) -> Result<(), String> {
    let ts = fmt_stamp(at).replacen('_', "T", 1);
    let ts = format!("{}:{}:{}", &ts[..13], &ts[14..16], &ts[17..]);
    let json = serde_json::json!({
        "timestamp": ts, "city": "Berlin", "est_lat": format!("{lat:.4}"), "est_lon": format!("{lon:.4}"),
        "reason": reason, "camera": camera,
    });
    fs::write(dir.join("event.json"), json.to_string()).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Grey quarter-size thumbnail of the front camera's first picture.
fn thumb(dir: &Path, w: usize, h: usize) -> Result<(), String> {
    let mut yuv = Vec::new();
    paint(0, 0, 0, w, h, &mut yuv);
    let (tw, th) = (w / 4, h / 4);
    let rgba = (0..tw * th).flat_map(|i| {
        let l = yuv[(i / tw * 4) * w + i % tw * 4];
        [l, l, l, 255]
    });
    let png = IconData { rgba: rgba.collect(), width: tw as u32, height: th as u32 }.to_png_bytes()?;
    fs::write(dir.join("thumb.png"), png).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Writes a TeslaCam tree under `root`: SavedClips with a 2-segment drive (SEI, event.json,
/// thumb), SentryClips with one parked segment (no SEI), RecentClips with 3 segments and a 45 s
/// gap before the last, and one encrypted (random bytes) SavedClips event.
pub fn make_fixture(root: &Path, secs: u32, w: usize, h: usize) -> Result<(), String> {
    let mkdir = |p: &Path| fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()));
    let frames = (secs * FPS) as usize;
    let size = (w, h, frames);
    let secs = secs as i64;
    let berlin = (52.5163, 13.3777);

    let t0 = stamp("2026-03-14_15-30-00").expect("valid stamp");
    let saved = root.join("SavedClips").join(fmt_stamp(t0 + 2 * secs - 2));
    mkdir(&saved)?;
    let tel = drive(2 * frames, (berlin.0, berlin.1, 80.0));
    for seg in 0..2 {
        segment(&saved, t0 + seg as i64 * secs, seg, size, Some(&tel[seg * frames..]))?;
    }
    event_json(&saved, t0 + 2 * secs - 2, "user_interaction_dashcam_launcher_action_tapped", berlin, "0")?;
    thumb(&saved, w, h)?;

    let s0 = stamp("2026-03-14_22-10-00").expect("valid stamp");
    let sentry = root.join("SentryClips").join(fmt_stamp(s0 + secs / 2));
    mkdir(&sentry)?;
    segment(&sentry, s0, 0, size, None)?;
    event_json(&sentry, s0 + secs / 2, "sentry_aware_object_detection", berlin, "5")?;
    thumb(&sentry, w, h)?;

    let r0 = stamp("2026-03-14_14-00-00").expect("valid stamp");
    let recent = root.join("RecentClips");
    mkdir(&recent)?;
    let tel = drive(3 * frames, (52.5219, 13.4132, 200.0));
    for (seg, start) in [r0, r0 + secs, r0 + 2 * secs + 45].into_iter().enumerate() {
        segment(&recent, start, seg, size, Some(&tel[seg * frames..]))?;
    }

    let e0 = stamp("2026-03-13_09-00-00").expect("valid stamp");
    let enc = root.join("SavedClips").join(fmt_stamp(e0));
    mkdir(&enc)?;
    let mut s = 0x1234_5678_9ABC_DEF1;
    let noise: Vec<u8> = (0..64 * 1024).map(|_| rng(&mut s) as u8).collect();
    let p = enc.join(format!("{}-front.mp4", fmt_stamp(e0 - 30)));
    fs::write(&p, noise).map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Kind, scan};
    use crate::player::Player;

    #[test]
    fn fixture_scans_and_plays() {
        let root = std::env::temp_dir().join(format!("otc-fixture-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        make_fixture(&root, 1, 128, 96).unwrap();
        let ev = scan(&root).unwrap();
        let _ = fs::remove_dir_all(&root);
        let got: Vec<_> = ev.iter().map(|e| (e.kind, e.name.as_str(), e.segments.len(), e.encrypted)).collect();
        assert_eq!(
            got,
            [
                (Kind::Sentry, "2026-03-14_22-10-00", 1, false),
                (Kind::Saved, "2026-03-14_15-30-00", 2, false),
                (Kind::Recent, "RecentClips", 3, false),
                (Kind::Saved, "2026-03-13_09-00-00", 1, true),
            ]
        );
        // Sentry's folder is named after its trigger (start + secs/2, rounded down to 0 here).
        let saved = &ev[1];
        assert!(saved.segments.iter().all(|s| s.files.iter().all(Option::is_some)));
        let info = saved.info.as_ref().unwrap();
        assert_eq!((info.city.as_str(), info.est_lat, info.timestamp), ("Berlin", Some(52.5163), stamp("2026-03-14_15-30-00")));
        assert!(saved.thumb.is_some() && ev[0].thumb.is_some() && ev[2].thumb.is_none());
    }

    #[test]
    fn fixture_content() {
        let root = std::env::temp_dir().join(format!("otc-fixture-c-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        make_fixture(&root, 2, 128, 96).unwrap();
        let ev = scan(&root).unwrap();
        let (saved, recent) = (&ev[1], &ev[2]);
        let p = Player::open(saved);
        let starts: Vec<_> = p.segs.iter().map(|s| (s.start, s.dur, s.frames.len())).collect();
        assert_eq!(starts, [(0.0, 2.0, 72), (2.0, 2.0, 72)]);
        let r = Player::open(recent);
        assert_eq!(r.segs.iter().map(|s| s.start).collect::<Vec<_>>(), [0.0, 2.0, 49.0]);
        assert_eq!(r.locate(10.0), None, "the gap is kept");

        let (_, frames) = sei::read_telemetry(saved.segments[0].files[0].as_ref().unwrap()).unwrap();
        assert_eq!(frames.len(), 72);
        assert!((frames[0].t.lat - 52.5163).abs() < 1e-3 && frames[0].t.gear == GEAR_PARK);
        let (_, none) = sei::read_telemetry(saved.segments[0].files[1].as_ref().unwrap()).unwrap();
        assert!(none.is_empty(), "only the front camera carries SEI");
        let (_, sentry) = sei::read_telemetry(ev[0].segments[0].files[0].as_ref().unwrap()).unwrap();
        assert!(sentry.is_empty());
        let png = fs::read(saved.thumb.as_ref().unwrap()).unwrap();
        let img = eframe::icon_data::from_png_bytes(&png).unwrap();
        assert_eq!((img.width, img.height), (32, 24));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn drive_covers_every_signal() {
        let d = drive(40 * FPS as usize, (52.5163, 13.3777, 80.0));
        let any = |f: &dyn Fn(&Telemetry) -> bool| d.iter().any(f);
        assert!(any(&|t| t.blink_left && t.blink_right), "hazards");
        assert!(any(&|t| t.blink_right && !t.blink_left) && any(&|t| t.blink_left && !t.blink_right));
        for ap in [AP_TACC, AP_AUTOSTEER, AP_FSD] {
            assert!(any(&|t| t.autopilot == ap));
        }
        assert!(any(&|t| t.accel_x < -7.0 && t.brake), "hard brake");
        assert!(any(&|t| t.steering_deg > 200.0));
        let max = d.iter().map(|t| t.speed_mps).fold(0.0, f32::max);
        assert!((14.0..16.0).contains(&max));
        // Moved a few hundred metres and stayed in Berlin.
        let last = d.last().unwrap();
        let moved = ((last.lat - 52.5163) * 111_320.0).hypot((last.lon - 13.3777) * 67_800.0);
        assert!((200.0..1500.0).contains(&moved), "{moved}");
    }
}
