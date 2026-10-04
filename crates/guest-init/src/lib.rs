//! `guest-init` is PID 1 inside every Cirrocumulus microVM; the binary is
//! `src/main.rs`. The library holds what can be used, and fuzzed, off the
//! guest: parsing the config the Node agent sends over vsock. `cirro`
//! depends on the package so its build script can find and build the
//! binary.

pub mod config;
