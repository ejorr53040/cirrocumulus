//! Fetches and verifies the Firecracker, jailer and guest kernel binaries
//! `cirro node install` (#11) needs, replacing the checked-in binaries and
//! `scripts/step0/fetch_kernel.sh`'s always-latest, unverified download.
//!
//! Pinned rather than "latest": a Node's install should be reproducible and
//! the same next week, and every artifact is checked against a SHA-256
//! recorded here rather than trusted on arrival.
//!
//! Shells out to `curl`/`tar`/`sha256sum` rather than pulling in an HTTP
//! client crate, matching how the rest of this crate already shells out to
//! `ip`/`nft`. [`install::install`](crate::install::install) is the only
//! caller in production; its own tests pass `--firecracker`/`--jailer`/
//! `--kernel` overrides (the same convention `node agent` already uses) to
//! skip this module entirely, so `cargo test` never needs network access.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const FIRECRACKER_VERSION: &str = "1.17.0";

const RELEASE_DIR: &str = "release-v1.17.0-x86_64";
const FIRECRACKER_URL: &str = "https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-x86_64.tgz";
/// The whole release tarball's own hash (from its published
/// `firecracker-v1.17.0-x86_64.tgz.sha256.txt`), checked before it's
/// trusted enough to extract anything from.
const FIRECRACKER_TGZ_SHA256: &str =
    "06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558";
/// The two binaries' own hashes, from the same release's `SHA256SUMS`,
/// checked again individually after extraction -- belt and suspenders, and
/// what lets a repeat install recognize them as already fetched.
const FIRECRACKER_BIN_SHA256: &str =
    "99ad0f5cd0514a88aad0e9ae8cfdb3cc3b4ab9d190e1194602406c786b5de7a5";
const JAILER_BIN_SHA256: &str = "65ef226e96f0ceda55ba643f445801ef2cc0ea667ef67cad8ac4f406c9c8434f";

// Firecracker CI builds vmlinux images under a date-stamped S3 prefix with
// no long-term stability guarantee (unlike a GitHub release asset), so this
// pin is the best available source, not an ideal one: it may 404 upstream
// eventually and need re-pinning with a fresh `scripts/step0/fetch_kernel.sh`
// run followed by `sha256sum`. Recorded here 2026-09-29.
const KERNEL_URL: &str = "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260929-a738f18a8db0-0/x86_64/vmlinux-6.18.48";
const KERNEL_SHA256: &str = "b0ff002711a6be32f2f5cbc21fbb7b2987b7807e0d37540034d49db22ce3d06b";
const KERNEL_FILENAME: &str = "vmlinux-6.18.48";

/// The three binaries a Node needs to run VMs: wherever they came from
/// (this module's own pinned fetch, or an override an operator/test
/// supplied instead), they always travel together, so `install`,
/// `NodeConfig` and the CLI's `--firecracker`/`--jailer`/`--kernel`
/// override all pass this one type around rather than three loose
/// `PathBuf`s each.
#[derive(Clone, Serialize, Deserialize)]
pub struct ReleaseBinaries {
    pub firecracker: PathBuf,
    pub jailer: PathBuf,
    pub kernel: PathBuf,
}

/// Downloads the pinned Firecracker release tarball (which holds both
/// `firecracker` and `jailer`) and the pinned kernel into `dest_dir`
/// (already created), verifying each against this module's SHA-256
/// constants before trusting it. Idempotent: a file already present with
/// the right hash is kept, not re-fetched, so a second `cirro node
/// install` does no network I/O at all.
pub fn fetch(dest_dir: &Path) -> io::Result<ReleaseBinaries> {
    let firecracker = dest_dir.join("firecracker");
    let jailer = dest_dir.join("jailer");
    if verify(&firecracker, FIRECRACKER_BIN_SHA256).is_err()
        || verify(&jailer, JAILER_BIN_SHA256).is_err()
    {
        let tgz = dest_dir.join("firecracker.tgz");
        download(FIRECRACKER_URL, &tgz)?;
        verify(&tgz, FIRECRACKER_TGZ_SHA256)?;
        extract_release_binary(&tgz, "firecracker", &firecracker)?;
        extract_release_binary(&tgz, "jailer", &jailer)?;
        fs::remove_file(&tgz)?;
        verify(&firecracker, FIRECRACKER_BIN_SHA256)?;
        verify(&jailer, JAILER_BIN_SHA256)?;
    }

    let kernel = dest_dir.join(KERNEL_FILENAME);
    if verify(&kernel, KERNEL_SHA256).is_err() {
        download(KERNEL_URL, &kernel)?;
        verify(&kernel, KERNEL_SHA256)?;
    }

    Ok(ReleaseBinaries {
        firecracker,
        jailer,
        kernel,
    })
}

/// Runs `command`, returning its stdout on success. On failure, folds the
/// command's own debug form and stderr into one `io::Error` -- every
/// caller in this module wants exactly this shape, whether it needs the
/// output (`sha256sum`, `tar -O`) or just success/failure (`curl`), and so
/// does [`crate::install`]'s own `run` (`groupadd`/`systemctl`).
pub(crate) fn run_capturing(command: &mut Command) -> io::Result<Vec<u8>> {
    let output = command.output()?;
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

fn download(url: &str, dest: &Path) -> io::Result<()> {
    run_capturing(
        Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(dest)
            .arg(url),
    )
    .map(drop)
}

/// `sha256sum` of `path`, compared against `want_sha256`. A missing file
/// or a mismatch are both reported the same way (`Err`), since every
/// caller here treats "not verified" and "not present" identically: fetch
/// it (again).
fn verify(path: &Path, want_sha256: &str) -> io::Result<()> {
    let output = run_capturing(Command::new("sha256sum").arg(path))?;
    let got = String::from_utf8_lossy(&output);
    let got_hash = got.split_whitespace().next().unwrap_or_default();
    if got_hash != want_sha256 {
        return Err(io::Error::other(format!(
            "{}: sha256 mismatch: got {got_hash}, want {want_sha256}",
            path.display()
        )));
    }
    Ok(())
}

/// Extracts one `release-v1.17.0-x86_64/<name>-v1.17.0-x86_64` member from
/// the release tarball to `dest`, via `tar -O` (extract-to-stdout) so the
/// versioned upstream filename never has to appear on disk before this
/// crate's own unversioned name does.
fn extract_release_binary(tgz: &Path, name: &str, dest: &Path) -> io::Result<()> {
    let member = format!("{RELEASE_DIR}/{name}-v{FIRECRACKER_VERSION}-x86_64");
    let bytes = run_capturing(Command::new("tar").arg("-xzOf").arg(tgz).arg(&member))?;
    fs::write(dest, &bytes)?;
    let mut perms = fs::metadata(dest)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(dest, perms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_rejects_a_file_with_the_wrong_hash() {
        let dir = tempdir();
        let path = dir.join("some-file");
        std::fs::write(&path, b"hello").unwrap();
        let err = verify(&path, &"0".repeat(64)).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_accepts_a_file_with_the_right_hash() {
        let dir = tempdir();
        let path = dir.join("some-file");
        std::fs::write(&path, b"hello").unwrap();
        // sha256("hello")
        verify(
            &path,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        )
        .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_reports_a_missing_file_as_an_error_not_a_panic() {
        let dir = tempdir();
        let path = dir.join("does-not-exist");
        assert!(verify(&path, &"0".repeat(64)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn tempdir() -> PathBuf {
        crate::test_util::tempdir("cirro-release-test")
    }
}
