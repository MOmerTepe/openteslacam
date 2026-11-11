//! Import: copies a TeslaCam drive into the library folder on this PC, keeping Tesla's layout so
//! `library::scan` reads the library like any drive. A file only counts once its copy has been read
//! back and matches, and only then may it be cleared from the drive.

use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// What `run` returns when `cancel` was set.
pub const CANCELLED: &str = "cancelled";
/// The folders that are imported, under these names whatever their case on the drive.
const KINDS: [&str; 3] = ["SavedClips", "SentryClips", "RecentClips"];

pub struct Job {
    /// A TeslaCam folder, or a drive holding one.
    pub src: PathBuf,
    pub lib: PathBuf,
    /// Delete each file from `src` once its copy is verified.
    pub clear: bool,
}

/// Counted up by the worker, read by the dialog.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub total_files: AtomicU64,
    pub bytes: AtomicU64,
    pub total_bytes: AtomicU64,
}

#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub copied: u64,
    /// Already in the library from an earlier import.
    pub already: u64,
    pub cleared: u64,
    /// What was left out, in words.
    pub notes: Vec<String>,
}

fn child_named(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.filter_map(|e| Some(e.ok()?.path())).find(|p| p.is_dir() && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)))
}

/// The folder holding SavedClips and the others: `src`, or the TeslaCam folder in it.
pub fn teslacam_root(src: &Path) -> Option<PathBuf> {
    let has_kinds = |d: &Path| KINDS.iter().any(|k| child_named(d, k).is_some());
    if has_kinds(src) {
        return Some(src.to_path_buf());
    }
    child_named(src, "TeslaCam").filter(|d| has_kinds(d))
}

fn files_in(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)?.filter_map(|e| Some(e.ok()?.path())).collect();
    v.sort();
    Ok(v)
}

/// Every file to import: (on the drive, path under the library, size). Event folders one level down,
/// and RecentClips' own files.
fn plan(root: &Path) -> io::Result<Vec<(PathBuf, PathBuf, u64)>> {
    let mut out = Vec::new();
    let mut add = |from: PathBuf, rel: PathBuf| {
        if let Some(m) = fs::metadata(&from).ok().filter(|m| m.is_file()) {
            out.push((from, rel, m.len()));
        }
    };
    for kind in KINDS {
        let Some(dir) = child_named(root, kind) else { continue };
        for p in files_in(&dir)? {
            let Some(name) = p.file_name().map(PathBuf::from) else { continue };
            if p.is_dir() {
                // One unreadable event folder must not stop the rest.
                for f in files_in(&p).unwrap_or_default() {
                    if let Some(file) = f.file_name() {
                        add(f.clone(), Path::new(kind).join(&name).join(file));
                    }
                }
            } else {
                add(p, Path::new(kind).join(name));
            }
        }
    }
    Ok(out)
}

fn hash(path: &Path, cancel: &AtomicBool) -> io::Result<Option<[u8; 32]>> {
    let (mut f, mut h, mut buf) = (File::open(path)?, Sha256::new(), vec![0u8; 1 << 20]);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match f.read(&mut buf)? {
            0 => return Ok(Some(h.finalize().into())),
            n => h.update(&buf[..n]),
        }
    }
}

enum Fail {
    Cancelled,
    /// The drive's side: this file is skipped and stays on the drive.
    Source(io::Error),
    /// The library's side: nothing after it would work either.
    Dest(io::Error),
}

/// Copies `from` to `to` and reads the copy back. Ok(false) when `to` was already there: same size,
/// and with `strict` the same content too (what clearing the drive relies on).
fn copy_verified(from: &Path, to: &Path, size: u64, strict: bool, cancel: &AtomicBool, mut wrote: impl FnMut(u64)) -> Result<bool, Fail> {
    let done = |h: io::Result<Option<[u8; 32]>>, side: fn(io::Error) -> Fail| h.map_err(side)?.ok_or(Fail::Cancelled);
    if fs::metadata(to).is_ok_and(|m| m.len() == size) && (!strict || done(hash(from, cancel), Fail::Source)? == done(hash(to, cancel), Fail::Dest)?) {
        wrote(size);
        return Ok(false);
    }
    let part = to.with_added_extension("part");
    fs::create_dir_all(to.parent().unwrap_or(to)).map_err(Fail::Dest)?;
    let mut copy = || -> Result<[u8; 32], Fail> {
        let (mut src, mut dst) = (File::open(from).map_err(Fail::Source)?, File::create(&part).map_err(Fail::Dest)?);
        let (mut h, mut buf) = (Sha256::new(), vec![0u8; 1 << 20]);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Fail::Cancelled);
            }
            let n = src.read(&mut buf).map_err(Fail::Source)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            dst.write_all(&buf[..n]).map_err(Fail::Dest)?;
            wrote(n as u64);
        }
        // On disk, not just in the cache, before anything is deleted because of it.
        dst.sync_all().map_err(Fail::Dest)?;
        Ok(h.finalize().into())
    };
    let result = copy().and_then(|read| match done(hash(&part, cancel), Fail::Dest)? {
        back if back == read => fs::rename(&part, to).map_err(Fail::Dest),
        _ => Err(Fail::Dest(io::Error::other("the copy doesn't match the original"))),
    });
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result.map(|()| true)
}

/// Runs on the worker thread. A cancelled or failed import keeps what it finished: those files are
/// whole and verified, and the next import skips them.
// shortcut: free space isn't checked first, a full disk stops the import at the file that didn't fit; ask the OS up front if that surprises people.
pub fn run(job: &Job, cancel: &AtomicBool, progress: &Progress) -> Result<Summary, String> {
    let root = teslacam_root(&job.src).ok_or_else(|| format!("{}: no SavedClips, SentryClips or RecentClips folder", job.src.display()))?;
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let (root, lib) = (abs(&root), abs(&job.lib));
    if lib.starts_with(&root) || root.starts_with(&lib) {
        return Err("the library can't be inside the drive's TeslaCam folder, or the other way round".to_owned());
    }
    let items = plan(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    progress.total_files.store(items.len() as u64, Ordering::Relaxed);
    progress.total_bytes.store(items.iter().map(|i| i.2).sum(), Ordering::Relaxed);
    let mut s = Summary::default();
    if child_named(&root, "EncryptedClips").is_some() {
        s.notes.push("EncryptedClips stays on the drive: this app can't read it".to_owned());
    }
    for (from, rel, size) in &items {
        let to = lib.join(rel);
        match copy_verified(from, &to, *size, job.clear, cancel, |n| _ = progress.bytes.fetch_add(n, Ordering::Relaxed)) {
            Ok(true) => s.copied += 1,
            Ok(false) => s.already += 1,
            Err(Fail::Cancelled) => return Err(CANCELLED.to_owned()),
            Err(Fail::Dest(e)) => return Err(format!("{}: {e}", to.display())),
            Err(Fail::Source(e)) => {
                s.notes.push(format!("{}: {e}", from.display()));
                progress.files.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }
        progress.files.fetch_add(1, Ordering::Relaxed);
        if job.clear && fs::remove_file(from).is_ok() {
            s.cleared += 1;
            // An emptied event folder goes too; the kind folders stay for the car. `remove_dir` only takes an empty one.
            if rel.components().count() == 3 {
                let _ = from.parent().map(fs::remove_dir);
            }
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Kind, scan};

    const NO: AtomicBool = AtomicBool::new(false);

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("otc-import-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(path: PathBuf, data: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    /// A drive root with a lowercase savedclips, one Saved event, one RecentClips file and encrypted clips.
    fn drive(d: &Path) -> PathBuf {
        let tc = d.join("usb/TeslaCam");
        put(tc.join("savedclips/2026-03-14_15-31-00/2026-03-14_15-30-00-front.mp4"), &[7; 3_000_000]);
        put(tc.join("savedclips/2026-03-14_15-31-00/event.json"), br#"{"city":"Berlin"}"#);
        put(tc.join("RecentClips/2026-03-13_10-00-00-back.mp4"), b"recent");
        put(tc.join("EncryptedClips/x.bin"), b"locked");
        d.join("usb")
    }

    fn import(src: &Path, lib: &Path, clear: bool) -> Result<Summary, String> {
        run(&Job { src: src.to_path_buf(), lib: lib.to_path_buf(), clear }, &NO, &Progress::default())
    }

    #[test]
    fn copies_tesla_layout_and_skips_what_is_there() {
        let d = temp("copy");
        let (usb, lib) = (drive(&d), d.join("lib"));
        let p = Progress::default();
        let s = run(&Job { src: usb.clone(), lib: lib.clone(), clear: false }, &NO, &p).unwrap();
        assert_eq!((s.copied, s.already, s.cleared), (3, 0, 0));
        assert_eq!(s.notes.len(), 1, "says what stayed behind: {:?}", s.notes);
        let total = 3_000_000 + 17 + 6;
        assert_eq!((p.files.load(Ordering::Relaxed), p.bytes.load(Ordering::Relaxed), p.total_bytes.load(Ordering::Relaxed)), (3, total, total));
        // Tesla's names whatever the drive's case, readable as a library, and nothing half-written.
        assert_eq!(fs::read(lib.join("SavedClips/2026-03-14_15-31-00/2026-03-14_15-30-00-front.mp4")).unwrap(), vec![7; 3_000_000]);
        let kinds: Vec<Kind> = scan(&lib).unwrap().iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&Kind::Saved) && kinds.contains(&Kind::Recent), "{kinds:?}");
        assert!(!lib.join("EncryptedClips").exists() && !lib.join("SavedClips/2026-03-14_15-31-00/event.json.part").exists());
        assert!(usb.join("TeslaCam/RecentClips/2026-03-13_10-00-00-back.mp4").exists(), "the drive is untouched");

        let again = import(&usb, &lib, false).unwrap();
        assert_eq!((again.copied, again.already), (0, 3));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn clearing_only_takes_files_whose_copy_matches() {
        let d = temp("clear");
        let (usb, lib) = (drive(&d), d.join("lib"));
        // Same size, wrong content: an earlier copy that went bad must not justify deleting the original.
        put(lib.join("RecentClips/2026-03-13_10-00-00-back.mp4"), b"wrong!");
        let s = import(&usb, &lib, true).unwrap();
        assert_eq!((s.copied, s.already, s.cleared), (3, 0, 3));
        assert_eq!(fs::read(lib.join("RecentClips/2026-03-13_10-00-00-back.mp4")).unwrap(), b"recent");
        let tc = usb.join("TeslaCam");
        assert!(!tc.join("savedclips/2026-03-14_15-31-00").exists(), "the emptied event folder goes");
        assert!(tc.join("savedclips").is_dir() && tc.join("RecentClips").is_dir(), "the car's folders stay");
        assert_eq!(fs::read(tc.join("EncryptedClips/x.bin")).unwrap(), b"locked");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn refuses_bad_places_and_cancels_cleanly() {
        let d = temp("bad");
        let usb = drive(&d);
        assert!(import(&d.join("nothing-here"), &d.join("lib"), true).is_err());
        assert!(import(&usb, &usb.join("TeslaCam/lib"), true).is_err(), "library inside the drive");
        assert!(import(&usb, &d, true).is_err(), "drive inside the library");
        let stop = AtomicBool::new(true);
        let r = run(&Job { src: usb.clone(), lib: d.join("lib"), clear: true }, &stop, &Progress::default());
        assert_eq!(r, Err(CANCELLED.to_owned()));
        assert!(usb.join("TeslaCam/savedclips/2026-03-14_15-31-00/event.json").exists(), "nothing cleared");
        let first = d.join("lib/SavedClips/2026-03-14_15-31-00/2026-03-14_15-30-00-front.mp4");
        assert!(!first.exists() && !first.with_added_extension("part").exists(), "nothing half-written");
        fs::remove_dir_all(&d).unwrap();
    }
}
