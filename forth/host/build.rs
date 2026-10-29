//! Builds the Forth VM (`../vm`) for wasm32-unknown-unknown and places the
//! module in OUT_DIR, where `src/kernel/forth.rs` embeds it with include_bytes!.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let vm = manifest.join("../vm");
    println!("cargo:rerun-if-changed=../vm/src");
    println!("cargo:rerun-if-changed=../vm/Cargo.toml");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // Own target dir inside OUT_DIR: no lock contention with the outer build.
    let target = out.join("vm-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(&vm)
        .args(["build", "--release", "--target", "wasm32-unknown-unknown", "--target-dir"])
        .arg(&target);
    for k in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_TARGET_DIR",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        cmd.env_remove(k);
    }
    let status = cmd.status().expect("run cargo to build the Forth VM");
    assert!(
        status.success(),
        "building the Forth VM (forth/vm) for wasm32-unknown-unknown failed; is the target installed? \
         (rustup target add wasm32-unknown-unknown)"
    );
    std::fs::copy(target.join("wasm32-unknown-unknown/release/cmh_forth_vm.wasm"), out.join("cmh_forth_vm.wasm"))
        .expect("copy cmh_forth_vm.wasm");
}
