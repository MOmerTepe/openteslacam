//! Managing an event in the library: delete it, keep only the part that matters, drop cameras, or
//! move it to the archive. Everything removed goes to the Recycle Bin, and nothing is re-encoded.

use crate::library::{Event, Kind};
use crate::player::SegTime;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Under the library folder; the scanner doesn't look in it, so archived events leave the list.
pub const ARCHIVE: &str = "Archive";
/// Trimming keeps the footage from this long before the range (seconds), like the lead-in on open,
/// and a little after it.
const TRIM_BEFORE: f64 = 10.0;
const TRIM_AFTER: f64 = 5.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Delete,
    /// Keep the segments touching this range (seconds from the event start). Whole segments only:
    /// cutting inside one would start each camera at its own keyframe and put them out of step.
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
            let keep = |s: &SegTime| s.dur > 0.0 && s.start <= to + TRIM_AFTER && s.start + s.dur > from - TRIM_BEFORE;
            let kept = segs.iter().filter(|s| keep(s)).count();
            if kept == 0 {
                return Err("no footage in that range".to_owned());
            }
            let remove: Vec<PathBuf> = ev.segments.iter().zip(segs).filter(|(_, t)| !keep(t)).flat_map(|(s, _)| s.files.iter().flatten().cloned()).collect();
            if remove.is_empty() { Err("the range already covers every segment".to_owned()) } else { Ok(Plan { remove, kept, ..Plan::default() }) }
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

/// Bytes `plan` takes out of the library's event (an archived event still takes its space).
pub fn freed(plan: &Plan) -> u64 {
    fn size(p: &Path) -> u64 {
        match fs::metadata(p) {
            Ok(m) if m.is_dir() => fs::read_dir(p).map_or(0, |d| d.flatten().map(|e| size(&e.path())).sum()),
            Ok(m) => m.len(),
            Err(_) => 0,
        }
    }
    plan.remove.iter().map(|p| size(p)).sum()
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
    for p in plan.remove.iter().filter(|p| p.exists()) {
        retry(|| if bin { recycle(p) } else { delete(p) })?;
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
    fn trim_keeps_whole_segments_around_the_range() {
        let (lib, ev) = library("trim");
        let segs = Player::open(&ev).segs;
        assert_eq!(ev.segments.len(), 2);
        // Far into a long event: only the segments within reach of the range stay.
        let long: Vec<SegTime> = (0..10).map(|i| SegTime { start: i as f64 * 60.0, dur: 60.0, frames: vec![] }).collect();
        let mut big = ev.clone();
        big.segments = (0..10).map(|_| ev.segments[0].clone()).collect();
        let p = plan(&big, &long, &Action::Trim(200.0, 215.0)).unwrap();
        assert_eq!(p.kept, 1, "190..220 s lies in segment 4 alone");
        assert_eq!(plan(&big, &long, &Action::Trim(185.0, 185.0)).unwrap().kept, 2, "the lead-in reaches back into segment 3");
        assert!(plan(&big, &long, &Action::Trim(9000.0, 9001.0)).is_err(), "nothing there");
        assert!(plan(&ev, &segs, &Action::Trim(0.0, 4.0)).is_err(), "nothing to drop");

        // For real: a range past the short fixture's lead-in drops its first segment.
        let far: Vec<SegTime> = segs.iter().enumerate().map(|(i, s)| SegTime { start: i as f64 * 60.0, ..s.clone() }).collect();
        let p = plan(&ev, &far, &Action::Trim(61.0, 61.5)).unwrap();
        assert!(freed(&p) > 0);
        apply(&p, false).unwrap();
        let after = saved(&lib);
        assert_eq!((after.segments.len(), after.segments[0].start), (1, ev.segments[1].start));
        assert_eq!(after.info, ev.info, "event.json stays");
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
