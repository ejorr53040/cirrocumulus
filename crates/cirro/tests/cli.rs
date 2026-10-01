use assert_cmd::Command;
use predicates::prelude::*;

fn cirro() -> Command {
    Command::cargo_bin("cirro").unwrap()
}

#[test]
fn help_lists_every_subcommand() {
    let mut assert = cirro().arg("--help").assert().success();
    for cmd in [
        "node", "server", "run", "ps", "logs", "ssh", "stop", "park", "wake", "top", "bench", "db",
    ] {
        assert = assert.stdout(predicate::str::is_match(format!(r"(?m)^\s+{cmd}\s")).unwrap());
    }
}

#[test]
fn unimplemented_subcommand_says_not_yet_and_fails() {
    cirro()
        .args(["ssh", "web"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cirro ssh: not yet implemented"));
}

/// A path that isn't there is a typo, not an image to look for in a
/// registry; nor is a directory, which can't be a rootfs.
#[test]
fn run_says_a_missing_or_directory_rootfs_is_not_a_rootfs() {
    let dir = std::env::temp_dir();
    for (arg, expected) in [
        (
            "./no-such-rootfs.ext4",
            "no rootfs at ./no-such-rootfs.ext4",
        ),
        ("/no/such/rootfs", "no rootfs at /no/such/rootfs"),
        (dir.to_str().unwrap(), "is a directory, not a rootfs"),
    ] {
        cirro()
            .env("CIRRO_SOCKET", "/nonexistent/agent.sock")
            .env("XDG_CACHE_HOME", "/nonexistent")
            .args(["run", "--name", "typo", arg, "--", "/bin/true"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected));
    }
}
