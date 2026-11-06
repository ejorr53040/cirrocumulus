//! Telling a process apart from a later one that reused its pid.

/// A VMM process, identified by its pid and start time together so that a
/// pid reused by some other process is never mistaken for the VMM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessId {
    pub(crate) pid: u32,
    /// The process's start time from `/proc/<pid>/stat`, in clock ticks
    /// since boot.
    pub(crate) start_time: u64,
}

impl ProcessId {
    /// The identity of the running process `pid`.
    pub(crate) fn of(pid: u32) -> Option<ProcessId> {
        let (_, start_time) = proc_stat(pid)?;
        Some(ProcessId { pid, start_time })
    }

    /// Whether this exact process is still running. A zombie has ended.
    pub(crate) fn is_running(self) -> bool {
        proc_stat(self.pid).is_some_and(|(state, start)| state != 'Z' && start == self.start_time)
    }
}

/// The state and start time of a process, from `/proc/<pid>/stat`. Its
/// fields follow the command name, which is in parentheses and may itself
/// contain spaces and parentheses, so they are counted from the last `)`.
fn proc_stat(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    // `state` is field 3 and the start time field 22.
    let start_time = fields.nth(18)?.parse().ok()?;
    Some((state, start_time))
}
