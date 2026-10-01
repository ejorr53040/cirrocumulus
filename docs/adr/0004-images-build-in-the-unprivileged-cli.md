# Images are pulled and built by the unprivileged CLI, not the Node agent

`cirro run <image>` pulls the image, flattens its layers and builds the ext4 rootfs in the `cirro` CLI process, as the calling user, and only then asks the Node agent to boot that rootfs. The agent, which runs as root, never parses an image. Layers are untrusted tarballs: path traversal, hardlinks out of the tree and device nodes are routine in hostile images, and parsing them as root is how one of those becomes a host compromise. Built this way, the worst a bad image can do is write where its caller already could, and the agent's trust boundary stays where #14 put it: a rootfs it opens with the caller's credentials.

## Considered options

- **Build in the agent.** One shared cache per Node and no per-user rebuilds, but every layer is parsed as root. Rejected for the reason above.
- **Extract to a directory, then `mke2fs -d <dir>`.** Keeping each file's owner and setuid bit would need root (or a user namespace) during extraction. `mke2fs -d <tarball>` (e2fsprogs 1.47.1 or later) takes owners and modes straight from the tar, so the build needs no privilege at all.

## Consequences

The image cache is per user (`$XDG_CACHE_HOME/cirro/images`), so two users on one Node each pull and build their own copy. Node-wide sharing waits for M10's chunk store. When M8 moves pulls onto Nodes, the same `cirro-image` library runs in whichever unprivileged process owns the build, never in the agent.
