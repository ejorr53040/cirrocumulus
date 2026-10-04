# Fuzzing

Two [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) targets
for the inputs Cirrocumulus parses from outside:

| Target | Input | Checks |
| --- | --- | --- |
| `image_layer` | An OCI image layer (tar, or gzipped tar) from a registry | Merging it returns, and every path stays inside the rootfs |
| `guest_init_config` | guest-init's config, as it arrives over vsock | Parsing it returns |

```sh
rustup toolchain install nightly --profile minimal
cargo +nightly install cargo-fuzz --locked
cd fuzz
cargo +nightly fuzz run image_layer corpus/image_layer seeds/image_layer -- -max_total_time=300
cargo +nightly fuzz run guest_init_config -- -max_total_time=60
```

`seeds/image_layer` holds small real layers (directories, a symlink, a
whiteout, an opaque directory, gzip), without which the fuzzer rarely gets
past tar's header checks. libFuzzer writes what it finds to the first
directory it's given, so the seeds come second and stay as they are.

## Runs so far

| Date | Target | Time | Runs | Found |
| --- | --- | ---: | ---: | --- |
| 2026-10-04 | `image_layer` from the seeds | 5 min | 1.5 M (1397 edges) | nothing |
| 2026-10-04 | `image_layer`, no seeds | 5 min | 3.4 M | nothing |
| 2026-10-04 | `guest_init_config` | 1 min | 18.6 M | nothing |
