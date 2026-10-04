//! Builds `guest-init` (musl static, see `crates/guest-init` and
//! RESEARCH.md M2) as part of building `cirro`, so `main.rs` can
//! `include_bytes!` it -- the "single `cirro` binary, no daemon sprawl"
//! pitch (RESEARCH.md §0) means guest-init has to ship inside `cirro`
//! itself, not as a separate artifact a user has to know to also install.
//!
//! The `cirro-guest-init` package says where its source is through `links`
//! metadata. In this repository that is `crates/guest-init`, built with the
//! workspace's lockfile into `target/guest-init-embed`, where the VM tests
//! also find it. Anywhere else (a crates.io download, a vendored copy) the
//! source is copied under `OUT_DIR` and built there, so nothing is written
//! next to the downloaded source. Either way the nested `cargo build` gets a
//! target dir of its own: sharing the outer build's is a known way to
//! deadlock on Cargo's target-dir lock.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const TARGET: &str = "x86_64-unknown-linux-musl";

/// Settings of the outer build that must not reach the nested one: flags
/// and wrappers meant for the host build (coverage, sanitizers, sccache),
/// and its target dir.
const OUTER_BUILD_ENV: [&str; 7] = [
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTFLAGS",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET",
    "CARGO_MAKEFLAGS",
];

fn main() {
    let source = PathBuf::from(
        std::env::var("DEP_CIRRO_GUEST_INIT_SRC")
            .expect("cirro-guest-init's build script names its source"),
    );
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sibling = manifest_dir.join("../guest-init");
    let from_this_repository = matches!(
        (source.canonicalize(), sibling.canonicalize()),
        (Ok(source), Ok(sibling)) if source == sibling
    );

    let mut cargo = Command::new(env!("CARGO"));
    cargo.args([
        "build",
        "--release",
        "--target",
        TARGET,
        "--bin",
        "guest-init",
    ]);
    for name in OUTER_BUILD_ENV {
        cargo.env_remove(name);
    }
    let target_dir = if from_this_repository {
        let workspace = manifest_dir.join("../..");
        println!(
            "cargo:rerun-if-changed={}",
            workspace.join("Cargo.lock").display()
        );
        cargo
            .arg("--locked")
            .arg("--manifest-path")
            .arg(source.join("Cargo.toml"));
        workspace.join("target/guest-init-embed")
    } else {
        let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
        let copy = out.join("guest-init-src");
        let _ = std::fs::remove_dir_all(&copy);
        copy_dir(&source, &copy).expect("copy guest-init's source to build it");
        // Its own workspace, or cargo would take whatever workspace the copy
        // sits in (the consumer's, through its target dir) for its own.
        let manifest = copy.join("Cargo.toml");
        let mut toml = std::fs::read_to_string(&manifest).expect("read guest-init's manifest");
        toml.push_str("\n[workspace]\n");
        std::fs::write(&manifest, toml).expect("write guest-init's manifest");
        cargo.arg("--manifest-path").arg(copy.join("Cargo.toml"));
        out.join("guest-init-target")
    };
    let status = cargo
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .expect("run cargo to build guest-init for embedding into cirro");
    assert!(
        status.success(),
        "building guest-init for embedding failed -- is the {TARGET} target installed? \
         (`rustup target add {TARGET}`)"
    );

    let binary = target_dir.join(TARGET).join("release/guest-init");
    println!("cargo:rustc-env=CIRRO_GUEST_INIT={}", binary.display());
    println!("cargo:rerun-if-changed={}", source.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        source.join("Cargo.toml").display()
    );
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let destination = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &destination)?;
        } else {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}
