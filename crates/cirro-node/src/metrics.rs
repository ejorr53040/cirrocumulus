//! What `cirro top` shows: CPU, memory, disk and network use of the Node
//! and of each VM, read from the kernel's own counters.
//!
//! A VM's counters live in the cgroup jailer put its VMM in and on the
//! host side of its veth, both named by [`vm::host_id`].

use crate::vm;
pub use cirro_proto::{NodeRates, VmRates};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::debug;

/// How many rates a [`Series`] keeps: a minute, sampled every second.
pub const HISTORY: usize = 60;

/// A running VM to sample: its name, and the host id its cgroup and veth
/// are named by.
pub struct RunningVm {
    pub name: String,
    pub id: String,
}

/// A VM's counters at one moment. Each only grows, except `memory_bytes`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VmSample {
    /// CPU time the VMM has used, in microseconds.
    pub cpu_usec: u64,
    pub memory_bytes: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    /// Bytes the VM has received and sent.
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// The Node's counters at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeSample {
    /// Clock ticks every CPU spent busy, and in all, since boot.
    pub cpu_busy_ticks: u64,
    pub cpu_total_ticks: u64,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    /// Bytes through the Node's own interfaces, not its loopback or VMs'.
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Counters that turn into rates between two samples.
pub trait Sample {
    type Rates;
    /// What was used per second since `earlier`, `elapsed` ago. A counter
    /// that went backwards reads as no activity.
    fn rates_since(&self, earlier: &Self, elapsed: Duration) -> Self::Rates;
}

impl Sample for VmSample {
    type Rates = VmRates;

    /// CPU is a percentage of one core: two busy vCPUs read 200.
    fn rates_since(&self, earlier: &VmSample, elapsed: Duration) -> VmRates {
        let per_sec = |now: u64, then: u64| per_second(now.saturating_sub(then), elapsed);
        let cpu_usec = self.cpu_usec.saturating_sub(earlier.cpu_usec);
        let elapsed_usec = elapsed.as_micros().max(1) as f64;
        VmRates {
            cpu_percent: cpu_usec as f64 * 100.0 / elapsed_usec,
            memory_bytes: self.memory_bytes,
            io_read_per_sec: per_sec(self.io_read_bytes, earlier.io_read_bytes),
            io_write_per_sec: per_sec(self.io_write_bytes, earlier.io_write_bytes),
            rx_per_sec: per_sec(self.rx_bytes, earlier.rx_bytes),
            tx_per_sec: per_sec(self.tx_bytes, earlier.tx_bytes),
        }
    }
}

impl Sample for NodeSample {
    type Rates = NodeRates;

    /// CPU is a percentage of all the Node's cores together.
    fn rates_since(&self, earlier: &NodeSample, elapsed: Duration) -> NodeRates {
        let busy = self.cpu_busy_ticks.saturating_sub(earlier.cpu_busy_ticks);
        let total = self.cpu_total_ticks.saturating_sub(earlier.cpu_total_ticks);
        NodeRates {
            cpu_percent: if total == 0 {
                0.0
            } else {
                busy as f64 * 100.0 / total as f64
            },
            memory_used_bytes: self
                .memory_total_bytes
                .saturating_sub(self.memory_available_bytes),
            memory_total_bytes: self.memory_total_bytes,
            rx_per_sec: per_second(self.rx_bytes.saturating_sub(earlier.rx_bytes), elapsed),
            tx_per_sec: per_second(self.tx_bytes.saturating_sub(earlier.tx_bytes), elapsed),
        }
    }
}

/// The last [`HISTORY`] rates of one Node or VM, oldest first.
pub struct Series<S: Sample> {
    last: Option<(Instant, S)>,
    rates: VecDeque<S::Rates>,
}

impl<S: Sample> Default for Series<S> {
    fn default() -> Self {
        Series {
            last: None,
            rates: VecDeque::new(),
        }
    }
}

impl<S: Sample> Series<S> {
    /// Adds the rates since the previous sample, dropping the oldest once
    /// there are [`HISTORY`]. The first sample only sets a baseline.
    pub fn record(&mut self, at: Instant, sample: S) {
        if let Some((then, earlier)) = &self.last {
            if self.rates.len() == HISTORY {
                self.rates.pop_front();
            }
            self.rates
                .push_back(sample.rates_since(earlier, at.saturating_duration_since(*then)));
        }
        self.last = Some((at, sample));
    }

    pub fn rates(&self) -> impl Iterator<Item = &S::Rates> {
        self.rates.iter()
    }

    pub fn latest(&self) -> Option<&S::Rates> {
        self.rates.back()
    }
}

/// Reads samples from a filesystem root: `/` on a Node, a fake tree in
/// tests.
pub struct Sampler {
    root: PathBuf,
}

/// One round of samples, read before [`Recorder::record`] takes them, so
/// no lock is held while the kernel's files are read.
pub struct Readings {
    node: Option<NodeSample>,
    /// Every running VM by name, with its sample if it could be read.
    vms: Vec<(String, Option<VmSample>)>,
}

impl Sampler {
    pub fn new(root: PathBuf) -> Sampler {
        Sampler { root }
    }

    /// Samples the Node and `vms`. A VM that can't be read yet (its cgroup
    /// appears a moment after it starts) has no sample this round.
    pub fn read(&self, vms: &[RunningVm]) -> Readings {
        let node = self.node().map_err(|e| debug!("sample the Node: {e}")).ok();
        let vms = vms
            .iter()
            .map(|vm| {
                let sample = self
                    .vm(&vm.id)
                    .map_err(|e| debug!(vm = %vm.name, "sample: {e}"))
                    .ok();
                (vm.name.clone(), sample)
            })
            .collect();
        Readings { node, vms }
    }

    /// The Node's counters.
    pub fn node(&self) -> io::Result<NodeSample> {
        let stat = read(&self.root.join("proc/stat"))?;
        // cpu user nice system idle iowait irq softirq steal guest guest_nice;
        // guest time is already counted in user, so it stops at steal.
        let ticks: Vec<u64> = stat
            .lines()
            .find_map(|line| line.strip_prefix("cpu "))
            .ok_or_else(|| invalid("no cpu line in /proc/stat"))?
            .split_whitespace()
            .take(8)
            .map(number)
            .collect::<io::Result<_>>()?;
        let [user, nice, system, idle, iowait, irq, softirq, steal] = ticks[..] else {
            return Err(invalid("a short cpu line in /proc/stat"));
        };
        let busy = user + nice + system + irq + softirq + steal;

        let meminfo = read(&self.root.join("proc/meminfo"))?;
        let kib = |name| {
            meminfo
                .lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
                .and_then(|v| v.trim().strip_suffix("kB")?.trim().parse::<u64>().ok())
                .ok_or_else(|| invalid(&format!("no {name} in /proc/meminfo")))
        };

        let (mut rx_bytes, mut tx_bytes) = (0, 0);
        let net = self.root.join("sys/class/net");
        for entry in std::fs::read_dir(&net)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if name == "lo" || vm::parse_host_id(&name).is_some() {
                continue;
            }
            let statistics = net.join(&*name).join("statistics");
            // Not every entry is an interface (`bonding_masters` is a file).
            if !statistics.is_dir() {
                continue;
            }
            rx_bytes += number(&read(&statistics.join("rx_bytes"))?)?;
            tx_bytes += number(&read(&statistics.join("tx_bytes"))?)?;
        }

        Ok(NodeSample {
            cpu_busy_ticks: busy,
            cpu_total_ticks: busy + idle + iowait,
            memory_total_bytes: kib("MemTotal")? * 1024,
            memory_available_bytes: kib("MemAvailable")? * 1024,
            rx_bytes,
            tx_bytes,
        })
    }

    /// The counters of the VM with host id `id`.
    pub fn vm(&self, id: &str) -> io::Result<VmSample> {
        let cgroup = vm::parent_cgroup_under(&self.root).join(id);
        let cpu_usec = field(&read(&cgroup.join("cpu.stat"))?, "usage_usec")?;
        let memory_bytes = number(&read(&cgroup.join("memory.current"))?)?;
        // Only there when the io controller is on for the cgroup.
        let (io_read_bytes, io_write_bytes) = match read(&cgroup.join("io.stat")) {
            Ok(text) => io_bytes(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (0, 0),
            Err(e) => return Err(e),
        };
        let veth = self.root.join("sys/class/net").join(id).join("statistics");
        Ok(VmSample {
            cpu_usec,
            memory_bytes,
            io_read_bytes,
            io_write_bytes,
            // The host side of the veth receives what the VM sends.
            rx_bytes: number(&read(&veth.join("tx_bytes"))?)?,
            tx_bytes: number(&read(&veth.join("rx_bytes"))?)?,
        })
    }
}

/// The Node's series and each running VM's, as the agent keeps them.
#[derive(Default)]
pub struct Recorder {
    node: Series<NodeSample>,
    vms: HashMap<String, Series<VmSample>>,
}

impl Recorder {
    /// Records `readings`, taken at `at`, and forgets every VM they don't
    /// list as running.
    pub fn record(&mut self, at: Instant, readings: Readings) {
        if let Some(sample) = readings.node {
            self.node.record(at, sample);
        }
        self.vms
            .retain(|name, _| readings.vms.iter().any(|(running, _)| running == name));
        for (name, sample) in readings.vms {
            if let Some(sample) = sample {
                self.vms.entry(name).or_default().record(at, sample);
            }
        }
    }

    /// The Node's rates, oldest first.
    pub fn node(&self) -> Vec<NodeRates> {
        self.node.rates().copied().collect()
    }

    /// VM `name`'s rates, oldest first.
    pub fn vm(&self, name: &str) -> Vec<VmRates> {
        self.vms
            .get(name)
            .map(|series| series.rates().copied().collect())
            .unwrap_or_default()
    }
}

/// `delta` spread over `elapsed`, per second. A counter that went
/// backwards (`delta` saturated to 0) reads as no activity.
fn per_second(delta: u64, elapsed: Duration) -> u64 {
    let millis = elapsed.as_millis().max(1);
    u64::try_from(u128::from(delta) * 1000 / millis).unwrap_or(u64::MAX)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn read(path: &Path) -> io::Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| io::Error::new(e.kind(), format!("read {}: {e}", path.display())))
}

fn number(text: &str) -> io::Result<u64> {
    text.trim()
        .parse()
        .map_err(|e| invalid(&format!("{text:?}: {e}")))
}

/// The value of `name` in `name value` lines, as in `cpu.stat`.
fn field(text: &str, name: &str) -> io::Result<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
        .ok_or_else(|| invalid(&format!("no {name}")))
        .and_then(number)
}

/// Bytes read and written, summed over every device in an `io.stat`.
fn io_bytes(text: &str) -> (u64, u64) {
    let mut totals = (0, 0);
    for pair in text.split_whitespace() {
        let value = |prefix| {
            pair.strip_prefix(prefix)
                .and_then(|v: &str| v.parse::<u64>().ok())
        };
        if let Some(n) = value("rbytes=") {
            totals.0 += n;
        } else if let Some(n) = value("wbytes=") {
            totals.1 += n;
        }
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn fake_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("cirro-metrics-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn a_vm_is_sampled_from_its_cgroup_and_its_veth() {
        let root = fake_root("vm");
        let cgroup = "sys/fs/cgroup/cirro/cirro-0102";
        write(
            &root,
            &format!("{cgroup}/cpu.stat"),
            "usage_usec 1500000\nuser_usec 1000000\nsystem_usec 500000\n",
        );
        write(&root, &format!("{cgroup}/memory.current"), "268435456\n");
        write(
            &root,
            &format!("{cgroup}/io.stat"),
            "8:0 rbytes=1000 wbytes=2000 rios=1 wios=2 dbytes=0 dios=0\n\
             259:0 rbytes=30 wbytes=40 rios=1 wios=1 dbytes=0 dios=0\n",
        );
        // The host side of the veth: what it receives, the VM sent.
        write(
            &root,
            "sys/class/net/cirro-0102/statistics/rx_bytes",
            "700\n",
        );
        write(
            &root,
            "sys/class/net/cirro-0102/statistics/tx_bytes",
            "900\n",
        );

        let sample = Sampler::new(root).vm("cirro-0102").unwrap();

        assert_eq!(
            sample,
            VmSample {
                cpu_usec: 1_500_000,
                memory_bytes: 268_435_456,
                io_read_bytes: 1030,
                io_write_bytes: 2040,
                rx_bytes: 900,
                tx_bytes: 700,
            }
        );
    }

    #[test]
    fn vm_rates_are_per_second_and_cpu_is_a_percentage_of_one_core() {
        let earlier = VmSample {
            cpu_usec: 1_000_000,
            memory_bytes: 100,
            io_read_bytes: 0,
            io_write_bytes: 1000,
            rx_bytes: 500,
            tx_bytes: 0,
        };
        let later = VmSample {
            // Two vCPUs, both busy for the whole 2 s.
            cpu_usec: 5_000_000,
            memory_bytes: 300,
            io_read_bytes: 4096,
            io_write_bytes: 1000,
            rx_bytes: 2500,
            tx_bytes: 10,
        };

        let rates = later.rates_since(&earlier, Duration::from_secs(2));

        assert_eq!(
            rates,
            VmRates {
                cpu_percent: 200.0,
                memory_bytes: 300,
                io_read_per_sec: 2048,
                io_write_per_sec: 0,
                rx_per_sec: 1000,
                tx_per_sec: 5,
            }
        );
    }

    #[test]
    fn a_counter_that_went_backwards_reads_as_no_activity() {
        let earlier = VmSample {
            cpu_usec: 9_000_000,
            rx_bytes: 9000,
            ..Default::default()
        };
        let later = VmSample::default();

        let rates = later.rates_since(&earlier, Duration::from_secs(1));

        assert_eq!(rates.cpu_percent, 0.0);
        assert_eq!(rates.rx_per_sec, 0);
    }

    #[test]
    fn the_node_is_sampled_from_proc_and_its_own_interfaces() {
        let root = fake_root("node");
        write(
            &root,
            "proc/stat",
            "cpu  100 20 30 800 50 0 0 0 0 0\ncpu0 50 10 15 400 25 0 0 0 0 0\nintr 1 2 3\n",
        );
        write(
            &root,
            "proc/meminfo",
            "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:    4000000 kB\n",
        );
        for (interface, rx, tx) in [
            ("wlo1", "1000", "2000"),
            ("eth0", "10", "20"),
            ("lo", "5", "5"),
        ] {
            write(
                &root,
                &format!("sys/class/net/{interface}/statistics/rx_bytes"),
                rx,
            );
            write(
                &root,
                &format!("sys/class/net/{interface}/statistics/tx_bytes"),
                tx,
            );
        }
        // A plain file, there when the bonding module is loaded.
        write(&root, "sys/class/net/bonding_masters", "");
        // A VM's veth: counted for the VM, and again on the way out.
        write(
            &root,
            "sys/class/net/cirro-0102/statistics/rx_bytes",
            "99999",
        );
        write(
            &root,
            "sys/class/net/cirro-0102/statistics/tx_bytes",
            "99999",
        );

        let sample = Sampler::new(root).node().unwrap();

        assert_eq!(
            sample,
            NodeSample {
                // user + nice + system + irq + softirq + steal; idle and
                // iowait are not busy.
                cpu_busy_ticks: 150,
                cpu_total_ticks: 1000,
                memory_total_bytes: 16_000_000 * 1024,
                memory_available_bytes: 4_000_000 * 1024,
                rx_bytes: 1010,
                tx_bytes: 2020,
            }
        );
    }

    #[test]
    fn node_cpu_is_a_percentage_of_every_core_and_memory_used_excludes_available() {
        let earlier = NodeSample {
            cpu_busy_ticks: 100,
            cpu_total_ticks: 1000,
            memory_total_bytes: 8000,
            memory_available_bytes: 6000,
            rx_bytes: 0,
            tx_bytes: 0,
        };
        let later = NodeSample {
            cpu_busy_ticks: 400,
            cpu_total_ticks: 2000,
            memory_total_bytes: 8000,
            memory_available_bytes: 2000,
            rx_bytes: 3000,
            tx_bytes: 1500,
        };

        let rates = later.rates_since(&earlier, Duration::from_millis(1500));

        assert_eq!(
            rates,
            NodeRates {
                cpu_percent: 30.0,
                memory_used_bytes: 6000,
                memory_total_bytes: 8000,
                rx_per_sec: 2000,
                tx_per_sec: 1000,
            }
        );
    }

    #[test]
    fn a_series_keeps_the_last_minute_of_rates_newest_last() {
        let start = Instant::now();
        let mut series = Series::default();

        for second in 0..=62u64 {
            let sample = VmSample {
                // One more second of CPU every second: 100%, then 0% at the end.
                cpu_usec: second.min(61) * 1_000_000,
                ..Default::default()
            };
            series.record(start + Duration::from_secs(second), sample);
        }

        let rates: Vec<f64> = series.rates().map(|r| r.cpu_percent).collect();
        assert_eq!(rates.len(), HISTORY);
        assert_eq!(rates[..HISTORY - 1], [100.0; HISTORY - 1]);
        assert_eq!(rates[HISTORY - 1], 0.0);
        assert_eq!(series.latest().map(|r| r.cpu_percent), Some(0.0));
    }
}
