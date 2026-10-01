# A Firecracker rootfs from a Docker image

`cirro run` boots a VM straight from an OCI image, such as one from Docker Hub:

```sh
cirro run --name web nginx:alpine
curl http://<the VM address it prints>/
```

No Dockerfile, no hand-built disk image, and no root outside the Node agent. This page
explains what happens between the image reference and the boot, and what it can't do yet.

## What `cirro run <image>` does

1. **Resolves the reference.** `nginx:alpine` means `docker.io/library/nginx:alpine`;
   `ghcr.io/owner/app:1.2` and `name@sha256:…` (a pinned digest) work too. The registry is
   asked for the manifest anonymously. If the image is multi-platform, cirro picks its
   `linux/amd64` image.
2. **Checks the cache.** Built images are kept per user in `$XDG_CACHE_HOME/cirro/images`
   (`~/.cache/cirro/images`), one per image digest. A cached image costs one manifest
   request: the tag is checked every time, so a pushed update is noticed, but nothing
   is downloaded twice.
3. **Downloads the config and layers.** Each blob's sha256 is checked before it's used.
4. **Flattens the layers** into the image's final filesystem tree. Later layers replace
   earlier files, and `.wh.` whiteouts and opaque directories delete them, as the OCI
   spec says. A path that would leave the image (`..`, or a hardlink pointing outside
   it) fails the build.
5. **Adds what every guest needs.** It adds `guest-init` as `/init`, the `/proc`, `/sys`
   and `/dev` mountpoints, an `/etc/resolv.conf` with public DNS servers (the guest has
   no DHCP), and an `/etc/hosts` with `localhost`.
6. **Builds ext4** with `mke2fs -d`, from a tarball of that tree. The filesystem is the
   image's size plus 25%, and at least 64 MiB.
7. **Asks the Node agent to boot it.** The command is the image's `Entrypoint` and `Cmd`,
   in its `WorkingDir`, with its `Env`, as its `User`, so the VM runs what `docker run`
   would.

Flags override the image the way Docker's do:

- Arguments after `--` replace `Cmd`; the `Entrypoint` stays.
- `-e KEY=VALUE` replaces or adds one variable.
- `-w` sets the working directory, and `-u UID:GID` the user.

```sh
cirro run --name check nginx:alpine -- nginx -t
```

## Why it builds without root

Image layers are untrusted tarballs. Everything above runs in the `cirro` CLI as you, not
in the root Node agent, so a malicious image can at worst write where you already can
([ADR 0004](adr/0004-images-build-in-the-unprivileged-cli.md)). The agent opens the
finished rootfs with your credentials, too.

The trick that makes this possible is `mke2fs -d <tarball>`. It takes each file's owner,
mode and setuid bit from the tar, so the filesystem keeps root-owned `/usr/bin/su` and
nginx's own user without the build ever running as root. Unpacking to a directory first
would need root to keep that ownership. This needs **e2fsprogs 1.47.1 or later**: Arch,
Fedora and Debian 13 have it, while Ubuntu 24.04 and Debian 12 ship 1.47.0. `cirro node
install` and the builder both check the version. `mke2fs` reads the tarball through
libarchive, which it loads only when needed: on Debian, install `libarchive13t64` too
if you skip recommended packages.

## Managing the cache

```sh
cirro image pull nginx:alpine   # pull and build ahead of time; prints the digest
cirro image ls                  # REFERENCE, DIGEST, SIZE
cirro image rm nginx:alpine     # by a reference it was pulled as, or by digest prefix
```

When a tag moves to a new image, the old one stays cached, and shows as `-` once no
reference names it, until you remove it.

## Limits

- **Public images only.** There's no registry login yet, so a private repository and a
  missing one look the same: `no image …: the registry has no repository …, or it's
  private`.
- **linux/amd64 only.** Other images are refused by name, with the platforms they do
  have.
- **gzip or uncompressed layers.** zstd layers are refused before any layer is downloaded.
- **Docker Hub's anonymous limit** is about 100 pulls per 6 hours per IP address. Cached
  images keep you well under it; past it, `cirro` says so and suggests `cirro image ls`.
- **Each VM gets a copy of the rootfs.** That's fine at nginx's size. A shared read-only
  rootfs, and storage shared across users and Nodes, come with the chunked image store.

Before images worked, `scripts/apps/uvm-career-quiz/build_rootfs.sh` built a rootfs by
hand from Alpine packages. It still works, and `cirro run <rootfs.ext4> -- <command>`
still boots any ext4 with `guest-init` as `/init`.
