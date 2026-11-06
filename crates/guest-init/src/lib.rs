//! `guest-init` is PID 1 inside every Cirrocumulus microVM; the binary is
//! `src/main.rs`. The library holds what can be used, and fuzzed, off the
//! guest: the config the Node agent sends over vsock, which `cirro-node`
//! sends with these same types. `cirro` depends on the package so its
//! build script can find and build the binary.

pub mod config;

/// Printed on the console as a host shutdown request is forwarded to the
/// app as `SIGTERM`.
pub const FORWARDING_MARKER: &str = "GUEST_INIT_FORWARDING_SIGTERM";
