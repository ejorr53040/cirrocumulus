//! VM lifecycle on one host: Firecracker API, jailer, taps, cgroups, snapshots, metrics sampling.

pub mod agent;
pub mod firecracker;
pub mod jailer;
pub mod network;
pub mod vm;
