use std::path::PathBuf;

fn main() {
    build_playfair();
    write_no_embed();
}

fn build_playfair() {
    let mut build = cc::Build::new();
    build
        .define("PLAYFAIR_QUIET", "1")
        .include("vendor/playfair")
        .file("vendor/playfair/playfair.c")
        .file("vendor/playfair/omg_hax.c")
        .file("vendor/playfair/modified_md5.c")
        .file("vendor/playfair/sap_hash.c")
        .file("vendor/playfair/hand_garble.c")
        .file("vendor/playfair/fairplay_encrypt.c");

    let is_windows = std::env::var("TARGET")
        .map(|t| t.contains("windows"))
        .unwrap_or(false);
    if !is_windows {
        build.flag("-Wno-unused-parameter");
    }

    build.compile("playfair");
    if !is_windows {
        println!("cargo:rustc-link-lib=m");
    }
    println!("cargo:rerun-if-changed=vendor/playfair");
}

fn write_no_embed() {
    let manifest_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let embed_rs = manifest_dir.join("fpsap_embed.rs");
    std::fs::write(embed_rs, "pub const FPSAP_HELPER_BYTES: &[u8] = &[];\n")
        .expect("write fpsap_embed.rs");
    println!("cargo:rerun-if-changed=../../tools/fpsap-helper");
}
