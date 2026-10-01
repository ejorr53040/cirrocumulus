use assert_cmd::Command;
use predicates::prelude::*;

fn cirro() -> Command {
    Command::cargo_bin("cirro").unwrap()
}

#[test]
fn help_lists_every_subcommand() {
    let mut assert = cirro().arg("--help").assert().success();
    for cmd in [
        "node", "server", "run", "image", "ps", "logs", "ssh", "stop", "park", "wake", "top",
        "bench", "db",
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

fn empty_cache() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cirro-cli-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn image_ls_on_an_empty_cache_prints_just_the_header() {
    let output = cirro()
        .env("XDG_CACHE_HOME", empty_cache())
        .args(["image", "ls"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(lines, [["REFERENCE", "DIGEST", "SIZE"]]);
}

#[test]
fn image_rm_of_an_image_thats_not_cached_fails() {
    cirro()
        .env("XDG_CACHE_HOME", empty_cache())
        .args(["image", "rm", "nginx:alpine"])
        .assert()
        .failure()
        .stderr("cirro: no cached image nginx:alpine\n");
}
