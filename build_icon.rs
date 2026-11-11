// Included by both build scripts: gives the exe `assets/icon.ico` as its icon, which is what Explorer,
// the Start menu and shortcuts show. Written out as a compiled resource (.res) by hand, so no resource
// compiler and no extra crate is needed; the MSVC linker takes the file as it is.
fn embed_icon(ico: &std::path::Path) {
    println!("cargo:rerun-if-changed={}", ico.display());
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let b = std::fs::read(ico).expect("assets/icon.ico");
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as usize;
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
    // One entry of a .res file: a 32-byte header with numeric type and name, then the data, padded to 4.
    fn entry(out: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(32u32.to_le_bytes());
        for v in [0xFFFF, kind, 0xFFFF, id] {
            out.extend(v.to_le_bytes());
        }
        // Data version, memory flags (moveable, pure, discardable), language, version, characteristics.
        out.extend([0u8; 4]);
        out.extend(0x1030u16.to_le_bytes());
        out.extend([0u8; 10]);
        out.extend(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    let mut res = Vec::new();
    // Every .res file starts with an empty entry.
    entry(&mut res, 0, 0, &[]);
    let count = u16_at(4);
    // The group lists the pictures: the .ico directory with a resource id where the file offset was.
    let mut group = b[..6].to_vec();
    for k in 0..count {
        let at = 6 + 16 * k;
        let (size, offset) = (u32_at(at + 8), u32_at(at + 12));
        // RT_ICON
        entry(&mut res, 3, k as u16 + 1, &b[offset..offset + size]);
        group.extend(&b[at..at + 12]);
        group.extend((k as u16 + 1).to_le_bytes());
    }
    // RT_GROUP_ICON; Explorer shows the first group.
    entry(&mut res, 14, 1, &group);
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("icon.res");
    std::fs::write(&out, res).expect("icon.res");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
