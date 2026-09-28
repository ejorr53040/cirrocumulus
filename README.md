# Cirrocumulus

[![CI](https://github.com/ejorr53040/cirrocumulus/actions/workflows/ci.yml/badge.svg)](https://github.com/ejorr53040/cirrocumulus/actions/workflows/ci.yml)

**A secure, tileable mini cloud you run yourself.**
Every workload gets its own Firecracker microVM, locked down with jailer.
Manage everything from your terminal.

## Development

CI (`.github/workflows/ci.yml`) is strict and every gate blocks a merge:
rustfmt, clippy and rustdoc with warnings denied, tests, shellcheck,
typos, actionlint, cargo-deny, unused-dependency checks, repo hygiene
(no binaries, large files, CRLF, or trailing whitespace), and commit
messages (capitalized subject of 72 chars or fewer, no trailing period,
no merge or fixup commits). On PRs, every commit must pass clippy on its
own, not just the tip.

Run the same gates locally, and install the git hooks that run them for you:

```sh
scripts/ci/local.sh           # everything CI runs (--quick skips docs + tests)
scripts/ci/install-hooks.sh   # commit-msg + pre-push hooks
```

The real-Firecracker tests in `crates/cirro-node/tests` are compiled in CI
but only run locally, since they need VM assets: see `scripts/step0/README.md`.
