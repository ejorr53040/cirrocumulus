//! Writing the edge's keys and certificates to the agent's state dir.

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Writes `bytes` to `path` with `mode` through a temporary file renamed
/// into place, so `path` never holds part of them.
pub(crate) fn write_whole(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}
