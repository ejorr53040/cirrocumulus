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
        .args(["run", "nginx:alpine"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cirro run: not yet implemented"));
}
