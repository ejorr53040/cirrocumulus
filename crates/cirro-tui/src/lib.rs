//! `cirro top`: the terminal dashboard.

use cirro_proto::Stats;
use std::fmt::Write;

/// `stats` as plain text, for `cirro top --once`: a line for the Node,
/// then a row per running VM with its latest rates (`-` until the agent has
/// two samples of it).
pub fn snapshot(stats: &Stats) -> String {
    let mut out = String::new();
    match stats.node.last() {
        Some(node) => {
            let _ = writeln!(
                out,
                "NODE  CPU {:.1}%  MEM {}/{}  NET {}/s in, {}/s out",
                node.cpu_percent,
                bytes(node.memory_used_bytes),
                bytes(node.memory_total_bytes),
                bytes(node.rx_per_sec),
                bytes(node.tx_per_sec),
            );
        }
        None => out.push_str("NODE  (sampling)\n"),
    }
    let _ = writeln!(
        out,
        "{:<32} {:>7} {:>7} {:>9} {:>9} {:>9} {:>9}",
        "NAME", "CPU", "MEM", "NET IN", "NET OUT", "DISK R", "DISK W"
    );
    for vm in &stats.vms {
        let row = match vm.history.last() {
            Some(r) => [
                format!("{:.1}%", r.cpu_percent),
                bytes(r.memory_bytes),
                per_sec(r.rx_per_sec),
                per_sec(r.tx_per_sec),
                per_sec(r.io_read_per_sec),
                per_sec(r.io_write_per_sec),
            ],
            None => std::array::from_fn(|_| "-".to_string()),
        };
        let _ = writeln!(
            out,
            "{:<32} {:>7} {:>7} {:>9} {:>9} {:>9} {:>9}",
            vm.info.name, row[0], row[1], row[2], row[3], row[4], row[5]
        );
    }
    out
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

fn per_sec(n: u64) -> String {
    format!("{}/s", bytes(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cirro_proto::{NodeRates, VmInfo, VmRates, VmStats};

    fn vm(name: &str, history: Vec<VmRates>) -> VmStats {
        VmStats {
            info: VmInfo {
                name: name.into(),
                vm_address: Some("10.77.0.2".parse().unwrap()),
                mem_mib: 256,
                vcpus: 1,
                started_at: 0,
                ended: None,
            },
            history,
        }
    }

    #[test]
    fn the_snapshot_shows_the_latest_rates_and_a_dash_before_there_are_any() {
        let stats = Stats {
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

        let text = snapshot(&stats);
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
            ]
        );
    }
}
