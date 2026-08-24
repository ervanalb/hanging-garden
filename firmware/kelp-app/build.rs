use std::fs;

fn main() {
    // println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    // println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_dir = std::path::PathBuf::from(out_dir);
    println!("cargo:rustc-link-search={}", out_dir.display());

    // Copy unified_memory.x from workspace root
    fs::copy("../unified_memory.x", out_dir.join("unified_memory.x")).unwrap();
    println!("cargo:rerun-if-changed=unified_memory.x");

    // Copy app-specific linker script
    fs::copy("memory.x", out_dir.join("memory.x")).unwrap();
    println!("cargo:rerun-if-changed=memory.x");
}
