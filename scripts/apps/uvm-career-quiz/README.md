# Demo: hosting UVM-Career-Quiz

End-to-end proof that this project can host a real third-party app
(https://github.com/VERSO-UVM/UVM-Career-Quiz, a Flask + sqlite site) in a
Firecracker microVM, driven by `crates/guest-init` (M2) as `/init`: boot it,
reach it over real TCP/IP, log in, load an authenticated page, then ask it to
shut down and confirm it does so cleanly.

This is a shell-script proof, the same way `scripts/step0/` proved Firecracker
+ jailer worked before any Rust got written for it. It doesn't use
`cirro-node`/`cirro-image` (M3/M4) — those aren't built yet — so there's no
OCI image pulling, no chunked storage, no jailer/cgroups isolation here, and
the packaging (Alpine + `apk.static`, not a Dockerfile) is one-off for this
app rather than the general pipeline M4 will build.

## Why Alpine + `apk.static`, not Docker

`guest-init` alone (a bare musl static binary, per M2) is enough for the
fixture apps in `scripts/step0/`, but a real Flask app needs a real Python
runtime, so this rootfs needs actual userspace packages, not just one
statically-linked binary. Docker wasn't usable in the environment this was
built in (daemon not running, no root to start it), so instead:
[`apk.static`](https://pkgs.alpinelinux.org/package/edge/main/x86_64/apk-tools-static)
— a fully static Alpine package-manager binary — installs `python3` and
`py3-flask` straight into a target directory (`apk.static --root <dir>`),
as a plain file-write operation, no chroot or root required. Its
post-install/trigger scripts (busybox's symlink farm, `chown`/setuid
bookkeeping) do need root and fail here — expected and harmless for a
single-process-as-PID-1 guest that always runs as root and calls busybox
applets directly (`/bin/busybox ip ...`) rather than through symlinks.

## Why no root, anywhere

Both privileged-looking steps turn out not to need real root:

- **Package install** (above): `apk.static --root` is a plain file
  operation; only its post-install scripts want root, and this guest
  doesn't need what they'd do.
- **Tap networking**: creating a tap device normally needs `CAP_NET_ADMIN`
  (confirmed by hand: `ip tuntap add` as a plain user fails with "Operation
  not permitted"). An unprivileged user+network namespace grants that
  capability *inside the namespace* without touching the host at all:
  `unshare --net --user --map-root-user -- ip tuntap add dev t0 mode tap`
  succeeds. `demo.sh` runs entirely inside one such namespace.

## Usage

```sh
./fetch_app.sh                                   # clone the app (pinned commit)
./build_rootfs.sh                                # apk install + package the rootfs (~1-2 min)
unshare --net --user --map-root-user -- ./demo.sh   # boot, curl it, shut it down
```

`demo.sh` refuses to run outside the namespace (checks `id -u`) rather than
silently failing partway through tap setup.

## What `demo.sh` actually checks

1. `HTTP_REACHABLE` — the app answers on `172.16.0.2:5000` at all.
2. `LOGIN_PAGE_RENDERED` — `GET /` renders the real login template.
3. A login round trip: `POST /quiz_login` with one of the two users
   `database_interaction.py`'s `create_db()` seeds (`1`/`1`), then
   `GET /quiz_selection` with the resulting session cookie —
   `AUTHENTICATED_ROUND_TRIP` passes only if the response actually contains
   the seeded quiz's name, i.e. real app logic and its sqlite-backed
   session/role checks ran, not just "some page rendered."
4. `SendCtrlAltDel` (Firecracker's own graceful-shutdown action) →
   `SIGTERM_FORWARDED_TO_APP`, `FIRECRACKER_EXITED_CLEANLY`,
   `NO_KERNEL_PANIC` — the M2 slice 5 path (see `RESEARCH.md` M2), proving
   the shutdown was requested and handled, not a hard reset that happened
   to also look clean.

## Known rough edges

- **`run_server.py`** is a launcher we add (not upstream): `app.py` defines
  a Flask `app` object but has no `if __name__ == "__main__"` block of its
  own (normally run via `flask run`).
- **Guest hostname is forced to `localhost`** in `start.sh`. Flask's dev
  server resolves its own hostname once at startup just to print it in the
  "Running on http://..." banner; with no DNS in this guest and a
  non-matching hostname, musl's resolver blocked for ~5s before giving up.
  Setting the hostname to the one name Alpine's `/etc/hosts` already maps
  to `127.0.0.1` makes that resolve instantly instead.
- **Static IP, no DHCP**: `start.sh` hardcodes `172.16.0.2/30`, matching
  `demo.sh`'s tap-side `172.16.0.1/30` — fine for one guest on one
  point-to-point link, not how M3's real networking (DHCP or the control
  plane assigning addresses) will work.
- **`secure_login_template.py`** (upstream, unused dead code — `pyargon2`
  and `dotenv` aren't installed here) is harmless to ship since nothing
  imports it, but isn't excluded either; not worth the special-casing for
  a demo script.
