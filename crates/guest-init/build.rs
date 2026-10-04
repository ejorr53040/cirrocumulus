//! Tells packages that depend on this one where its source is, as `links`
//! metadata (`DEP_CIRRO_GUEST_INIT_SRC`): `cirro`'s build script builds the
//! `guest-init` binary from it.

fn main() {
    println!("cargo:src={}", env!("CARGO_MANIFEST_DIR"));
}
