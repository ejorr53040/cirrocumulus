# Contributing to Cirrocumulus

Thanks for helping. Bug reports, docs fixes and code are all welcome.

## Before you start

- **Bugs and features** go in [GitHub Issues](https://github.com/ejorr53040/cirrocumulus/issues).
  For anything bigger than a small fix, open an issue first so we can agree
  on the shape before you write it. Issues labelled `good first issue` are a
  good place to begin.
- **Security problems** don't go in issues: see [SECURITY.md](SECURITY.md).
- [CONTEXT.md](CONTEXT.md) is the project's vocabulary (Node, VM, App, Route,
  park, wake, ...). Code, docs and messages use those words, and avoid the
  ones it lists under _Avoid_.
- Decisions that would be hard to reverse are recorded as ADRs in
  [docs/adr](docs/adr).

## Building and testing

You need Linux x86_64 and `rustup`; `rust-toolchain.toml` pins the toolchain
and the musl target guest-init is built for.

```sh
scripts/ci/install-hooks.sh   # once: commit-msg and pre-push hooks
scripts/ci/local.sh           # the gates CI runs
```

The tests that boot real Firecracker VMs (`crates/cirro/tests/node_agent.rs`
and `crates/cirro-node/tests/`) need `/dev/kvm`, the kernel from `scripts/step0/fetch_kernel.sh`, and one
sudoers rule described at the top of that file. They skip, rather than fail,
without them, and CI only compiles them, so run them before sending a change
that touches VMs. New behaviour gets a test there, written first.

## Sending a change

- Branch from `main`, and keep each pull request to one change.
- Commit messages: a capitalized subject of at most 72 characters with no
  trailing period, a blank line, and a body wrapped at 100 columns that says
  why. No merge, WIP or fixup commits; every commit must build and pass
  clippy on its own. The commit-msg hook checks this.
- `CI OK` must pass. PRs are squash- or rebase-merged.
- New dependencies must pass `cargo deny` (`deny.toml`: permissive
  licenses, crates.io only, no open advisories).
- New `unsafe` needs a `// SAFETY:` comment saying why it's sound.

By contributing you agree that your work is licensed under
[Apache-2.0](LICENSE), like the rest of the project.
