// The setup program and the uninstaller carry the app's icon.
include!("../build_icon.rs");

fn main() {
    println!("cargo:rerun-if-changed=../build_icon.rs");
    embed_icon(std::path::Path::new("../assets/icon.ico"));
}
