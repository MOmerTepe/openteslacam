mod bench;
mod decode;
mod fixture;
// The UI (next step) uses the rest of these APIs.
#[allow(dead_code)]
mod library;
// Phase 0 scaffolding: later phases call into these modules.
#[allow(dead_code)]
mod mp4;
#[allow(dead_code)]
mod mux;
#[allow(dead_code)]
mod player;
#[allow(dead_code)]
mod sei;
mod view;

use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage:
  openteslacam --dump-sei <clip.mp4>         print SEI telemetry as CSV (same CSV as Tesla's sei_extractor.py)
  openteslacam --view <clip.mp4>             decode one camera into a window
  openteslacam --scan <folder>               list the events found in a TeslaCam folder
  openteslacam --bench-decode <folder|clip.mp4>...   decode up to 6 clips at once and report speed
  openteslacam --bench-synthetic             same benchmark on generated HW3/HW4-sized clips";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.first().map(String::as_str) {
        Some("--dump-sei") if args.len() == 2 => dump_sei(Path::new(&args[1])),
        Some("--view") if args.len() == 2 => view::view(Path::new(&args[1])),
        Some("--bench-decode") if args.len() >= 2 => bench::bench_decode(&args[1..]),
        Some("--bench-synthetic") if args.len() == 1 => bench::bench_synthetic(),
        Some("--scan") if args.len() == 2 => scan(Path::new(&args[1])),
        // Hidden: writes a synthetic TeslaCam tree (20 s segments, 640x480) for testing the viewer.
        Some("--make-fixture") if args.len() == 2 => fixture::make_fixture(Path::new(&args[1]), 20, 640, 480),
        // Phase 1: no arguments will open the GUI here.
        None => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
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
