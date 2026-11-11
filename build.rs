// mfplat.dll is missing on Windows N and some Server editions: a plain import would stop the app
// from starting there, so it and d3d11.dll load only once src/mf.rs has checked they exist.
include!("build_icon.rs");

fn main() {
    println!("cargo:rerun-if-changed=build_icon.rs");
    embed_icon(std::path::Path::new("assets/icon.ico"));
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:mfplat.dll");
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:d3d11.dll");
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
