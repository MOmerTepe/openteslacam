//! OpenTeslaCam Setup. The release build carries the app appended to its own exe (scripts/build-installer.ps1),
//! and installs the bare exe as uninstall.exe. See USAGE for the unattended flags.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod install;
#[allow(dead_code)]
#[path = "../../src/ui/theme.rs"]
mod theme;
mod ui;

use install::{Event, Options, Out};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const USAGE: &str = "usage:
  setup                                     open the installer
  setup --silent --dir <folder> [--desktop] [--tesla]
  setup --silent                            update the installed app where it is
  uninstall --uninstall [--silent]";

const MAGIC: &[u8; 8] = b"OTCSETUP";

/// Splits `[stub][app][u64 little-endian app length][magic]` into stub and app.
fn split_payload(exe: &[u8]) -> Option<(&[u8], &[u8])> {
    let (rest, magic) = exe.split_at_checked(exe.len().checked_sub(8)?)?;
    if magic != MAGIC {
        return None;
    }
    let (rest, len) = rest.split_at_checked(rest.len().checked_sub(8)?)?;
    let len = usize::try_from(u64::from_le_bytes(len.try_into().ok()?)).ok()?;
    let (stub, app) = rest.split_at_checked(rest.len().checked_sub(len)?)?;
    // Both must look like Windows programs.
    (stub.starts_with(b"MZ") && app.starts_with(b"MZ")).then_some((stub, app))
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let exe = std::env::current_exe().unwrap_or_default();
    let mode = if has("--uninstall") {
        let dir = exe.parent().map(PathBuf::from).unwrap_or_default();
        if has("--silent") {
            attach_console();
            let code = silent(move |out| install::run_uninstall(&dir, out));
            install::delete_self_if_uninstalled();
            return code;
        }
        ui::Mode::Uninstall(dir)
    } else {
        let bytes = std::fs::read(&exe).unwrap_or_default();
        let payload = split_payload(&bytes).map(|(stub, app)| (Arc::new(stub.to_vec()), Arc::new(app.to_vec())));
        if has("--silent") {
            attach_console();
            return unattended(&args, payload);
        }
        ui::Mode::Install(payload)
    };
    let result = ui::run(mode);
    install::delete_self_if_uninstalled();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            ui::message_box(&format!("OpenTeslaCam Setup couldn't open its window.\n\n{e}"));
            ExitCode::FAILURE
        }
    }
}

fn unattended(args: &[OsString], payload: ui::Payload) -> ExitCode {
    // Without --dir, an existing install is updated where it is.
    let value = args.iter().position(|a| a == "--dir").and_then(|i| args.get(i + 1)).map(|d| d.to_string_lossy().into_owned());
    let value = value.or_else(|| install::installed().map(|i| i.dir.display().to_string()));
    let (Some(typed), Some((stub, app))) = (value, payload) else {
        eprintln!("{USAGE}\n(this exe needs --dir when nothing is installed yet, and an app appended by scripts/build-installer.ps1)");
        return ExitCode::FAILURE;
    };
    let dir = match install::validate_dir(&typed) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let checks = match install::detect(&dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("detection failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    for c in &checks {
        println!("{:<8} {:<8} {}", c.id, c.status, c.detail);
    }
    if install::find(&checks, "windows").is_some_and(|c| !c.ok()) {
        return ExitCode::FAILURE;
    }
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let options = Options {
        desktop: has("--desktop"),
        tesla: has("--tesla"),
        git: install::found_path(&checks, "git"),
        winget: install::found_path(&checks, "winget"),
        dir,
        app,
        stub,
    };
    silent(move |out| install::run_install(options, out))
}

/// Runs the steps with their progress on stdout; the exit code says whether any failed.
fn silent(run: impl FnOnce(Out)) -> ExitCode {
    let failed = Arc::new(AtomicBool::new(false));
    let seen = failed.clone();
    run(Arc::new(move |e| match e {
        Event::Start(t) => println!("start    {}", t.label()),
        Event::Done(t) => println!("done     {}", t.label()),
        Event::Skipped(t, why) => println!("skipped  {}: {why}", t.label()),
        Event::Failed(t, why) => {
            seen.store(true, Ordering::Relaxed);
            println!("FAILED   {}: {why}", t.label());
        }
        Event::Log(l) => println!("         {l}"),
        Event::Finished => {}
    }));
    if failed.load(Ordering::Relaxed) { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// A windows-subsystem program has no console; borrow the caller's so `--silent` can print there.
fn attach_console() {
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    // ATTACH_PARENT_PROCESS. Failing is fine: redirected output doesn't need a console.
    unsafe {
        AttachConsole(u32::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(stub: &[u8], app: &[u8], len: u64, magic: &[u8]) -> Vec<u8> {
        [stub, app, &len.to_le_bytes(), magic].concat()
    }

    #[test]
    fn finds_the_appended_app() {
        let exe = bundle(b"MZstub", b"MZapp", 5, MAGIC);
        assert_eq!(split_payload(&exe), Some((&b"MZstub"[..], &b"MZapp"[..])));
    }

    #[test]
    fn rejects_anything_else() {
        assert_eq!(split_payload(b"MZ a bare stub with no payload at all"), None);
        assert_eq!(split_payload(b""), None);
        assert_eq!(split_payload(MAGIC), None);
        assert_eq!(split_payload(&bundle(b"MZstub", b"MZapp", 5, b"NOTMAGIC")), None);
        assert_eq!(split_payload(&bundle(b"MZstub", b"MZapp", 500, MAGIC)), None, "length past the start");
        assert_eq!(split_payload(&bundle(b"MZstub", b"MZapp", u64::MAX, MAGIC)), None);
        assert_eq!(split_payload(&bundle(b"MZstub", b"app!!", 5, MAGIC)), None, "app isn't an exe");
        assert_eq!(split_payload(&bundle(b"stub", b"MZapp", 5, MAGIC)), None, "stub isn't an exe");
    }
}
