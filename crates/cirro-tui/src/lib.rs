//! `cirro top`: the terminal dashboard.

mod dashboard;

pub use dashboard::{Action, Dashboard};

use cirro_proto::{MachineAppInfo, NodeRates, Stats, VmRates};
use std::fmt::Write;

/// `stats` as plain text, for `cirro top --once`: a line for the Node,
/// then a row per running VM with its latest rates (`-` until the agent has
/// two samples of it), then a row per Machine-backed App with its Machine,
/// port and status, which take the place of rates: they run elsewhere.
pub fn snapshot(stats: &Stats, machine_apps: &[MachineAppInfo]) -> String {
    let mut out = String::new();
    match stats.node.last() {
        Some(node) => {
            let _ = writeln!(out, "NODE  {}", node_summary(node, "  "));
        }
        None => out.push_str("NODE  (sampling)\n"),
    }
    row(
        &mut out,
        [
            "NAME", "CPU", "MEM", "NET IN", "NET OUT", "DISK R", "DISK W",
        ]
        .map(String::from),
    );
    for vm in &stats.vms {
        let [cpu, mem, rx, tx, read, write] = rate_cells(vm.history.last());
        row(
            &mut out,
            [vm.info.name.clone(), cpu, mem, rx, tx, read, write],
        );
    }
    if !machine_apps.is_empty() {
        let _ = writeln!(out, "{:<32} {:<24} STATUS", "MACHINE-BACKED APP", "MACHINE");
    }
    for app in machine_apps {
        let _ = writeln!(
            out,
            "{:<32} {:<24} {}",
            app.name,
            machine_port(app),
            app.status.as_str()
        );
    }
    out
}

/// The Node's latest CPU, memory and network, with `between` separating
/// the network from the rest: the dashboard puts it on a line of its own.
pub(crate) fn node_summary(node: &NodeRates, between: &str) -> String {
    format!(
        "CPU {:.1}%  MEM {}/{}{between}NET {} in, {} out",
        node.cpu_percent,
        bytes(node.memory_used_bytes),
        bytes(node.memory_total_bytes),
        per_sec(node.rx_per_sec),
        per_sec(node.tx_per_sec),
    )
}

/// A VM's latest rates as table cells: CPU, memory, network in and out,
/// disk read and write. All `-` until the agent has two samples of it.
pub(crate) fn rate_cells(rates: Option<&VmRates>) -> [String; 6] {
    match rates {
        Some(r) => [
            format!("{:.1}%", r.cpu_percent),
            bytes(r.memory_bytes),
            per_sec(r.rx_per_sec),
            per_sec(r.tx_per_sec),
            per_sec(r.io_read_per_sec),
            per_sec(r.io_write_per_sec),
        ],
        None => std::array::from_fn(|_| "-".to_string()),
    }
}

/// `secs` in its largest whole unit: `45s`, `12m`, `3h`, `2d`.
pub fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// `mib` MiB of memory or disk in the largest unit that keeps it at least
/// 1: `256M`, `1.5G`.
pub fn mem(mib: u64) -> String {
    bytes(mib << 20)
}

/// Where a Machine-backed App runs: its Machine and port, as `pi:8080`.
pub fn machine_port(app: &MachineAppInfo) -> String {
    format!("{}:{}", app.machine, app.route.port)
}

/// One line of the `--once` table: a name, then six right-aligned cells.
fn row(out: &mut String, [name, cells @ ..]: [String; 7]) {
    let _ = write!(out, "{name:<32}");
    for (cell, width) in cells.iter().zip([7, 7, 9, 9, 9, 9]) {
        let _ = write!(out, " {cell:>width$}");
    }
    out.push('\n');
}

/// `n` bytes in the largest unit that keeps it at least 1: `512B`, `140M`,
/// `1.5G`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 10.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

pub(crate) fn per_sec(n: u64) -> String {
    format!("{}/s", bytes(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cirro_proto::{MachineStatus, Route, VmInfo, VmStats};

    fn vm(name: &str, history: Vec<VmRates>) -> VmStats {
        VmStats {
            info: VmInfo {
                name: name.into(),
                vm_address: Some("10.77.0.2".parse().unwrap()),
                mem_mib: 256,
                vcpus: 1,
                started_at: 0,
                ended: None,
                route: None,
            },
            history,
        }
    }

    #[test]
    fn memory_shows_in_the_largest_unit_that_keeps_it_at_least_one() {
        assert_eq!([mem(256), mem(1024), mem(1536)], ["256M", "1.0G", "1.5G"]);
    }

    #[test]
    fn the_snapshot_shows_the_latest_rates_and_a_dash_before_there_are_any() {
        let stats = Stats {
            now: 0,
            node: vec![NodeRates {
                cpu_percent: 12.34,
                memory_used_bytes: 3 << 30,
                memory_total_bytes: 16 << 30,
                rx_per_sec: 1536,
                tx_per_sec: 300,
            }],
            vms: vec![
                vm(
                    "web",
                    vec![
                        VmRates::default(),
                        VmRates {
                            cpu_percent: 99.75,
                            memory_bytes: 140 << 20,
                            io_read_per_sec: 0,
                            io_write_per_sec: 4096,
                            rx_per_sec: 10 << 20,
                            tx_per_sec: 512,
                        },
                    ],
                ),
                vm("new", Vec::new()),
            ],
        };

        let blog = MachineAppInfo {
            name: "blog".into(),
            machine: "pi".into(),
            route: Route {
                host: "blog.test".into(),
                port: 8080,
                idle_park_secs: None,
            },
            status: MachineStatus::Up,
        };

        let text = snapshot(&stats, &[blog]);
        let lines: Vec<Vec<&str>> = text
            .lines()
            .map(|l| l.split_whitespace().collect())
            .collect();

        assert_eq!(
            lines,
            [
                vec![
                    "NODE", "CPU", "12.3%", "MEM", "3.0G/16G", "NET", "1.5K/s", "in,", "300B/s",
                    "out"
                ],
                vec![
                    "NAME", "CPU", "MEM", "NET", "IN", "NET", "OUT", "DISK", "R", "DISK", "W"
                ],
                vec!["web", "99.8%", "140M", "10M/s", "512B/s", "0B/s", "4.0K/s"],
                vec!["new", "-", "-", "-", "-", "-", "-"],
                vec!["MACHINE-BACKED", "APP", "MACHINE", "STATUS"],
                vec!["blog", "pi:8080", "up"],
            ]
        );
    }
}
