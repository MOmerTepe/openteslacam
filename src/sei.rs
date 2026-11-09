//! Tesla SEI telemetry: finds the `SeiMetadata` protobuf in H.264 SEI NALs and decodes it by hand.

use crate::mp4::{self, Mp4Error, Track};
use std::fs::File;
use std::path::Path;

pub const GEAR_PARK: i32 = 0;
pub const GEAR_DRIVE: i32 = 1;
pub const GEAR_REVERSE: i32 = 2;
pub const GEAR_NEUTRAL: i32 = 3;

pub const AP_NONE: i32 = 0;
pub const AP_FSD: i32 = 1;
pub const AP_AUTOSTEER: i32 = 2;
pub const AP_TACC: i32 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Telemetry {
    pub version: u32,
    pub gear: i32,
    pub frame_seq_no: u64,
    pub speed_mps: f32,
    pub accel_pct: f32,
    pub steering_deg: f32,
    pub blink_left: bool,
    pub blink_right: bool,
    pub brake: bool,
    pub autopilot: i32,
    pub lat: f64,
    pub lon: f64,
    pub heading: f64,
    pub accel_x: f64,
    pub accel_y: f64,
    pub accel_z: f64,
}

pub struct Frame {
    /// Presentation time, in track timescale, of the sample carrying the SEI.
    pub pts: u64,
    pub t: Telemetry,
}

pub fn gear_letter(g: i32) -> &'static str {
    match g {
        GEAR_PARK => "P",
        GEAR_DRIVE => "D",
        GEAR_REVERSE => "R",
        GEAR_NEUTRAL => "N",
        _ => "?",
    }
}

pub fn ap_name(a: i32) -> &'static str {
    match a {
        AP_NONE => "none",
        AP_FSD => "fsd",
        AP_AUTOSTEER => "autosteer",
        AP_TACC => "tacc",
        _ => "?",
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    /// Up to 10 bytes; bits past 64 are dropped, as upb does.
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for i in 0..10 {
            let b = *self.take(1)?.first()?;
            v |= ((b & 0x7F) as u64).wrapping_shl(7 * i);
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    /// Tags and lengths: at most 5 bytes and must fit in u32 (upb rejects the rest).
    fn varint32(&mut self) -> Option<u32> {
        let start = self.pos;
        let v = self.varint()?;
        (self.pos - start <= 5 && v <= u32::MAX as u64).then_some(v as u32)
    }
}

/// Decodes a proto3 `SeiMetadata`; `None` where protobuf would raise `DecodeError`.
pub fn decode_metadata(buf: &[u8]) -> Option<Telemetry> {
    let mut t = Telemetry::default();
    let mut r = Reader { buf, pos: 0 };
    // Field numbers of open (unknown) groups; fields inside a group are not ours.
    let mut groups: Vec<u32> = Vec::new();
    while r.pos < buf.len() {
        let tag = r.varint32()?;
        let (field, wire) = (tag >> 3, tag & 7);
        let top = groups.is_empty();
        // upb only rejects field 0 outside groups.
        if field == 0 && top {
            return None;
        }
        // A known field sent with the wrong wire type is kept as unknown by protobuf, so it
        // falls through to the skip arms below.
        match wire {
            0 => {
                let v = r.varint()?;
                if top {
                    match field {
                        1 => t.version = v as u32,
                        2 => t.gear = v as i32,
                        3 => t.frame_seq_no = v,
                        7 => t.blink_left = v != 0,
                        8 => t.blink_right = v != 0,
                        9 => t.brake = v != 0,
                        10 => t.autopilot = v as i32,
                        _ => {}
                    }
                }
            }
            1 => {
                let v = f64::from_le_bytes(r.take(8)?.try_into().ok()?);
                if top {
                    match field {
                        11 => t.lat = v,
                        12 => t.lon = v,
                        13 => t.heading = v,
                        14 => t.accel_x = v,
                        15 => t.accel_y = v,
                        16 => t.accel_z = v,
                        _ => {}
                    }
                }
            }
            2 => {
                let n = r.varint32()?;
                r.take(n as usize)?;
            }
            3 => {
                // upb's nesting limit: 100 open groups parse, 101 fail.
                if groups.len() == 100 {
                    return None;
                }
                groups.push(field);
            }
            4 => {
                if groups.pop() != Some(field) {
                    return None;
                }
            }
            5 => {
                let v = f32::from_le_bytes(r.take(4)?.try_into().ok()?);
                if top {
                    match field {
                        4 => t.speed_mps = v,
                        5 => t.accel_pct = v,
                        6 => t.steering_deg = v,
                        _ => {}
                    }
                }
            }
            _ => return None,
        }
    }
    groups.is_empty().then_some(t)
}

/// Drops each 0x03 that follows two zero bytes; the zero count restarts after a dropped byte.
pub fn strip_epb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Inserts emulation-prevention bytes the way an H.264 encoder would (inverse of `strip_epb`).
pub fn add_epb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 64);
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

/// Proto3 encoding of `t` (zero fields omitted), for synthetic clips.
pub fn encode_metadata(t: &Telemetry) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push(v as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    let mut out = Vec::new();
    let ints = [(1, t.version as u64), (2, t.gear as i64 as u64), (3, t.frame_seq_no)];
    let flags = [(7, t.blink_left as u64), (8, t.blink_right as u64), (9, t.brake as u64), (10, t.autopilot as i64 as u64)];
    for (f, v) in ints.into_iter().chain(flags).filter(|&(_, v)| v != 0) {
        varint(&mut out, f << 3);
        varint(&mut out, v);
    }
    for (f, v) in [(4, t.speed_mps), (5, t.accel_pct), (6, t.steering_deg)].into_iter().filter(|&(_, v)| v != 0.0) {
        varint(&mut out, f << 3 | 5);
        out.extend(v.to_le_bytes());
    }
    let doubles = [(11, t.lat), (12, t.lon), (13, t.heading), (14, t.accel_x), (15, t.accel_y), (16, t.accel_z)];
    for (f, v) in doubles.into_iter().filter(|&(_, v)| v != 0.0) {
        varint(&mut out, f << 3 | 1);
        out.extend(v.to_le_bytes());
    }
    out
}

/// A user_data_unregistered SEI NAL in Tesla's wrapper (see `proto_payload`) around `proto`.
pub fn sei_nal(proto: &[u8]) -> Vec<u8> {
    // shortcut: one payload-size byte, so the protobuf must stay under 251 bytes (16 fields need ~110).
    assert!(proto.len() + 4 < 255, "SEI payload too long for a one-byte size");
    let body = [&[5, proto.len() as u8 + 4, 0x42, 0x42, 0x42, 0x69][..], proto].concat();
    [&[0x06][..], &add_epb(&body), &[0x80]].concat()
}

/// Tesla's wrapper: from byte 3, a run of 0x42 then 0x69; payload runs to the byte before the
/// last (the RBSP trailing byte).
fn proto_payload(nal: &[u8]) -> Option<Vec<u8>> {
    let i = (3..nal.len().saturating_sub(1)).find(|&i| nal[i] != 0x42)?;
    (nal[i] == 0x69).then(|| strip_epb(&nal[i + 1..nal.len() - 1]))
}

/// Protobuf payloads of the user_data_unregistered SEI NALs in one AVCC sample.
pub fn sei_payloads(sample: &[u8], nal_len: u8) -> Vec<Vec<u8>> {
    let n = nal_len as usize;
    let mut out = Vec::new();
    let mut pos = 0;
    while n > 0 && pos + n <= sample.len() {
        let len = sample[pos..pos + n].iter().fold(0usize, |a, &b| a << 8 | b as usize);
        pos += n;
        let Some(nal) = sample.get(pos..pos + len) else { break };
        pos += len;
        if nal.len() >= 2 && nal[0] & 0x1F == 6 && nal[1] == 5 {
            out.extend(proto_payload(nal).filter(|p| !p.is_empty()));
        }
    }
    out
}

pub fn read_telemetry(path: &Path) -> Result<(Track, Vec<Frame>), Mp4Error> {
    let track = mp4::read_track(path)?;
    let mut f = File::open(path)?;
    let mut frames = Vec::new();
    for s in &track.samples {
        let data = mp4::read_sample(&mut f, s)?;
        for p in sei_payloads(&data, track.nal_len) {
            if let Some(t) = decode_metadata(&p) {
                frames.push(Frame { pts: s.pts, t });
            }
        }
    }
    Ok((track, frames))
}

const FIELDS: [&str; 16] = [
    "version",
    "gear_state",
    "frame_seq_no",
    "vehicle_speed_mps",
    "accelerator_pedal_position",
    "steering_wheel_angle",
    "blinker_on_left",
    "blinker_on_right",
    "brake_applied",
    "autopilot_state",
    "latitude_deg",
    "longitude_deg",
    "heading_deg",
    "linear_acceleration_mps2_x",
    "linear_acceleration_mps2_y",
    "linear_acceleration_mps2_z",
];

pub fn csv_header() -> String {
    FIELDS.join(",")
}

/// Python's `repr(float)` (shortest round-trip digits), with JSON's names for non-finite values.
fn py_float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Infinity" } else { "Infinity" }.into();
    }
    let s = format!("{x:e}"); // "-1.5e16", "1e-7", "0e0"
    // Between two equally short candidates Rust may pick the wrong one; Python takes the nearest
    // (ties to even), which is the correctly rounded string of that length when it round-trips.
    let k = s.split('e').next().unwrap().trim_start_matches('-').replace('.', "").len();
    let near = format!("{:.*e}", k - 1, x);
    let s = if near.parse::<f64>() == Ok(x) { near } else { s };
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let (sign, mant) = mant.strip_prefix('-').map_or(("", mant), |m| ("-", m));
    if !(-4..16).contains(&exp) {
        let esign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{mant}e{esign}{:02}", exp.abs());
    }
    let digits = mant.replace('.', "");
    if exp < 0 {
        return format!("{sign}0.{}{digits}", "0".repeat((-exp - 1) as usize));
    }
    let int_len = exp as usize + 1;
    if digits.len() <= int_len {
        format!("{sign}{digits}{}.0", "0".repeat(int_len - digits.len()))
    } else {
        format!("{sign}{}.{}", &digits[..int_len], &digits[int_len..])
    }
}

/// protobuf's ToShortestFloat: fewest significant digits, starting at 6 (not 1), that
/// round-trip through f32. That is why 1.4e-45 prints as 1.4013e-45.
fn py_float32(x: f32) -> String {
    if !x.is_finite() {
        return py_float(x as f64);
    }
    let r = (6..=9)
        .map(|p| format!("{:.*e}", p - 1, x as f64).parse::<f64>().unwrap())
        .find(|r| *r as f32 == x)
        .unwrap(); // 9 significant digits always round-trip an f32
    py_float(r)
}

fn enum_cell(v: i32, names: [&str; 4]) -> String {
    match v {
        1..=3 => names[v as usize].into(),
        _ => v.to_string(),
    }
}

/// One CSV line exactly as sei_extractor.py prints it: proto3 defaults are empty cells.
pub fn csv_row(t: &Telemetry) -> String {
    // Implicit presence is "non-zero bits", so -0.0 and NaN count as set.
    let f32c = |v: f32| if v.to_bits() == 0 { String::new() } else { py_float32(v) };
    let f64c = |v: f64| if v.to_bits() == 0 { String::new() } else { py_float(v) };
    let int = |v: u64| if v == 0 { String::new() } else { v.to_string() };
    let en = |v: i32, names| if v == 0 { String::new() } else { enum_cell(v, names) };
    let b = |v: bool| if v { "True" } else { "" }.to_string();
    [
        int(t.version as u64),
        en(t.gear, ["", "GEAR_DRIVE", "GEAR_REVERSE", "GEAR_NEUTRAL"]),
        int(t.frame_seq_no),
        f32c(t.speed_mps),
        f32c(t.accel_pct),
        f32c(t.steering_deg),
        b(t.blink_left),
        b(t.blink_right),
        b(t.brake),
        en(t.autopilot, ["", "SELF_DRIVING", "AUTOSTEER", "TACC"]),
        f64c(t.lat),
        f64c(t.lon),
        f64c(t.heading),
        f64c(t.accel_x),
        f64c(t.accel_y),
        f64c(t.accel_z),
    ]
    .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(field: u32, wire: u32) -> Vec<u8> {
        varint((field << 3 | wire) as u64)
    }

    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn vfield(field: u32, v: u64) -> Vec<u8> {
        [tag(field, 0), varint(v)].concat()
    }

    fn f32field(field: u32, v: f32) -> Vec<u8> {
        [tag(field, 5), v.to_le_bytes().to_vec()].concat()
    }

    fn f64field(field: u32, v: f64) -> Vec<u8> {
        [tag(field, 1), v.to_le_bytes().to_vec()].concat()
    }

    #[test]
    fn all_fields() {
        let buf = [
            vfield(1, 7),
            vfield(2, 2),
            vfield(3, 1 << 40),
            f32field(4, 12.5),
            f32field(5, 0.25),
            f32field(6, -3.5),
            vfield(7, 1),
            vfield(8, 1),
            vfield(9, 1),
            vfield(10, 3),
            f64field(11, 37.77493),
            f64field(12, -122.41942),
            f64field(13, 271.5),
            f64field(14, 0.1),
            f64field(15, -9.81),
            f64field(16, 1e-7),
        ]
        .concat();
        let want = Telemetry {
            version: 7,
            gear: GEAR_REVERSE,
            frame_seq_no: 1 << 40,
            speed_mps: 12.5,
            accel_pct: 0.25,
            steering_deg: -3.5,
            blink_left: true,
            blink_right: true,
            brake: true,
            autopilot: AP_TACC,
            lat: 37.77493,
            lon: -122.41942,
            heading: 271.5,
            accel_x: 0.1,
            accel_y: -9.81,
            accel_z: 1e-7,
        };
        assert_eq!(decode_metadata(&buf), Some(want));
        assert_eq!(decode_metadata(&[]), Some(Telemetry::default()));
    }

    #[test]
    fn varints() {
        let d = |b: &[u8]| decode_metadata(b).map(|t| t.frame_seq_no);
        assert_eq!(d(&[0x18, 0xAC, 0x02]), Some(300));
        let mut max = vec![0x18];
        max.extend([0xFF; 9]);
        max.push(0x01);
        assert_eq!(d(&max), Some(u64::MAX));
        // 10th byte bits past 64 are dropped, not an error.
        *max.last_mut().unwrap() = 0x7F;
        assert_eq!(d(&max), Some(u64::MAX));
        let mut wrap = vec![0x18];
        wrap.extend([0x80; 9]);
        wrap.push(0x02);
        assert_eq!(d(&wrap), Some(0));
        // 11 bytes is too long.
        let mut long = vec![0x18];
        long.extend([0xFF; 10]);
        long.push(0x01);
        assert_eq!(d(&long), None);
        // Narrowing: uint32 keeps the low bits, enums become signed.
        assert_eq!(decode_metadata(&vfield(1, 0x1F_FFFF_FFFF)).unwrap().version, u32::MAX);
        assert_eq!(decode_metadata(&vfield(2, u64::MAX)).unwrap().gear, -1);
        assert_eq!(decode_metadata(&vfield(10, 1 << 31)).unwrap().autopilot, i32::MIN);
        assert_eq!(decode_metadata(&vfield(10, 7)).unwrap().autopilot, 7);
        assert!(decode_metadata(&vfield(7, 2)).unwrap().blink_left);
    }

    #[test]
    fn tags() {
        // Overlong but in-range tag is fine; 6-byte tags and tags over u32 are not.
        assert_eq!(decode_metadata(&[0x88, 0x00, 1]).unwrap().version, 1);
        assert_eq!(decode_metadata(&[0xF8, 0xFF, 0xFF, 0xFF, 0x0F, 1]), Some(Telemetry::default()));
        assert_eq!(decode_metadata(&[0x88, 0x80, 0x80, 0x80, 0x10, 1]), None);
        assert_eq!(decode_metadata(&[0x88, 0x80, 0x80, 0x80, 0x80, 0x00, 1]), None);
        // Field 0, wire types 6/7, lone end-group.
        for bad in [&[0x00, 1][..], &[0x02, 0], &[0x0E], &[0x0F], &[0x0C]] {
            assert_eq!(decode_metadata(bad), None, "{bad:02x?}");
        }
    }

    #[test]
    fn skips_unknown() {
        let v3 = vfield(1, 3);
        let unknown = [
            vfield(99, 5),
            [tag(20, 5), vec![1, 2, 3, 4]].concat(),
            [tag(21, 1), vec![0; 8]].concat(),
            [tag(22, 2), vec![2, 9, 9]].concat(),
            [tag(22, 2), vec![0]].concat(),
            // Length as an overlong 5-byte varint.
            [tag(22, 2), vec![0x81, 0x80, 0x80, 0x80, 0x00, 9]].concat(),
            // Group with content and a nested group.
            [tag(17, 3), vfield(1, 5), tag(18, 3), tag(18, 4), tag(17, 4)].concat(),
            // Known fields with the wrong wire type are unknown, not applied.
            vec![0x20, 5],
            vec![0x0D, 1, 0, 0, 0],
            vec![0x0A, 1, 0x41],
            [vec![0x09], vec![1; 8]].concat(),
            vec![0x22, 2, 1, 2],
        ];
        for u in unknown {
            let mut buf = u.clone();
            buf.extend(&v3);
            let want = Telemetry { version: 3, ..Default::default() };
            assert_eq!(decode_metadata(&buf), Some(want), "{u:02x?}");
        }
    }

    #[test]
    fn groups() {
        let open = |n| tag(17, 3).repeat(n);
        let close = |n| tag(17, 4).repeat(n);
        assert!(decode_metadata(&[open(100), close(100)].concat()).is_some());
        assert_eq!(decode_metadata(&[open(101), close(101)].concat()), None);
        assert_eq!(decode_metadata(&[tag(17, 3), tag(18, 4)].concat()), None);
        assert_eq!(decode_metadata(&[tag(17, 3), vfield(1, 1)].concat()), None);
        assert_eq!(decode_metadata(&[tag(17, 3), vec![0x0E], tag(17, 4)].concat()), None);
        // Field 0 is accepted inside a group (upb only checks it at top level).
        let inner = [vec![0x00, 7, 0x05, 1, 2, 3, 4, 0x03, 0x04], vfield(1, 5)].concat();
        let want = Telemetry { version: 3, ..Default::default() };
        assert_eq!(decode_metadata(&[tag(1, 3), inner, tag(1, 4), vfield(1, 3)].concat()), Some(want));
    }

    #[test]
    fn last_wins_and_truncation() {
        let buf = [vfield(1, 1), f32field(4, 1.0), vfield(1, 2), f32field(4, 2.0)].concat();
        let t = decode_metadata(&buf).unwrap();
        assert_eq!((t.version, t.speed_mps), (2, 2.0));
        let full = [vfield(3, 300), f32field(4, 1.0), f64field(11, 2.0), [tag(22, 2), vec![2, 9, 9]].concat()]
            .concat();
        for cut in 1..full.len() {
            // Each field boundary is a valid message; everything else is truncated.
            let ok = matches!(cut, 3 | 8 | 17);
            assert_eq!(decode_metadata(&full[..cut]).is_some(), ok, "cut {cut}");
        }
        assert_eq!(decode_metadata(&[tag(22, 2), vec![0xFF, 0xFF, 0xFF, 0xFF, 0x0F]].concat()), None);
        assert_eq!(decode_metadata(&[0x08, 1, 0x00]), None);
    }

    #[test]
    fn epb() {
        assert_eq!(strip_epb(&[0, 0, 3, 1]), [0, 0, 1]);
        assert_eq!(strip_epb(&[0, 0, 3, 3]), [0, 0, 3]);
        assert_eq!(strip_epb(&[0, 0, 3]), [0, 0]);
        assert_eq!(strip_epb(&[0, 0, 3, 0, 0, 3]), [0, 0, 0, 0]);
        // The count restarts after a dropped byte, so 00 00 03 00 03 keeps the second 03.
        assert_eq!(strip_epb(&[0, 0, 3, 0, 3]), [0, 0, 0, 3]);
        assert_eq!(strip_epb(&[0, 0, 0, 3, 7]), [0, 0, 0, 7]);
        assert_eq!(strip_epb(&[0, 3, 0, 1, 3, 3]), [0, 3, 0, 1, 3, 3]);
    }

    fn sei_nal(wrapper: &[u8], payload: &[u8]) -> Vec<u8> {
        [&[0x06, 0x05, 0x40][..], wrapper, payload, &[0x80]].concat()
    }

    fn avcc(nals: &[Vec<u8>], n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend(&(nal.len() as u32).to_be_bytes()[4 - n..]);
            out.extend(nal);
        }
        out
    }

    #[test]
    fn wrapper() {
        let p = vfield(1, 9);
        let one = |nal: Vec<u8>| sei_payloads(&avcc(&[nal], 4), 4);
        assert_eq!(one(sei_nal(&[0x42, 0x42, 0x69], &p)), std::slice::from_ref(&p));
        assert_eq!(one(sei_nal(&[0x69], &p)), std::slice::from_ref(&p));
        assert!(one(sei_nal(&[0x42, 0x42, 0x42], &p)).is_empty());
        assert!(one(sei_nal(&[0x42, 0x41, 0x69], &p)).is_empty());
        // All 0x42 (the 0x69 must come before the last byte).
        assert!(one([&[0x06, 0x05, 0x40][..], &[0x42; 6], &[0x69]].concat()).is_empty());
        assert!(one(sei_nal(&[0x42, 0x69], &[])).is_empty());
        // Emulation prevention is removed from the payload.
        assert_eq!(one(sei_nal(&[0x69], &[8, 0, 0, 3, 1])), [vec![8, 0, 0, 1]]);
        // Other NAL types and other SEI payload types are ignored.
        assert!(one([&[0x65, 0x05, 0x40, 0x69][..], &p, &[0x80]].concat()).is_empty());
        assert!(one([&[0x06, 0x04, 0x40, 0x69][..], &p, &[0x80]].concat()).is_empty());
    }

    #[test]
    fn multi_nal_sample() {
        let (a, b) = (vfield(1, 1), vfield(1, 2));
        let nals = [
            vec![0x09, 0xF0],
            sei_nal(&[0x42, 0x69], &a),
            vec![0x06],
            sei_nal(&[0x69], &b),
            vec![0x65, 0x88, 0x84, 0x00],
        ];
        for n in [1, 2, 4] {
            assert_eq!(sei_payloads(&avcc(&nals, n), n as u8), [a.clone(), b.clone()]);
        }
        // A length running past the sample stops the walk.
        let mut cut = avcc(&nals, 4);
        cut.truncate(cut.len() - 1);
        assert_eq!(sei_payloads(&cut, 4), [a, b]);
    }

    #[test]
    fn read_telemetry_from_mp4() {
        let path = std::env::temp_dir().join(format!("otc-sei-{}.mp4", std::process::id()));
        let cfg = [1, 0x64, 0, 0x28, 0xFF, 0xE1, 0, 2, 0x67, 0x64, 1, 0, 2, 0x68, 0xEB];
        let first = Telemetry { frame_seq_no: 1, heading: 2.0, gear: GEAR_DRIVE, ..Default::default() };
        let second = Telemetry { frame_seq_no: 3, speed_mps: 10.5, lat: -0.0, ..Default::default() };
        let enc = |t: &Telemetry| {
            [vfield(2, t.gear as u64), vfield(3, t.frame_seq_no), f32field(4, t.speed_mps), f64field(11, t.lat), f64field(13, t.heading)]
                .concat()
        };
        let slice = vec![0x65, 0x88, 0, 0, 1];
        let samples = [
            vec![sei_nal(&[0x42, 0x42, 0x42, 0x69], &add_epb(&enc(&first))), slice.clone()],
            vec![slice.clone()],
            // A malformed protobuf NAL is skipped, the next one in the same sample still counts.
            vec![sei_nal(&[0x69], &[0x0F]), sei_nal(&[0x69], &add_epb(&enc(&second))), slice],
        ];
        let mut m = crate::mux::Muxer::create(&path, 30000, 64, 48, &cfg).unwrap();
        for (i, s) in samples.iter().enumerate() {
            m.push(&avcc(s, 4), 1000, 0, i == 0).unwrap();
        }
        m.finish().unwrap();
        let got = read_telemetry(&path);
        std::fs::remove_file(&path).unwrap();
        let (track, frames) = got.unwrap();
        assert_eq!(track.samples.len(), 3);
        let got: Vec<_> = frames.iter().map(|f| (f.pts, f.t)).collect();
        assert_eq!(got, [(0, first), (2000, second)]);
        // Same rows sei_extractor.py printed for this file.
        assert_eq!(csv_row(&frames[0].t), ",GEAR_DRIVE,1,,,,,,,,,,2.0,,,");
        assert_eq!(csv_row(&frames[1].t), ",,3,10.5,,,,,,,-0.0,,,,,");
    }

    #[test]
    fn encode_round_trip() {
        let t = Telemetry {
            version: 1,
            gear: GEAR_REVERSE,
            frame_seq_no: 1 << 40,
            speed_mps: 13.25,
            steering_deg: -90.5,
            blink_right: true,
            brake: true,
            autopilot: AP_TACC,
            lat: 52.5163,
            lon: -13.0,
            heading: 0.0,
            accel_x: -7.5,
            accel_z: 1e-300,
            ..Default::default()
        };
        let p = encode_metadata(&t);
        assert_eq!(decode_metadata(&p), Some(t));
        // Wrapped and found again in a sample; -13.0 is mostly zero bytes, so EPB is exercised.
        let nal = super::sei_nal(&p);
        assert!(nal.windows(3).any(|w| w == [0, 0, 3]));
        assert_eq!(sei_payloads(&avcc(&[nal], 4), 4), [p]);
        assert!(encode_metadata(&Telemetry::default()).is_empty());
    }

    #[test]
    fn csv() {
        assert_eq!(
            csv_header(),
            "version,gear_state,frame_seq_no,vehicle_speed_mps,accelerator_pedal_position,\
             steering_wheel_angle,blinker_on_left,blinker_on_right,brake_applied,autopilot_state,\
             latitude_deg,longitude_deg,heading_deg,linear_acceleration_mps2_x,\
             linear_acceleration_mps2_y,linear_acceleration_mps2_z"
        );
        let t = |f: fn(&mut Telemetry)| {
            let mut t = Telemetry::default();
            f(&mut t);
            csv_row(&t)
        };
        // Expected strings come from running sei_extractor.py's MessageToDict path
        // (protobuf 7.36.2, upb) on the same values.
        let cases = [
            (t(|_| {}), ",,,,,,,,,,,,,,,"),
            (
                t(|t| {
                    *t = Telemetry {
                        version: 1,
                        gear: 1,
                        frame_seq_no: u64::MAX,
                        speed_mps: 12.0,
                        accel_pct: 0.1,
                        steering_deg: -0.0,
                        blink_left: true,
                        blink_right: true,
                        brake: true,
                        autopilot: 3,
                        lat: 37.77493,
                        lon: -122.41942,
                        heading: -0.0,
                        accel_x: 1e-07,
                        accel_y: 1.5e16,
                        accel_z: 1e16,
                    }
                }),
                "1,GEAR_DRIVE,18446744073709551615,12.0,0.1,-0.0,True,True,True,TACC,37.77493,\
                 -122.41942,-0.0,1e-07,1.5e+16,1e+16",
            ),
            (
                t(|t| {
                    t.gear = 2;
                    t.autopilot = 1;
                    t.speed_mps = 1e-05;
                    t.accel_pct = f32::MAX;
                    t.steering_deg = f32::NAN;
                    t.lat = f64::INFINITY;
                    t.lon = f64::NEG_INFINITY;
                    t.heading = f64::NAN;
                }),
                ",GEAR_REVERSE,,1e-05,3.4028235e+38,NaN,,,,SELF_DRIVING,Infinity,-Infinity,NaN,,,",
            ),
            (
                t(|t| {
                    t.gear = 3;
                    t.autopilot = 2;
                    t.speed_mps = f32::INFINITY;
                    t.accel_pct = f32::NEG_INFINITY;
                    t.steering_deg = 1.4e-45;
                    t.lat = 5e-324;
                    t.lon = 0.0001;
                    t.heading = 0.00001;
                    t.accel_x = 123456789012345678.0;
                    t.accel_y = 9999999999999998.0;
                    t.accel_z = 1234567890123456.7;
                }),
                ",GEAR_NEUTRAL,,Infinity,-Infinity,1.4013e-45,,,,AUTOSTEER,5e-324,0.0001,1e-05,\
                 1.2345678901234568e+17,9999999999999998.0,1234567890123456.8",
            ),
            (
                t(|t| {
                    t.version = u32::MAX;
                    t.frame_seq_no = 1;
                    t.speed_mps = 16777216.0;
                    t.accel_pct = 1e16;
                    t.steering_deg = 123456789.0;
                    t.accel_x = 0.1 + 0.2;
                    t.accel_y = -9.81;
                    t.accel_z = 100.0;
                }),
                "4294967295,,1,16777216.0,1e+16,123456790.0,,,,,,,,0.30000000000000004,-9.81,100.0",
            ),
            (
                t(|t| {
                    t.speed_mps = 0.0001;
                    t.accel_pct = 0.00009999;
                    t.steering_deg = 1e15;
                    t.lat = 1e15;
                    t.lon = 1e-4;
                    t.heading = 9.99e-5;
                }),
                ",,,0.0001,9.999e-05,1000000000000000.0,,,,,1000000000000000.0,0.0001,9.99e-05,,,",
            ),
            (
                t(|t| {
                    t.speed_mps = f32::MIN_POSITIVE;
                    t.accel_pct = -3.3;
                    t.steering_deg = 29.2;
                    t.lat = 2.5;
                    t.lon = -1e-5;
                }),
                ",,,1.1754944e-38,-3.3,29.2,,,,,2.5,-1e-05,,,,",
            ),
            (t(|t| (t.gear, t.autopilot) = (7, 127)), ",7,,,,,,,,127,,,,,,"),
            (t(|t| t.gear = -1), ",-1,,,,,,,,,,,,,,"),
            (t(|t| (t.speed_mps, t.lat) = (-0.0, -0.0)), ",,,-0.0,,,,,,,-0.0,,,,,"),
            (t(|t| t.blink_left = true), ",,,,,,True,,,,,,,,,"),
            // Two 16-digit candidates tie; Python takes the even one.
            (t(|t| (t.lat, t.accel_y) = (1.5e-323, -942405961250223.0 - 0.25)), ",,,,,,,,,,1.5e-323,,,,-942405961250223.2,"),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }
}
