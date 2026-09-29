//! VM lifecycle on one host: Firecracker API, jailer, taps, cgroups, snapshots, metrics sampling.

pub mod agent;
pub mod egress;
pub mod firecracker;
pub mod jailer;
pub mod network;
pub mod rootfs_open;
mod state;
pub mod vm;
