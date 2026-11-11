//! Managing an event in the library: delete it, keep only the part that matters, drop cameras, or
//! move it to the archive. Everything removed goes to the Recycle Bin, and nothing is re-encoded.

use crate::library::{Event, Kind, fmt_stamp};
use crate::mp4::{self, Track};
use crate::mux::Muxer;
use crate::player::SegTime;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Under the library folder; the scanner doesn't look in it, so archived events leave the list.
pub const ARCHIVE: &str = "Archive";
/// How far a keyframe may sit from a whole second and still count as on it: half a frame at 24 fps.
const ON_SECOND: f64 = 0.021;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Delete,
    /// Keep this range (seconds from the event start). The end is cut to the frame. The start is
    /// cut at the last whole second before it where every camera has a keyframe: a clip can only
    /// start on one, the cameras' keyframes rarely fall together, and a clip's name gives its
    /// start in whole seconds. Each camera cut at its own keyframe would put them out of step.
    Trim(f64, f64),
    /// Cameras to keep, indexed like `library::CAMERAS`.
    Cameras([bool; 6]),
    /// Into the archive of this library folder.
    Archive(PathBuf),
}

/// What an action does to the disk.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Files, or the whole event folder, to remove.
    pub remove: Vec<PathBuf>,
    /// The event folder and where it moves to.
    pub rename: Option<(PathBuf, PathBuf)>,
    /// Segments left afterwards, of the event's count.
    pub kept: usize,
    /// Clips to write anew, shorter; their originals are removed too.
    pub cut: Vec<Cut>,
}

/// One clip rewritten without re-encoding: the samples from the keyframe at `from` through `to`,
/// both in seconds into the clip.
#[derive(Debug, Clone, PartialEq)]
pub struct Cut {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub from: f64,
    pub to: f64,
}

fn all_files(ev: &Event) -> Vec<PathBuf> {
    ev.segments.iter().flat_map(|s| s.files.iter().flatten().cloned()).collect()
}

/// `segs` are `Player::segs` of `ev`. Err says why the action makes no sense here.
pub fn plan(ev: &Event, segs: &[SegTime], action: &Action) -> Result<Plan, String> {
    let n = ev.segments.len();
    match action {
        // RecentClips is one flat folder that the event doesn't own.
        Action::Delete if ev.kind == Kind::Recent => Ok(Plan { remove: all_files(ev), kept: 0, ..Plan::default() }),
        Action::Delete => Ok(Plan { remove: vec![ev.dir.clone()], kept: 0, ..Plan::default() }),
        Action::Trim(from, to) => {
            let keep = |s: &SegTime| s.dur > 0.0 && s.start <= *to && s.start + s.dur > *from;
            let kept: Vec<usize> = (0..n.min(segs.len())).filter(|&i| keep(&segs[i])).collect();
            let (Some(&first), Some(&last)) = (kept.first(), kept.last()) else { return Err("no footage in that range".to_owned()) };
            let remove: Vec<PathBuf> = ev.segments.iter().zip(segs).filter(|(_, t)| !keep(t)).flat_map(|(s, _)| s.files.iter().flatten().cloned()).collect();
            let mut cut = Vec::new();
            for i in if first == last { vec![first] } else { vec![first, last] } {
                let (seg, t) = (&ev.segments[i], &segs[i]);
                let lo = if i == first { (from - t.start).max(0.0) } else { 0.0 };
                let hi = if i == last { to - t.start } else { f64::INFINITY };
                let files: Vec<&PathBuf> = seg.files.iter().flatten().collect();
                let tracks = files.iter().map(|p| mp4::read_track(p).map_err(|e| format!("{}: {e}", p.display()))).collect::<Result<Vec<Track>, String>>()?;
                let start = shared_key(&tracks, lo);
                if start == 0 && hi >= t.dur {
                    continue;
                }
                for src in files {
                    // The clip's name is its start time, then the camera.
                    let name = src.file_name().and_then(|n| n.to_str()).and_then(|n| n.get(19..)).ok_or("a clip has no time in its name")?;
                    let dst = src.with_file_name(format!("{}{name}", fmt_stamp(seg.start + start as i64)));
                    cut.push(Cut { src: src.clone(), dst, from: start as f64, to: hi });
                }
            }
            if remove.is_empty() && cut.is_empty() { Err("the range already covers the whole event".to_owned()) } else { Ok(Plan { remove, kept: kept.len(), cut, ..Plan::default() }) }
        }
        Action::Cameras(keep) => {
            let has = |on: bool| ev.segments.iter().any(|s| s.files.iter().zip(keep).any(|(f, k)| f.is_some() && *k == on));
            if !has(true) {
                return Err("that would leave no camera".to_owned());
            }
            if !has(false) {
                return Err("every camera here is ticked".to_owned());
            }
            let remove = ev.segments.iter().flat_map(|s| s.files.iter().zip(keep).filter(|(_, k)| !**k).filter_map(|(f, _)| f.clone())).collect();
            Ok(Plan { remove, kept: n, ..Plan::default() })
        }
        Action::Archive(_) if ev.kind == Kind::Recent => Err("the rolling buffer has no folder of its own to archive".to_owned()),
        Action::Archive(lib) => {
            let kind = ev.dir.parent().and_then(Path::file_name).ok_or("the event has no folder")?;
            let to = lib.join(ARCHIVE).join(kind).join(&ev.name);
            if to.exists() { Err(format!("{} is already there", to.display())) } else { Ok(Plan { rename: Some((ev.dir.clone(), to)), kept: n, ..Plan::default() }) }
        }
    }
}

fn secs(pts: u64, t: &Track) -> f64 {
    pts as f64 / t.timescale.max(1) as f64
}

/// The last whole second at or before `lo` (seconds into the clips) where every camera has a keyframe.
fn shared_key(tracks: &[Track], lo: f64) -> u32 {
    let on = |t: &Track, c: u32| t.samples.iter().any(|s| s.key && (secs(s.pts, t) - c as f64).abs() < ON_SECOND);
    (1..=lo.max(0.0).floor() as u32).rev().find(|&c| tracks.iter().all(|t| on(t, c))).unwrap_or(0)
}

/// Writes `c.dst` + `.part`: the samples as they are, so the picture and the vehicle data in it stay
/// bit for bit. Read back before it counts.
fn write_cut(c: &Cut) -> Result<PathBuf, String> {
    let part = PathBuf::from(format!("{}.part", c.dst.display()));
    let src_err = |e: &dyn std::fmt::Display| format!("{}: {e}", c.src.display());
    let part_err = |e: std::io::Error| format!("{}: {e}", part.display());
    let t = mp4::read_track(&c.src).map_err(|e| src_err(&e))?;
    let first = t.samples.iter().position(|s| s.key && secs(s.pts, &t) > c.from - ON_SECOND).ok_or_else(|| src_err(&"no keyframe to start on"))?;
    // With B-frames a later sample can show earlier: such a clip keeps its end.
    let reordered = t.samples.windows(2).any(|w| w[1].pts < w[0].pts);
    let last = if reordered { t.samples.len() - 1 } else { t.samples.iter().rposition(|s| secs(s.pts, &t) <= c.to).unwrap_or(0).max(first) };
    let mut f = File::open(&c.src).map_err(|e| src_err(&e))?;
    let mut m = Muxer::create(&part, t.timescale, t.width, t.height, &t.avcc).map_err(part_err)?;
    for s in &t.samples[first..=last] {
        let data = mp4::read_sample(&mut f, s).map_err(|e| src_err(&e))?;
        m.push(&data, s.dur, (s.pts as i64 - s.dts as i64) as i32, s.key).map_err(part_err)?;
    }
    m.finish().map_err(part_err)?;
    match mp4::read_track(&part) {
        Ok(new) if new.samples.len() == last - first + 1 => Ok(part),
        Ok(new) => Err(format!("{}: {} frames written, {} read back", part.display(), last - first + 1, new.samples.len())),
        Err(e) => Err(format!("{}: {e}", part.display())),
    }
}

/// Bytes `plan` sends to the Recycle Bin (an archived event still takes its space). A cut clip's
/// original goes there whole.
pub fn freed(plan: &Plan) -> u64 {
    fn size(p: &Path) -> u64 {
        match fs::metadata(p) {
            Ok(m) if m.is_dir() => fs::read_dir(p).map_or(0, |d| d.flatten().map(|e| size(&e.path())).sum()),
            Ok(m) => m.len(),
            Err(_) => 0,
        }
    }
    plan.remove.iter().chain(plan.cut.iter().map(|c| &c.src)).map(|p| size(p)).sum()
}

/// A decoder thread may still be letting go of a file the app had open a moment ago.
fn retry<T>(mut op: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let mut last = op();
    for _ in 0..30 {
        if last.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        last = op();
    }
    last
}

/// To the Recycle Bin, so a wrong click can be undone.
// shortcut: a drive without a Recycle Bin (some network shares) deletes for good without saying so; check the drive first if libraries end up there.
#[cfg(windows)]
fn recycle(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    #[repr(C)]
    struct FileOp {
        hwnd: isize,
        func: u32,
        from: *const u16,
        to: *const u16,
        flags: u16,
        aborted: i32,
        mappings: *mut std::ffi::c_void,
        title: *const u16,
    }
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHFileOperationW(op: *mut FileOp) -> i32;
    }
    // FO_DELETE; FOF_SILENT | FOF_NOCONFIRMATION | FOF_ALLOWUNDO | FOF_NOERRORUI.
    const DELETE: u32 = 3;
    const QUIET_UNDOABLE: u16 = 0x4 | 0x10 | 0x40 | 0x400;
    // The list of paths ends with two NULs.
    let from: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    let mut op = FileOp { hwnd: 0, func: DELETE, from: from.as_ptr(), to: std::ptr::null(), flags: QUIET_UNDOABLE, aborted: 0, mappings: std::ptr::null_mut(), title: std::ptr::null() };
    // Safety: `op` matches SHFILEOPSTRUCTW on 64-bit Windows and `from` outlives the call.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code == 0 && op.aborted == 0 && !path.exists() { Ok(()) } else { Err(format!("{}: couldn't be moved to the Recycle Bin (code {code})", path.display())) }
}

#[cfg(not(windows))]
fn recycle(path: &Path) -> Result<(), String> {
    delete(path)
}

fn delete(path: &Path) -> Result<(), String> {
    let r = if path.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    r.map_err(|e| format!("{}: {e}", path.display()))
}

/// Carries `plan` out. Call it off the UI thread, after the app let go of the event. `bin`: removed
/// files go to the Recycle Bin rather than for good.
pub fn apply(plan: &Plan, bin: bool) -> Result<(), String> {
    // Every new clip is written and read back before any original goes.
    let mut parts = Vec::new();
    for c in &plan.cut {
        match retry(|| write_cut(c)) {
            Ok(p) => parts.push(p),
            Err(e) => {
                parts.push(PathBuf::from(format!("{}.part", c.dst.display())));
                parts.iter().for_each(|p| drop(fs::remove_file(p)));
                return Err(e);
            }
        }
    }
    for p in plan.remove.iter().chain(plan.cut.iter().map(|c| &c.src)).filter(|p| p.exists()) {
        retry(|| if bin { recycle(p) } else { delete(p) })?;
    }
    for (c, part) in plan.cut.iter().zip(&parts) {
        retry(|| fs::rename(part, &c.dst).map_err(|e| format!("{}: {e}", part.display())))?;
    }
    if let Some((from, to)) = &plan.rename {
        fs::create_dir_all(to.parent().unwrap_or(to)).map_err(|e| format!("{}: {e}", to.display()))?;
        retry(|| fs::rename(from, to).map_err(|e| format!("{}: {e}", from.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::library::scan;
    use crate::player::Player;

    /// (library folder with the fixture in it, its Saved event of two 2 s segments).
    fn library(name: &str) -> (PathBuf, Event) {
        let lib = std::env::temp_dir().join(format!("otc-manage-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&lib);
        fixture::make_fixture(&lib, 2, 64, 48).unwrap();
        (lib.clone(), saved(&lib))
    }

    fn saved(lib: &Path) -> Event {
        scan(lib).unwrap().into_iter().find(|e| e.kind == Kind::Saved && !e.encrypted).unwrap()
    }

    fn files(ev: &Event) -> usize {
        all_files(ev).len()
    }

    #[test]
    fn trim_cuts_to_the_range_without_re_encoding() {
        let (lib, ev) = library("trim");
        let segs = Player::open(&ev).segs;
        // Two segments of 2 s, 36 frames a second, a keyframe every second in every camera.
        assert_eq!((ev.segments.len(), segs[1].start, segs[1].dur), (2, 2.0, 2.0));
        assert!(plan(&ev, &segs, &Action::Trim(9000.0, 9001.0)).is_err(), "nothing there");
        assert!(plan(&ev, &segs, &Action::Trim(0.0, 4.0)).is_err(), "nothing to drop");
        assert!(plan(&ev, &segs, &Action::Trim(0.9, 4.0)).is_err(), "no shared keyframe between the start and In");

        // In 1.2 s into the second segment, Out half a second later: the first segment goes, and the
        // second starts on its keyframe at 1 s and ends on the frame at Out.
        let p = plan(&ev, &segs, &Action::Trim(3.2, 3.5)).unwrap();
        assert_eq!((p.kept, p.remove.len(), p.cut.len()), (1, files(&ev) / 2, files(&ev) / 2));
        assert!(p.cut.iter().all(|c| (c.from, c.to) == (1.0, 1.5) && c.dst != c.src));
        assert!(freed(&p) > 0);
        let front = ev.segments[1].files[0].clone().unwrap();
        let old = mp4::read_track(&front).unwrap();
        let bytes = |path: &Path, t: &Track, i: usize| mp4::read_sample(&mut File::open(path).unwrap(), &t.samples[i]).unwrap();
        let sample_36 = bytes(&front, &old, 36);
        apply(&p, false).unwrap();

        let after = saved(&lib);
        assert_eq!((after.segments.len(), after.segments[0].start), (1, ev.segments[1].start + 1), "named by its new start");
        assert_eq!(files(&after), files(&ev) / 2, "every camera, and no leftovers");
        assert_eq!(fs::read_dir(&after.dir).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "part")).count(), 0);
        let new_front = after.segments[0].files[0].clone().unwrap();
        let new = mp4::read_track(&new_front).unwrap();
        // Frames 36 (1.0 s) through 54 (1.5 s), the same bytes as before.
        assert_eq!((new.samples.len(), new.samples[0].key, new.samples[0].pts), (19, true, 0));
        assert_eq!(bytes(&new_front, &new, 0), sample_36);
        assert_eq!(after.info, ev.info, "event.json stays");

        // Only an Out inside the one clip left: it keeps its name and loses its tail.
        let segs = Player::open(&after).segs;
        let p = plan(&after, &segs, &Action::Trim(0.0, 0.25)).unwrap();
        assert!(p.remove.is_empty() && p.cut.iter().all(|c| c.dst == c.src));
        apply(&p, false).unwrap();
        let last = saved(&lib);
        assert_eq!((last.segments[0].start, files(&last)), (after.segments[0].start, files(&after)));
        assert_eq!(mp4::read_track(last.segments[0].files[0].as_ref().unwrap()).unwrap().samples.len(), 10);
        fs::remove_dir_all(&lib).unwrap();
    }

    #[test]
    fn a_clip_that_cannot_be_written_leaves_the_originals() {
        let (lib, ev) = library("trimfail");
        let segs = Player::open(&ev).segs;
        let mut p = plan(&ev, &segs, &Action::Trim(3.2, 3.5)).unwrap();
        p.cut.last_mut().unwrap().dst = lib.join("no such folder").join("x.mp4");
        assert!(apply(&p, false).is_err());
        let after = saved(&lib);
        assert_eq!((after.segments.len(), files(&after)), (2, files(&ev)));
        assert_eq!(fs::read_dir(&after.dir).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "part")).count(), 0);
        fs::remove_dir_all(&lib).unwrap();
    }

    #[test]
    fn cameras_archive_and_delete() {
        let (lib, ev) = library("rest");
        let segs = Player::open(&ev).segs;
        let before = files(&ev);

        let mut keep = [false; 6];
        assert!(plan(&ev, &segs, &Action::Cameras(keep)).is_err(), "no camera left");
        assert!(plan(&ev, &segs, &Action::Cameras([true; 6])).is_err(), "nothing to drop");
        (keep[0], keep[1]) = (true, true);
        apply(&plan(&ev, &segs, &Action::Cameras(keep)).unwrap(), false).unwrap();
        let ev = saved(&lib);
        assert!(files(&ev) < before && ev.segments.iter().all(|s| s.files[0].is_some() && s.files[2..].iter().all(Option::is_none)));

        // Archived: out of the library's list, whole in the archive, and that folder opens like a library.
        let listed = scan(&lib).unwrap().len();
        let p = plan(&ev, &segs, &Action::Archive(lib.clone())).unwrap();
        assert_eq!(freed(&p), 0);
        apply(&p, false).unwrap();
        assert_eq!(scan(&lib).unwrap().len(), listed - 1);
        let archived = saved(&lib.join(ARCHIVE));
        assert_eq!((archived.name.as_str(), files(&archived)), (ev.name.as_str(), files(&ev)));

        apply(&plan(&archived, &segs, &Action::Delete).unwrap(), false).unwrap();
        assert!(!archived.dir.exists());

        // RecentClips: only its clips go, not the folder the car writes to.
        let recent = scan(&lib).unwrap().into_iter().find(|e| e.kind == Kind::Recent).unwrap();
        assert!(plan(&recent, &segs, &Action::Archive(lib.clone())).is_err());
        apply(&plan(&recent, &segs, &Action::Delete).unwrap(), false).unwrap();
        assert!(recent.dir.is_dir() && !all_files(&recent).iter().any(|f| f.exists()));
        fs::remove_dir_all(&lib).unwrap();
    }

    /// Puts a file in the real Recycle Bin. `cargo test recycle -- --ignored`
    #[test]
    #[ignore]
    fn recycle_bin_takes_a_file() {
        let f = std::env::temp_dir().join(format!("otc-recycle-test-{}.txt", std::process::id()));
        fs::write(&f, b"safe to delete: an OpenTeslaCam test file").unwrap();
        recycle(&f).unwrap();
        assert!(!f.exists());
        assert!(recycle(&f).is_err(), "a missing file is an error, not a silent success");
    }
}
