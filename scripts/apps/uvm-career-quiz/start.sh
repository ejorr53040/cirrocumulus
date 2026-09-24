#!/bin/sh
# Guest-side startup: guest-init execs this as the configured app. Brings
# up networking -- there's no DHCP server on the host<->guest tap link
# run.sh sets up, so a static address in the same /30 -- then execs the
# app server so its pid replaces this shell's own. guest-init only ever
# tracks the one pid it forked; exec(2) preserves that pid across the
# image replacement, so the SIGTERM forwarding path (RESEARCH.md M2 slice
# 5) still reaches the real server, not this shell.
# Flask's dev server resolves its own hostname once at startup, just to
# print it in the "Running on http://..." banner. With no DNS in this
# guest and no matching /etc/hosts entry, that resolution falls through to
# a real (bare-kernel default) hostname that isn't "localhost", and musl's
# resolver blocks for several seconds before giving up -- observed as a
# real ~5s hang before the app ever binds its socket. Setting the hostname
# to the one name /etc/hosts (from Alpine's alpine-baselayout package)
# already maps to 127.0.0.1 makes that lookup resolve instantly instead.
/bin/busybox hostname localhost

/bin/busybox ip link set lo up
/bin/busybox ip addr add 172.16.0.2/30 dev eth0
/bin/busybox ip link set eth0 up

cd /app
exec /usr/bin/python3 run_server.py
