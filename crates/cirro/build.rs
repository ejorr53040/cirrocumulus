//! Builds `guest-init` (musl static, see `crates/guest-init` and
//! RESEARCH.md M2) as part of building `cirro`, so `main.rs` can
//! `include_bytes!` it -- the "single `cirro` binary, no daemon sprawl"
//! pitch (RESEARCH.md §0) means guest-init has to ship inside `cirro`
//! itself, not as a separate artifact a user has to know to also install.
//!
//! Uses its own `--target-dir` (rather than the workspace's default
//! `target/`) deliberately: a nested `cargo build` sharing the *same*
//! target directory as the outer build that's currently running this
//! build script is a known way to deadlock on Cargo's target-dir lock.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/cirro is two levels under the workspace root");
    let target_dir = workspace_root.join("target/guest-init-embed");

    let status = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg("x86_64-unknown-linux-musl")
        .arg("--package")
        .arg("guest-init")
        .arg("--manifest-path")
        .arg(workspace_root.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .expect("run cargo to build guest-init for embedding into cirro");

    assert!(
        status.success(),
        "building guest-init for embedding failed -- is the \
         x86_64-unknown-linux-musl target installed? \
         (`rustup target add x86_64-unknown-linux-musl`)"
    );

    println!("cargo:rerun-if-changed=../guest-init/src");
    println!("cargo:rerun-if-changed=../guest-init/Cargo.toml");
}
