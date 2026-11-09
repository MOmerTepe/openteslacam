//! Phase 0 go/no-go: can openh264 decode 6 camera streams at 1x and 4x on this machine?

use crate::decode::{CamDecoder, to_rgba};
use crate::fixture::{FPS, encode_clip, rng};
use crate::library::CAMERAS;
use crate::mp4;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

struct Stats {
    name: String,
    w: u32,
    h: u32,
    frames: usize,
    samples: usize,
    video_s: f64,
    bytes: u64,
    wall: Duration,
    rgba: Duration,
}

/// Decodes a whole clip like a camera thread of the app would (read, decode, convert to RGBA).
fn run_stream(path: &Path) -> Result<Stats, String> {
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let track = mp4::read_track(path).map_err(|e| err(&e))?;
    let mut f = File::open(path).map_err(|e| err(&e))?;
    let mut dec = CamDecoder::new(&track)?;
    let (mut buf, mut frames, mut rgba) = (Vec::new(), 0, Duration::ZERO);
    let mut convert = |yuv: &openh264::decoder::DecodedYUV| {
        let t = Instant::now();
        to_rgba(yuv, &mut buf);
        rgba += t.elapsed();
        frames += 1;
    };
    let start = Instant::now();
    for s in &track.samples {
        let data = mp4::read_sample(&mut f, s).map_err(|e| err(&e))?;
        if let Some(yuv) = dec.decode(&data, s.key).map_err(|e| err(&e))? {
            convert(&yuv);
        }
    }
    for yuv in dec.flush().map_err(|e| err(&e))? {
        convert(&yuv);
    }
    let wall = start.elapsed();
    Ok(Stats {
        name: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        w: track.width,
        h: track.height,
        frames,
        samples: track.samples.len(),
        video_s: track.duration as f64 / track.timescale.max(1) as f64,
        bytes: track.samples.iter().map(|s| s.size as u64).sum(),
        wall,
        rgba,
    })
}

/// Runs all clips at once, one thread each, prints a table and the 1x/4x verdict.
fn run(paths: &[PathBuf]) -> Result<(), String> {
    let handles: Vec<_> = paths.iter().cloned().map(|p| thread::spawn(move || run_stream(&p))).collect();
    let stats: Vec<Stats> = handles.into_iter().map(|h| h.join().map_err(|_| "decoder thread panicked".to_string())?).collect::<Result<_, _>>()?;
    println!(
        "{:<34} {:>9} {:>9} {:>7} {:>6} {:>7} {:>8} {:>9} {:>9}",
        "stream", "res", "frames", "video s", "Mbit/s", "wall s", "fps", "rgba ms/f", "realtime"
    );
    for s in &stats {
        let wall = s.wall.as_secs_f64();
        println!(
            "{:<34} {:>9} {:>9} {:>7.2} {:>6.1} {:>7.2} {:>8.1} {:>9.2} {:>8.2}x",
            s.name,
            format!("{}x{}", s.w, s.h),
            format!("{}/{}", s.frames, s.samples),
            s.video_s,
            s.bytes as f64 * 8e-6 / s.video_s,
            wall,
            s.frames as f64 / wall,
            s.rgba.as_secs_f64() * 1000.0 / s.frames.max(1) as f64,
            s.video_s / wall,
        );
    }
    let min = stats.iter().map(|s| s.video_s / s.wall.as_secs_f64()).fold(f64::INFINITY, f64::min);
    let verdict = |x: f64| if min >= x { "PASS" } else { "FAIL" };
    println!("slowest stream {min:.2}x realtime -> 1x: {}, 4x: {}", verdict(1.0), verdict(4.0));
    Ok(())
}

/// `--bench-decode`: each argument is a clip or a folder of clips; at most 6 run (like 6 cameras).
pub fn bench_decode(args: &[String]) -> Result<(), String> {
    let mut paths = Vec::new();
    for a in args {
        let p = PathBuf::from(a);
        if p.is_dir() {
            let rd = std::fs::read_dir(&p).map_err(|e| format!("{a}: {e}"))?;
            let mut clips: Vec<_> = rd
                .filter_map(|e| Some(e.ok()?.path()))
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")))
                .collect();
            clips.sort();
            paths.extend(clips);
        } else {
            paths.push(p);
        }
    }
    if paths.is_empty() {
        return Err("no .mp4 clips found".into());
    }
    if paths.len() > 6 {
        println!("using the first 6 of {} clips", paths.len());
        paths.truncate(6);
    }
    print_machine();
    run(&paths)
}

fn print_machine() {
    let cpus = thread::available_parallelism().map_or(0, |n| n.get());
    println!("{cpus} hardware threads; one decoder thread per stream, all streams at once");
}

const SECONDS: u32 = 10;

/// `--bench-synthetic`: encodes HW3/HW4-sized clips with openh264, then benchmarks them.
pub fn bench_synthetic() -> Result<(), String> {
    println!("SYNTHETIC CLIPS: numbers are indicative only.");
    println!(
        "openh264 encodes Constrained Baseline (CAVLC, no B-frames); real Tesla clips are likely \
         High profile with CABAC, which costs more to decode. Target bitrates (5 Mbit/s, HW4 front \
         12) are guesses that openh264 only loosely follows; Mbit/s shows what it produced. Sizes \
         are used as-is (all even, which is all the encoder needs)."
    );
    print_machine();
    // (name, width, height, target bits/s)
    let hw4 = CAMERAS.map(|n| if n == "front" { (n, 2896, 1876, 12_000_000) } else { (n, 1448, 938, 5_000_000) });
    let sets = [
        ("HW3 (6 x 1280x960)", CAMERAS.map(|n| (n, 1280, 960, 5_000_000))),
        ("HW4 (front 2896x1876 + 5 x 1448x938)", hw4),
    ];
    for (set, cams) in sets {
        println!("\n== {set}: encoding {SECONDS} s at {FPS} fps ...");
        let t = Instant::now();
        let handles: Vec<_> = cams
            .iter()
            .enumerate()
            .map(|(i, &(name, w, h, bps))| {
                let tag: String = set.chars().take(3).collect();
                let path = std::env::temp_dir().join(format!("otc-bench-{tag}-{i}-{name}.mp4"));
                thread::spawn(move || synth_clip(&path, w, h, bps, i as u64 + 1).map(|_| path))
            })
            .collect();
        let paths = handles
            .into_iter()
            .map(|h| h.join().map_err(|_| "encoder thread panicked".to_string())?)
            .collect::<Result<Vec<_>, _>>()?;
        println!("encoded in {:.1} s", t.elapsed().as_secs_f64());
        let res = run(&paths);
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
        res?;
    }
    Ok(())
}

/// A textured scene panning sideways (so motion search has real work) plus per-frame sensor noise
/// (so it doesn't compress to nothing), encoded and muxed to `path`.
fn synth_clip(path: &Path, w: usize, h: usize, bps: u32, seed: u64) -> Result<(), String> {
    const PAN: usize = 3; // pixels per frame
    let frames = (FPS * SECONDS) as usize;
    let tw = w + frames * PAN;
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    // 8x8 random blocks over a fine diagonal pattern: edges at many scales.
    let blocks: Vec<u8> = (0..tw.div_ceil(8) * h.div_ceil(8)).map(|_| rng(&mut s) as u8).collect();
    let bw = tw.div_ceil(8);
    let tex: Vec<u8> =
        (0..tw * h).map(|i| (blocks[(i / tw / 8) * bw + (i % tw) / 8] / 2).wrapping_add((((i % tw) ^ (i / tw)) & 63) as u8)).collect();
    encode_clip(path, w, h, bps, frames, |t, yuv| {
        let off = t * PAN;
        for y in 0..h {
            let row = &tex[y * tw + off..y * tw + off + w];
            yuv.extend(row.iter().map(|&p| p.wrapping_add((rng(&mut s) & 7) as u8)));
        }
        for base in [96u8, 160] {
            for y in 0..h / 2 {
                yuv.extend((0..w / 2).map(|x| base.wrapping_add(tex[2 * y * tw + off + 2 * x] / 4)));
            }
        }
        None
    })
}
