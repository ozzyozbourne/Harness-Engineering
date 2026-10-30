//! Builds the built-in kernel image (`../guest`) for wasm32-wasip2 and
//! embeds it, together with its source (so the state dir can hold an
//! editable copy, `kernel-src/`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The guest crate's files that make up an image's source.
fn source_files(guest: &Path) -> Vec<PathBuf> {
    let mut out = vec![guest.join("Cargo.toml"), guest.join("Cargo.lock")];
    let mut stack = vec![guest.join("src"), guest.join("seed"), guest.join("wit")];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir).expect("read guest dir").map(|e| e.expect("entry").path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() { stack.push(p) } else { out.push(p) }
        }
    }
    out.sort();
    out
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let guest = manifest.join("../guest").canonicalize().expect("guest dir");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let files = source_files(&guest);
    let mut list =
        String::from("/// The built-in kernel image's source: (path, contents).\npub const KERNEL_FILES: &[(&str, &[u8])] = &[\n");
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        let rel = f.strip_prefix(&guest).expect("inside guest").to_string_lossy().into_owned();
        let _ = writeln!(list, "    ({rel:?}, include_bytes!({:?})),", f.display().to_string());
    }
    list.push_str("];\n");
    std::fs::write(out.join("kernel_files.rs"), list).expect("write kernel_files.rs");
    println!("cargo:rerun-if-changed={}", guest.join("src").display());

    // Own target dir inside OUT_DIR: no lock contention with the outer build.
    let target = out.join("guest-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(&guest).args(["build", "--release", "--locked", "--target", "wasm32-wasip2", "--target-dir"]).arg(&target);
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
    let status = cmd.status().expect("run cargo to build the kernel image");
    assert!(
        status.success(),
        "building the kernel image (wasm/guest) for wasm32-wasip2 failed; is the target installed? (rustup target add wasm32-wasip2)"
    );
    std::fs::copy(target.join("wasm32-wasip2/release/cmh_guest.wasm"), out.join("cmh_guest.wasm")).expect("copy cmh_guest.wasm");
}
