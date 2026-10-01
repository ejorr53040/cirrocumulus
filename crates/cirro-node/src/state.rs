//! The Node agent's persistent state: one SQLite database in the state dir
//! holding a record per VM, so VMs can outlive the agent (ADR 0002).
//!
//! A record is written when a VM has started and updated when it ends.
//! Everything a running VM owns on the host is named from its VM address
//! ([`crate::vm::host_id`]), so a record needs only the VM address and the
//! identity of its VMM process to find and remove that state again.
//! Console logs stay as files next to their record; the record names its file.

use crate::vm::ProcessId;
use cirro_proto::{EndReason, Ended, VmInfo};
use rusqlite::{Connection, OptionalExtension, params};
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

/// One VM's record: a running VM has a `process`, an Ended VM has
/// `info.ended` and neither a process nor a VM address.
pub(crate) struct Record {
    pub(crate) info: VmInfo,
    pub(crate) log: PathBuf,
    pub(crate) process: Option<ProcessId>,
}

pub(crate) struct Store {
    conn: Connection,
}

impl Store {
    /// Opens the database at `path`, creating it on first use for the Node
    /// subnet `subnet`. A database made for another subnet is refused: its
    /// VM addresses aren't this Node's to hand out or clean up after.
    pub(crate) fn open(path: &Path, subnet: &str) -> io::Result<Store> {
        let conn = Connection::open(path).map_err(io_error)?;
        // WAL with `synchronous = NORMAL`: a commit doesn't fsync. Waking
        // records the VM after it runs, and on btrfs that fsync waits for
        // the park's snapshot to reach disk too: 30 ms typical, 100 ms+ in
        // the tail. NORMAL survives the agent crashing; a power cut may
        // lose the last commits, but nothing fsyncs a snapshot either, so a
        // parked VM never outlived one.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .map_err(io_error)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS node (subnet TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS vms (
                 name       TEXT PRIMARY KEY,
                 vm_address TEXT,
                 pid        INTEGER,
                 pid_start  INTEGER,
                 mem_mib    INTEGER NOT NULL,
                 vcpus      INTEGER NOT NULL,
                 started_at INTEGER NOT NULL,
                 ended_at   INTEGER,
                 end_reason TEXT,
                 log        TEXT NOT NULL
             )",
        )
        .map_err(io_error)?;
        let stored: Option<String> = conn
            .query_row("SELECT subnet FROM node", [], |row| row.get(0))
            .optional()
            .map_err(io_error)?;
        match stored {
            None => {
                conn.execute("INSERT INTO node (subnet) VALUES (?1)", params![subnet])
                    .map_err(io_error)?;
            }
            Some(stored) if stored == subnet => {}
            Some(stored) => {
                return Err(io::Error::other(format!(
                    "the state dir belongs to the Node subnet {stored}, but this agent was \
                     started with {subnet}"
                )));
            }
        }
        Ok(Store { conn })
    }

    /// Every record, running VMs and Ended VMs alike.
    pub(crate) fn load(&self) -> io::Result<Vec<Record>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT name, vm_address, pid, pid_start, mem_mib, vcpus, started_at,
                        ended_at, end_reason, log
                 FROM vms ORDER BY name",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map([], |row| {
                let vm_address: Option<String> = row.get(1)?;
                let pid: Option<u32> = row.get(2)?;
                let pid_start: Option<i64> = row.get(3)?;
                let ended_at: Option<i64> = row.get(7)?;
                let end_reason: Option<String> = row.get(8)?;
                let log: String = row.get(9)?;
                Ok(Record {
                    info: VmInfo {
                        name: row.get(0)?,
                        vm_address: vm_address.and_then(|a| a.parse::<Ipv4Addr>().ok()),
                        mem_mib: row.get(4)?,
                        vcpus: row.get(5)?,
                        started_at: from_sql(row.get(6)?),
                        ended: ended_at.zip(end_reason).and_then(|(at, reason)| {
                            Some(Ended {
                                at: from_sql(at),
                                reason: parse_reason(&reason)?,
                            })
                        }),
                    },
                    log: PathBuf::from(log),
                    process: pid.zip(pid_start).map(|(pid, start_time)| ProcessId {
                        pid,
                        start_time: from_sql(start_time),
                    }),
                })
            })
            .map_err(io_error)?;
        rows.collect::<Result<_, _>>().map_err(io_error)
    }

    /// Records a VM that has started, replacing any record under its name
    /// (an Ended VM whose name is being reused).
    pub(crate) fn insert_running(
        &self,
        info: &VmInfo,
        log: &Path,
        process: ProcessId,
    ) -> io::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO vms
                     (name, vm_address, pid, pid_start, mem_mib, vcpus, started_at, log)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    info.name,
                    info.vm_address.map(|a| a.to_string()),
                    process.pid,
                    to_sql(process.start_time),
                    info.mem_mib,
                    info.vcpus,
                    to_sql(info.started_at),
                    log.to_string_lossy(),
                ],
            )
            .map(drop)
            .map_err(io_error)
    }

    /// Turns a running VM's record into an Ended VM's.
    pub(crate) fn mark_ended(&self, name: &str, ended: &Ended) -> io::Result<()> {
        self.conn
            .execute(
                "UPDATE vms
                 SET vm_address = NULL, pid = NULL, pid_start = NULL,
                     ended_at = ?2, end_reason = ?3
                 WHERE name = ?1",
                params![name, to_sql(ended.at), reason_text(ended.reason)],
            )
            .map(drop)
            .map_err(io_error)
    }

    pub(crate) fn delete(&self, name: &str) -> io::Result<()> {
        self.conn
            .execute("DELETE FROM vms WHERE name = ?1", params![name])
            .map(drop)
            .map_err(io_error)
    }
}

/// SQLite integers are signed; every number stored here (times, ticks) is
/// far below `i64::MAX`.
fn to_sql(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn from_sql(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// The stored spelling of a reason: its wire spelling, so one place names them.
fn reason_text(reason: EndReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("an end reason serializes as a string")
}

fn parse_reason(text: &str) -> Option<EndReason> {
    serde_json::from_value(serde_json::Value::String(text.to_string())).ok()
}

fn io_error(e: rusqlite::Error) -> io::Error {
    io::Error::other(format!("state database: {e}"))
}
