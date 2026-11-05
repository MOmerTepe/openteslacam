//! Diffs `openteslacam --dump-sei` against Tesla's own `sei_extractor.py` (vendor-dev/, dev only).

// The binary crate's modules can't be imported, so the muxer source is compiled in directly.
#[path = "../src/mux.rs"]
mod mux;

use std::path::{Path, PathBuf};
use std::process::Command;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// (venv python, vendor-dev/dashcam) or None with the skip reason printed.
fn python() -> Option<(PathBuf, PathBuf)> {
    let root = Path::new(ROOT).join("vendor-dev");
    let py = ["Scripts/python.exe", "bin/python"].iter().map(|p| root.join(".venv").join(p)).find(|p| p.exists());
    let dir = root.join("dashcam");
    match py {
        Some(py) if dir.join("sei_extractor.py").exists() => Some((py, dir)),
        _ => {
            println!("skipped: vendor-dev/.venv python or vendor-dev/dashcam/sei_extractor.py missing");
            None
        }
    }
}

/// (success, stdout, stderr)
fn run(cmd: &mut Command) -> (bool, String, String) {
    let out = cmd.output().expect("failed to start process");
    let text = |b: &[u8]| String::from_utf8_lossy(b).replace("\r\n", "\n");
    (out.status.success(), text(&out.stdout), text(&out.stderr))
}

const HEADER: &str = "version,gear_state,";

/// Returns our output after asserting it matches Python's: identical CSV, or no rows from
/// either tool (the no-SEI messages are worded differently on purpose).
///
/// Known divergence: sei_extractor.py scans the first mdat to EOF, so it still prints rows for a
/// recording cut short before its moov was written. We read the sample table, which such a file
/// lacks, and reject it as truncated; that case is reported, not failed.
fn compare(clip: &Path, py: &Path, dir: &Path) -> String {
    let (py_ok, want, py_err) = run(Command::new(py).arg("sei_extractor.py").arg(clip).current_dir(dir));
    let (ok, got, err) = run(Command::new(env!("CARGO_BIN_EXE_openteslacam")).arg("--dump-sei").arg(clip));
    if py_ok && !ok && err.contains("truncated") {
        println!("known divergence, rejected as truncated: {}", clip.display());
        return got;
    }
    assert_eq!(ok, py_ok, "{}: exit status differs\n  ours: {err}\n  python: {py_err}", clip.display());
    if py_ok && !want.starts_with(HEADER) {
        assert!(!got.starts_with(HEADER), "{}: python found no SEI, we did", clip.display());
        return got;
    }
    if py_ok && got != want {
        let (g, w): (Vec<_>, Vec<_>) = (got.lines().collect(), want.lines().collect());
        let i = (0..g.len().max(w.len())).find(|&i| g.get(i) != w.get(i)).unwrap_or(0);
        panic!(
            "{}: first difference at line {}\n  ours:   {:?}\n  python: {:?}\n  ({} vs {} lines)",
            clip.display(),
            i + 1,
            g.get(i),
            w.get(i),
            g.len(),
            w.len()
        );
    }
    got
}

fn mp4s(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            mp4s(&p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")) {
            out.push(p);
        }
    }
}

#[test]
fn real_clips_match_python() {
    let mut clips = Vec::new();
    mp4s(&Path::new(ROOT).join("testdata"), &mut clips);
    if clips.is_empty() {
        println!("skipped: no clips in testdata/");
        return;
    }
    let Some((py, dir)) = python() else { return };
    for c in &clips {
        compare(c, &py, &dir);
        println!("ok: {}", c.display());
    }
}

// --- synthetic clip -------------------------------------------------------------------------

fn varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
    out
}

fn u(field: u32, v: u64) -> Vec<u8> {
    [varint((field << 3) as u64), varint(v)].concat()
}

fn f(field: u32, v: f32) -> Vec<u8> {
    [varint((field << 3 | 5) as u64), v.to_le_bytes().to_vec()].concat()
}

fn d(field: u32, v: f64) -> Vec<u8> {
    [varint((field << 3 | 1) as u64), v.to_le_bytes().to_vec()].concat()
}

/// What an H.264 encoder does: 0x03 after 00 00 whenever the next byte is 00..03.
fn add_epb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Tesla's wrapper: SEI user_data_unregistered, a size byte, `n42` x 0x42, 0x69, payload, trailing byte.
fn sei(n42: usize, proto: &[u8]) -> Vec<u8> {
    let body = add_epb(proto);
    [&[0x06, 0x05, body.len() as u8][..], &vec![0x42; n42], &[0x69], &body, &[0x80]].concat()
}

fn rng(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// One full message per drive frame, roughly what a car sends while driving.
fn drive_frame(i: u64, s: &mut u64) -> Vec<u8> {
    let t = i as f64 / 36.0;
    let noise = (rng(s) % 1000) as f64 / 1000.0;
    [
        u(1, 1),
        u(2, [1, 1, 1, 0, 2, 3][(i / 60) as usize % 6]),
        u(3, 1000 + i),
        f(4, (t * 2.5 + noise) as f32),
        f(5, ((t * 0.7).sin() * 50.0 + 50.0) as f32),
        f(6, ((t * 0.3).sin() * 120.0) as f32),
        u(7, (i / 13) % 2),
        u(8, (i / 29) % 3 / 2),
        u(9, (i % 50 > 40) as u64),
        u(10, (i / 90) % 4),
        d(11, 37.774929 + t * 1.1e-5),
        d(12, -122.419416 - t * 0.9e-5),
        d(13, (t * 7.0) % 360.0),
        d(14, noise - 0.5),
        d(15, (t * 1.3).cos() * 0.2),
        d(16, 9.81 + noise * 0.01),
    ]
    .concat()
}

/// Edge-case messages: defaults, every enum value (plus unknown ones), awkward floats, huge ints.
fn edge_messages() -> Vec<Vec<u8>> {
    let mut m = vec![u(1, 0)]; // decodes to all defaults: a row of empty cells
    for g in [0, 1, 2, 3, 7, u64::MAX /* -1 */] {
        m.push([u(1, 1), u(2, g)].concat());
    }
    for a in [0, 1, 2, 3, 99] {
        m.push([u(1, 1), u(10, a)].concat());
    }
    let f32s = [-12.5, f32::MAX, f32::MIN, 1e-30, 1e-40, f32::from_bits(1), -0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.1, 123456789.0];
    for (i, &v) in f32s.iter().enumerate() {
        m.push([f(4, v), f(5, -v), f(6, v * 0.5), u(3, i as u64)].concat());
    }
    let f64s = [
        -0.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MAX,
        f64::MIN_POSITIVE,
        5e-324,
        1e16,
        1e-5,
        0.1 + 0.2,
        2.0, // little-endian 00 00 00 00 00 00 00 40: needs emulation prevention
        f64::from_bits(0x4010_0000_0300_0000), // contains 00 00 03 itself
        -942405961250223.0 - 0.25, // tie between two 16-digit candidates
    ];
    for &v in &f64s {
        m.push([d(11, v), d(12, -v), d(13, v), d(14, v * 3.0), d(15, v / 7.0), d(16, v)].concat());
    }
    m.push([u(1, u32::MAX as u64), u(3, u64::MAX), u(7, 1), u(8, 1), u(9, 1)].concat());
    m.push([u(1, 1), u(3, 5), u(3, 6)].concat()); // repeated field: last wins
    // Random bit patterns hit every float formatting branch.
    let mut s = 0x1234_5678_9ABC_DEF1;
    for _ in 0..100 {
        let r = rng(&mut s);
        m.push([f(4, f32::from_bits(r as u32)), f(6, f32::from_bits((r >> 32) as u32)), d(11, f64::from_bits(rng(&mut s)))].concat());
    }
    m
}

#[test]
fn synthetic_clip_matches_python() {
    let Some((py, dir)) = python() else { return };
    let filler = |i: usize| [&[0x41u8, 0x9A][..], &(i as u32).to_be_bytes(), &[0, 0, 1, 0x80]].concat();
    let mut samples: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut s = 42;
    for (i, msg) in edge_messages().into_iter().enumerate() {
        samples.push(vec![sei(i % 5, &msg), filler(i)]);
    }
    for i in 0..300 {
        samples.push(vec![filler(i as usize), sei(3, &drive_frame(i, &mut s))]);
    }
    // Both tools must skip these: malformed protobuf, other SEI types, a broken wrapper, tiny NALs.
    samples.push(vec![
        sei(2, &[0x0F]),                  // wire type 7
        sei(2, &[0x0A, 0x05, 0x01]),      // length past the end
        sei(2, &[0x08]),                  // truncated varint
        sei(2, &[0x00]),                  // field 0
        vec![0x06, 0x04, 0x10, 0x42, 0x69, 0x08, 0x01, 0x80], // SEI payload type 4
        vec![0x06, 0x05, 0x10, 0x42, 0x43, 0x08, 0x01, 0x80], // no 0x69 after the 0x42 run
        vec![0x06, 0x05, 0x10, 0x69, 0x80],                   // empty payload
        vec![0x09],
        vec![],
        filler(0),
    ]);
    // A message after the junk in the same sample still counts.
    samples.push(vec![sei(1, &[0x0F]), sei(1, &drive_frame(9999, &mut s))]);
    assert!(
        edge_messages().iter().any(|m| add_epb(m).len() > m.len()),
        "fixture must contain payloads that need emulation prevention"
    );

    let path = std::env::temp_dir().join(format!("otc-oracle-{}.mp4", std::process::id()));
    let cfg = [1, 0x64, 0, 0x28, 0xFF, 0xE1, 0, 2, 0x67, 0x64, 1, 0, 2, 0x68, 0xEB];
    let mut m = mux::Muxer::create(&path, 36000, 1280, 960, &cfg).unwrap();
    for (i, nals) in samples.iter().enumerate() {
        let data: Vec<u8> = nals.iter().flat_map(|n| [&(n.len() as u32).to_be_bytes()[..], n].concat()).collect();
        m.push(&data, 1000, 0, i % 36 == 0).unwrap();
    }
    m.finish().unwrap();
    let got = compare(&path, &py, &dir);
    std::fs::remove_file(&path).unwrap();
    // Header plus one row per valid message; the junk sample adds none.
    assert_eq!(got.lines().count(), 1 + edge_messages().len() + 300 + 1);
}

#[test]
fn truncated_recording_is_known_divergence() {
    let Some((py, dir)) = python() else { return };
    let path = std::env::temp_dir().join(format!("otc-oracle-trunc-{}.mp4", std::process::id()));
    // ftyp, then an mdat whose header claims 5000 more bytes than exist, and no moov.
    let nal = [0, 0, 0, 10, 0x06, 0x05, 0x0A, 0x42, 0x69, 0x08, 0x05, 0x18, 0x07, 0x80];
    let body = nal.repeat(3);
    let mdat = [&(8 + body.len() as u32 + 5000).to_be_bytes()[..], b"mdat", &body].concat();
    std::fs::write(&path, [&[0, 0, 0, 16][..], b"ftypisom\0\0\x02\0", &mdat].concat()).unwrap();
    let (py_ok, want, _) = run(Command::new(&py).arg("sei_extractor.py").arg(&path).current_dir(&dir));
    let (ok, _, err) = run(Command::new(env!("CARGO_BIN_EXE_openteslacam")).arg("--dump-sei").arg(&path));
    compare(&path, &py, &dir);
    std::fs::remove_file(&path).unwrap();
    assert!(py_ok && want.lines().count() == 4, "python: {want}");
    assert!(!ok && err.contains("truncated"), "ours: {err}");
}

#[test]
fn no_sei_both_print_no_rows() {
    let Some((py, dir)) = python() else { return };
    let path = std::env::temp_dir().join(format!("otc-oracle-nosei-{}.mp4", std::process::id()));
    let cfg = [1, 0x64, 0, 0x28, 0xFF, 0xE1, 0, 2, 0x67, 0x64, 1, 0, 2, 0x68, 0xEB];
    let mut m = mux::Muxer::create(&path, 36000, 64, 48, &cfg).unwrap();
    m.push(&[0, 0, 0, 3, 0x65, 0x88, 0x80], 1000, 0, true).unwrap();
    m.finish().unwrap();
    let got = compare(&path, &py, &dir);
    std::fs::remove_file(&path).unwrap();
    assert!(got.starts_with("no SEI telemetry in "), "{got}");
}
