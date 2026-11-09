// Release builds open no console window; CLI flags borrow the caller's console instead (see attach_console).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod bench;
mod decode;
mod export;
mod fixture;
mod library;
// Phase 0 scaffolding: later phases call into these modules.
#[allow(dead_code)]
mod mp4;
#[allow(dead_code)]
mod mux;
mod player;
#[allow(dead_code)]
mod sei;
mod telemetry;
mod ui;

use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage:
  openteslacam [folder]                      open the viewer (on a TeslaCam folder)
  openteslacam --version                     print the version
  openteslacam --dump-sei <clip.mp4>         print SEI telemetry as CSV (same CSV as Tesla's sei_extractor.py)
  openteslacam --scan <folder>               list the events found in a TeslaCam folder
  openteslacam --bench-decode <folder|clip.mp4>...   decode up to 6 clips at once and report speed
  openteslacam --bench-synthetic             same benchmark on generated HW3/HW4-sized clips";

fn main() -> ExitCode {
    // Not `args()`: it panics on a Windows path that isn't valid Unicode.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if !args.is_empty() {
        attach_console();
    }
    let res = match args.first().and_then(|a| a.to_str()) {
        Some("--version") if args.len() == 1 => {
            println!("openteslacam {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--dump-sei") if args.len() == 2 => dump_sei(Path::new(&args[1])),
        Some("--bench-decode") if args.len() >= 2 => bench::bench_decode(&args[1..]),
        Some("--bench-synthetic") if args.len() == 1 => bench::bench_synthetic(),
        Some("--scan") if args.len() == 2 => scan(Path::new(&args[1])),
        // Hidden: writes a synthetic TeslaCam tree (20 s segments, 640x480) for testing the viewer.
        Some("--make-fixture") if args.len() == 2 => fixture::make_fixture(Path::new(&args[1]), 20, 640, 480),
        _ if args.is_empty() => ui::run(None),
        _ if args.len() == 1 && Path::new(&args[0]).is_dir() => ui::run(Some(PathBuf::from(&args[0]))),
        _ => Err(USAGE.to_string()),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// With the windows subsystem there's no console to print to; use the one this was run from, if any.
fn attach_console() {
    #[cfg(all(windows, not(debug_assertions)))]
    {
        unsafe extern "system" {
            fn AttachConsole(process_id: u32) -> i32;
        }
        // ATTACH_PARENT_PROCESS. Failing is fine: redirected output doesn't need a console.
        unsafe {
            AttachConsole(u32::MAX);
        }
    }
}

fn dump_sei(path: &Path) -> Result<(), String> {
    let (_, frames) = sei::read_telemetry(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = BufWriter::new(io::stdout().lock());
    let write = |out: &mut BufWriter<_>| -> io::Result<()> {
        if frames.is_empty() {
            writeln!(
                out,
                "no SEI telemetry in {} (it needs firmware 2025.44.25+ on HW3 or newer, and parked cars may record none)",
                path.display()
            )?;
        } else {
            writeln!(out, "{}", sei::csv_header())?;
            for f in &frames {
                writeln!(out, "{}", sei::csv_row(&f.t))?;
            }
        }
        out.flush()
    };
    write(&mut out).map_err(|e| format!("stdout: {e}"))
}

fn scan(dir: &Path) -> Result<(), String> {
    let events = library::scan(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for e in &events {
        let kind = format!("{:?}", e.kind).to_lowercase();
        let enc = if e.encrypted { "  encrypted" } else { "" };
        let info = e.info.as_ref().map(|i| format!("  {} {}", i.reason, i.city)).unwrap_or_default();
        println!("{kind:<7} {:<22} {:>3} segments{enc}{info}", e.name, e.segments.len());
    }
    println!("{} events", events.len());
    Ok(())
}
