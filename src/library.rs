//! Scans a TeslaCam folder into events. Only directory entries, event.json and normally one MP4
//! header per event are read, so hundreds of events scan quickly; segment durations come later
//! (`Player::open`).

use crate::mp4::{self, Mp4Error};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Index order of `Segment::files` everywhere in the app.
pub const CAMERAS: [&str; 6] = ["front", "back", "left_repeater", "right_repeater", "left_pillar", "right_pillar"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Saved,
    Sentry,
    Recent,
    /// A folder of clips that isn't inside SavedClips/SentryClips.
    Folder,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Wall-clock start from the filename, in `Stamp` seconds.
    pub start: i64,
    /// Indexed like `CAMERAS`.
    pub files: [Option<PathBuf>; 6],
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EventInfo {
    pub timestamp: Option<i64>,
    pub reason: String,
    pub city: String,
    pub street: String,
    pub est_lat: Option<f64>,
    pub est_lon: Option<f64>,
    pub camera: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: Kind,
    pub name: String,
    pub dir: PathBuf,
    /// Sorted by start; never empty.
    pub segments: Vec<Segment>,
    pub info: Option<EventInfo>,
    pub thumb: Option<PathBuf>,
    pub encrypted: bool,
}

impl Event {
    /// For the library row, before the tracks are read.
    // shortcut: assumes Tesla's ~60 s segments and ignores gaps, exact once the event is opened.
    pub fn approx_duration(&self) -> i64 {
        self.segments.last().map_or(0, |l| l.start - self.segments[0].start + 60)
    }

    /// What the library lists and sorts it by: the trigger, else the folder name's time (as in the
    /// legacy app), else the newest footage. RecentClips has no single moment, so it's the last.
    pub fn time(&self) -> i64 {
        let named = || if self.kind == Kind::Recent { None } else { stamp(&self.name) };
        self.info.as_ref().and_then(|i| i.timestamp).or_else(named).unwrap_or_else(|| self.segments.last().map_or(0, |s| s.start))
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's days_from_civil).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of `stamp`: (year, month, day, hour, minute, second).
pub fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let (days, s) = (secs.div_euclid(86400), secs.rem_euclid(86400) as u32);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y, m, d, s / 3600, s / 60 % 60, s % 60)
}

/// `YYYY-MM-DD_HH-MM-SS`, as in Tesla's file and folder names.
pub fn fmt_stamp(secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}")
}

/// Parses `YYYY-MM-DD_HH-MM-SS` (filenames) or `YYYY-MM-DDTHH:MM:SS` (event.json) at the start of
/// `s`. The car's local time is kept as is, counted as if it were UTC: the timeline shows the
/// time of day the car showed, and no time zone is needed.
pub fn stamp(s: &str) -> Option<i64> {
    let b = s.as_bytes().get(..19)?;
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        b[r].iter().try_fold(0, |a, &c| c.is_ascii_digit().then(|| a * 10 + (c - b'0') as i64))
    };
    if [4, 7, 10, 13, 16].iter().any(|&i| b[i].is_ascii_digit()) {
        return None;
    }
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo as u32, d as u32) * 86400 + h * 3600 + mi * 60 + se)
}

/// `2026-01-27_16-31-59-left_repeater.mp4` -> (start, camera index).
pub fn parse_clip_name(name: &str) -> Option<(i64, usize)> {
    let lower = name.to_ascii_lowercase();
    let cam = lower.strip_suffix(".mp4")?.get(19..)?.strip_prefix('-')?;
    Some((stamp(&lower)?, CAMERAS.iter().position(|&c| c == cam)?))
}

fn kind_of(dir_name: &str) -> Option<Kind> {
    match dir_name.to_ascii_lowercase().as_str() {
        "savedclips" => Some(Kind::Saved),
        "sentryclips" => Some(Kind::Sentry),
        "recentclips" => Some(Kind::Recent),
        _ => None,
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn subdirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)?.filter_map(|e| Some(e.ok()?.path())).filter(|p| p.is_dir()).collect();
    v.sort();
    Ok(v)
}

/// Accepts a TeslaCam root (or a folder holding one), one SavedClips/SentryClips/RecentClips
/// folder, or one event folder. Newest event first.
pub fn scan(dir: &Path) -> io::Result<Vec<Event>> {
    // A relative path may have no name or parent to go by.
    let dir = &std::path::absolute(dir)?;
    let mut events = Vec::new();
    if let Some(kind) = kind_of(&file_name(dir)) {
        scan_kind(dir, kind, &mut events)?;
    } else {
        let subs = subdirs(dir)?;
        let roots: Vec<_> = subs.iter().filter_map(|p| Some((p, kind_of(&file_name(p))?))).collect();
        if let Some(tc) = subs.iter().find(|p| file_name(p).eq_ignore_ascii_case("teslacam")).filter(|_| roots.is_empty()) {
            return scan(tc);
        }
        if roots.is_empty() {
            let parent = dir.parent().map(file_name).unwrap_or_default();
            let kind = kind_of(&parent).filter(|&k| k != Kind::Recent).unwrap_or(Kind::Folder);
            let event = scan_event(dir, kind, file_name(dir))?;
            // A copied event folder still is one, as in the legacy app.
            events.extend(event.map(|e| Event { kind: if e.kind == Kind::Folder && e.info.is_some() { Kind::Saved } else { e.kind }, ..e }));
        }
        scan_roots(&roots, &mut events)?;
    }
    events.sort_by_key(|e| std::cmp::Reverse(e.time()));
    Ok(events)
}

/// One unreadable kind folder must not hide the others; it fails the scan only if nothing else
/// turned up.
// shortcut: next to readable folders the failure goes unreported, show it in the library if people miss footage.
fn scan_roots(roots: &[(&PathBuf, Kind)], out: &mut Vec<Event>) -> io::Result<()> {
    let mut err = None;
    for (p, kind) in roots {
        if let Err(e) = scan_kind(p, *kind, out) {
            err.get_or_insert(e);
        }
    }
    match err {
        Some(e) if out.is_empty() => Err(e),
        _ => Ok(()),
    }
}

/// RecentClips is flat and becomes one event (its gaps show on the timeline); the others hold
/// one folder per event.
fn scan_kind(dir: &Path, kind: Kind, out: &mut Vec<Event>) -> io::Result<()> {
    if kind == Kind::Recent {
        out.extend(scan_event(dir, kind, file_name(dir))?);
        return Ok(());
    }
    for d in subdirs(dir)? {
        // One unreadable event folder must not hide the rest.
        if let Ok(Some(e)) = scan_event(&d, kind, file_name(&d)) {
            out.push(e);
        }
    }
    Ok(())
}

fn scan_event(dir: &Path, kind: Kind, name: String) -> io::Result<Option<Event>> {
    let mut segs: BTreeMap<i64, [Option<PathBuf>; 6]> = BTreeMap::new();
    let (mut thumb, mut json) = (None, None);
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        let n = file_name(&p);
        if let Some((start, cam)) = parse_clip_name(&n) {
            segs.entry(start).or_default()[cam] = Some(p);
        } else if n.eq_ignore_ascii_case("thumb.png") {
            thumb = Some(p);
        } else if n.eq_ignore_ascii_case("event.json") {
            json = Some(p);
        }
    }
    if segs.is_empty() {
        return Ok(None);
    }
    let segments: Vec<Segment> = segs.into_iter().map(|(start, files)| Segment { start, files }).collect();
    // Only if every camera with data agrees: one damaged file must not lock playable footage away.
    let mut probes = segments[0].files.iter().flatten().filter(|p| fs::metadata(p).is_ok_and(|m| m.len() > 0)).peekable();
    let encrypted = probes.peek().is_some() && probes.all(|p| matches!(mp4::read_track(p), Err(Mp4Error::Encrypted)));
    let info = json.and_then(|p| fs::read(p).ok()).and_then(|b| parse_event_json(&b));
    Ok(Some(Event { kind, name, dir: dir.to_path_buf(), segments, info, thumb, encrypted }))
}

/// Tesla writes every value as a string (`"est_lat":"52.5163"`); numbers are accepted too.
pub fn parse_event_json(bytes: &[u8]) -> Option<EventInfo> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let s = |k: &str| match &v[k] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        x => x.to_string(),
    };
    let f = |k: &str| s(k).trim().parse::<f64>().ok().filter(|x| x.is_finite());
    Some(EventInfo {
        timestamp: stamp(&s("timestamp")),
        reason: s("reason"),
        city: s("city"),
        street: s("street"),
        est_lat: f("est_lat"),
        est_lon: f("est_lon"),
        camera: s("camera"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        for days in [-800_000, -1, 0, 59, 60, 11016, 11017, 20_000, 2_000_000] {
            let (y, m, d, ..) = civil(days * 86400);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        let t = stamp("2024-02-29_23-59-58").unwrap();
        assert_eq!(civil(t), (2024, 2, 29, 23, 59, 58));
        assert_eq!(stamp("2024-02-29T23:59:58"), Some(t));
        assert_eq!(stamp("2024-03-01_00-00-00").unwrap() - t, 2);
        assert_eq!(fmt_stamp(t), "2024-02-29_23-59-58");
    }

    #[test]
    fn clip_names() {
        let t = stamp("2026-01-27_16-31-59").unwrap();
        assert_eq!(parse_clip_name("2026-01-27_16-31-59-front.mp4"), Some((t, 0)));
        assert_eq!(parse_clip_name("2026-01-27_16-31-59-LEFT_REPEATER.MP4"), Some((t, 2)));
        assert_eq!(parse_clip_name("2026-01-27_16-31-59-right_pillar.mp4"), Some((t, 5)));
        for bad in [
            "2026-01-27_16-31-59-side.mp4",
            "2026-01-27_16-31-59-front.mov",
            "2026-13-27_16-31-59-front.mp4",
            "2026-01-27_16-31-5x-front.mp4",
            "2026-01-27_16-31-59front.mp4",
            "thumb.png",
            "",
        ] {
            assert_eq!(parse_clip_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn event_json() {
        let j = br#"{"timestamp":"2026-03-14T15:30:38","city":"Berlin","street":"Unter den Linden","est_lat":"52.5163","est_lon":13.3777,"reason":"user_interaction_honk","camera":"0"}"#;
        let i = parse_event_json(j).unwrap();
        assert_eq!(i.timestamp, stamp("2026-03-14_15-30-38"));
        assert_eq!((i.city.as_str(), i.reason.as_str(), i.camera.as_str()), ("Berlin", "user_interaction_honk", "0"));
        assert_eq!(i.street, "Unter den Linden");
        assert_eq!((i.est_lat, i.est_lon), (Some(52.5163), Some(13.3777)));
        let i = parse_event_json(br#"{"est_lat":"","reason":5}"#).unwrap();
        assert_eq!((i.timestamp, i.est_lat, i.reason.as_str()), (None, None, "5"));
        assert_eq!(parse_event_json(b"not json"), None);
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("otc-lib-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }

    fn touch(dir: &Path, names: &[&str], data: &[u8]) {
        fs::create_dir_all(dir).unwrap();
        for n in names {
            fs::write(dir.join(n), data).unwrap();
        }
    }

    /// Minimal valid header: `read_track` gets past ftyp (then fails on the missing moov).
    const FTYP: &[u8] = b"\0\0\0\x10ftypisom\0\0\x02\0";

    fn tree(root: &Path) {
        let saved = root.join("SavedClips/2026-03-14_15-31-00");
        touch(&saved, &["2026-03-14_15-30-00-front.mp4", "2026-03-14_15-30-00-back.mp4", "2026-03-14_15-31-00-front.mp4"], FTYP);
        touch(&saved, &["thumb.png", "notes.txt"], b"x");
        touch(&saved, &["event.json"], br#"{"reason":"user_interaction_dashcam_launcher_action_tapped","city":"Berlin"}"#);
        let enc = root.join("SentryClips/2026-03-15_09-00-00");
        touch(&enc, &["2026-03-15_08-59-00-front.mp4"], &[0x5A; 64]);
        touch(&root.join("SentryClips/empty"), &["readme.txt"], b"x");
        let recent = root.join("RecentClips");
        touch(&recent, &["2026-03-13_10-00-00-front.mp4", "2026-03-13_10-01-00-front.mp4", "2026-03-13_10-05-00-Right_Pillar.mp4"], FTYP);
    }

    #[test]
    fn scans_teslacam_root() {
        let t = tmp("root");
        tree(&t.0);
        let ev = scan(&t.0).unwrap();
        let summary: Vec<_> = ev.iter().map(|e| (e.kind, e.name.as_str(), e.segments.len(), e.encrypted)).collect();
        assert_eq!(
            summary,
            [
                (Kind::Sentry, "2026-03-15_09-00-00", 1, true),
                (Kind::Saved, "2026-03-14_15-31-00", 2, false),
                (Kind::Recent, "RecentClips", 3, false),
            ]
        );
        let saved = &ev[1];
        assert_eq!(saved.segments[1].start - saved.segments[0].start, 60);
        assert!(saved.segments[0].files[0].is_some() && saved.segments[0].files[1].is_some());
        assert!(saved.segments[1].files[1..].iter().all(Option::is_none));
        assert!(saved.thumb.as_ref().unwrap().ends_with("thumb.png"));
        assert_eq!(saved.info.as_ref().unwrap().city, "Berlin");
        assert_eq!(saved.approx_duration(), 120);
        // RecentClips stays one event; the 4-minute gap is kept in the start times.
        let r = &ev[2];
        assert_eq!(r.segments[2].start - r.segments[1].start, 240);
        assert!(r.segments[2].files[5].is_some());
        assert!(r.thumb.is_none() && r.info.is_none());
    }

    #[test]
    fn scans_partial_inputs() {
        let t = tmp("partial");
        tree(&t.0.join("TeslaCam"));
        // A drive root holding TeslaCam/.
        assert_eq!(scan(&t.0).unwrap().len(), 3);
        // One kind folder.
        let s = scan(&t.0.join("TeslaCam/SavedClips")).unwrap();
        assert_eq!((s.len(), s[0].kind), (1, Kind::Saved));
        assert_eq!(scan(&t.0.join("TeslaCam/RecentClips")).unwrap()[0].segments.len(), 3);
        // One event folder: kind from its parent, else Folder.
        let one = scan(&t.0.join("TeslaCam/SavedClips/2026-03-14_15-31-00")).unwrap();
        assert_eq!((one.len(), one[0].kind, one[0].segments.len()), (1, Kind::Saved, 2));
        let loose = t.0.join("loose");
        touch(&loose, &["2026-01-01_00-00-00-back.mp4"], FTYP);
        let l = scan(&loose).unwrap();
        assert_eq!((l[0].kind, l[0].name.as_str()), (Kind::Folder, "loose"));
        // With an event.json it's a saved event wherever it was copied to.
        touch(&loose, &["event.json"], br#"{"timestamp":"2026-01-01T00:00:30"}"#);
        assert_eq!(scan(&loose).unwrap()[0].kind, Kind::Saved);
        assert!(scan(&t.0.join("TeslaCam/SentryClips/empty")).unwrap().is_empty());
        assert!(scan(&t.0.join("missing")).is_err());
    }

    #[test]
    fn one_unreadable_kind_folder_does_not_hide_the_others() {
        let t = tmp("kinds");
        tree(&t.0);
        let (saved, missing) = (t.0.join("SavedClips"), t.0.join("SentryClips-gone"));
        let mut events = Vec::new();
        scan_roots(&[(&missing, Kind::Sentry), (&saved, Kind::Saved)], &mut events).unwrap();
        assert_eq!(events.len(), 1);
        // Nothing else turned up: the error is the result.
        assert!(scan_roots(&[(&missing, Kind::Sentry)], &mut Vec::new()).is_err());
    }

    fn encrypted(tag: &str, files: &[(&str, &[u8])]) -> bool {
        let t = tmp(tag);
        for (n, data) in files {
            touch(&t.0, &[n], data);
        }
        scan(&t.0).unwrap()[0].encrypted
    }

    #[test]
    fn encrypted_means_every_camera_with_data() {
        const LOCKED: &[u8] = &[0x5A; 64];
        let (front, back) = ("2026-03-14_15-30-00-front.mp4", "2026-03-14_15-30-00-back.mp4");
        assert!(encrypted("enc-all", &[(front, LOCKED), (back, LOCKED)]));
        assert!(encrypted("enc-empty-first", &[(front, b""), (back, LOCKED)]), "an empty file says nothing");
        assert!(!encrypted("enc-zeroed", &[(front, &[0; 4096]), (back, FTYP)]), "one zero-filled camera");
        assert!(!encrypted("enc-all-zeroed", &[(front, &[0; 4096]), (back, &[0; 4096])]), "zeros aren't encryption");
        assert!(!encrypted("enc-garbled", &[(front, LOCKED), (back, FTYP)]), "another camera is a real MP4");
        assert!(!encrypted("enc-empty", &[(front, b"")]));
    }

    #[test]
    fn events_sort_by_the_time_they_list() {
        let t = tmp("sort");
        // Segments start long before the trigger, and RecentClips ran until after both.
        let saved = t.0.join("SavedClips/2026-03-14_15-31-00");
        touch(&saved, &["2026-03-14_15-21-00-front.mp4"], FTYP);
        touch(&saved, &["event.json"], br#"{"timestamp":"2026-03-14T15:31:00"}"#);
        touch(&t.0.join("SentryClips/2026-03-14_15-25-00"), &["2026-03-14_15-24-00-front.mp4"], FTYP);
        touch(&t.0.join("RecentClips"), &["2026-03-14_15-00-00-front.mp4", "2026-03-14_18-00-00-front.mp4"], FTYP);
        let ev = scan(&t.0).unwrap();
        assert_eq!(ev.iter().map(|e| e.kind).collect::<Vec<_>>(), [Kind::Recent, Kind::Saved, Kind::Sentry]);
        assert_eq!(ev[0].time(), stamp("2026-03-14_18-00-00").unwrap());
        assert_eq!(ev[1].time(), stamp("2026-03-14_15-31-00").unwrap());
        assert_eq!(ev[2].time(), stamp("2026-03-14_15-25-00").unwrap(), "no event.json: the folder's name");
    }
}
