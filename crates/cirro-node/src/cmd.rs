//! Running a host tool to completion from blocking code.

use std::io::{self, Write};
use std::process::{Command, Stdio};

/// Runs `command`, feeding it `stdin` when given, and returns its stdout.
/// On failure the error names the command and carries its stderr.
pub(crate) fn run(command: &mut Command, stdin: Option<&str>) -> io::Result<Vec<u8>> {
    let mut child = command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(io::Error::other(format!(
            "{command:?}: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
