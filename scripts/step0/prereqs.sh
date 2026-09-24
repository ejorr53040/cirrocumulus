#!/usr/bin/env bash
# Step 0 prereq check: is this machine able to run Firecracker microVMs under
# jailer at all, before any Rust gets written? Exits 0 if every check passes,
# 1 otherwise, printing a pass/fail line per check either way.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

status=0

check() {
    local name="$1" ok="$2" detail="$3"
    if [ "$ok" -eq 0 ]; then
        echo "PASS: $name -- $detail"
    else
        echo "FAIL: $name -- $detail"
        status=1
    fi
}

# --- KVM ---
if [ -e /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    check "KVM device" 0 "/dev/kvm present and read/write for $(whoami)"
else
    check "KVM device" 1 "/dev/kvm missing or not read/write for $(whoami)"
fi

if grep -Eq '(vmx|svm)' /proc/cpuinfo; then
    check "KVM virtualization extension" 0 "vmx/svm flag present in /proc/cpuinfo"
else
    check "KVM virtualization extension" 1 "no vmx/svm flag in /proc/cpuinfo"
fi

# --- cgroups (jailer defaults to v1; needs --cgroup-version=2 on this host) ---
cgroup_fs="$(stat -f -c %T /sys/fs/cgroup 2>/dev/null || echo unknown)"
if [ "$cgroup_fs" = "cgroup2fs" ]; then
    check "cgroup version" 0 "unified cgroup v2 hierarchy at /sys/fs/cgroup (jailer needs --cgroup-version=2)"
elif [ "$cgroup_fs" = "tmpfs" ]; then
    check "cgroup version" 0 "cgroup v1 hierarchy (jailer default)"
else
    check "cgroup version" 1 "could not determine cgroup version ($cgroup_fs)"
fi

# --- rust toolchain ---
if command -v rustc >/dev/null 2>&1; then
    check "rustc" 0 "$(rustc --version)"
else
    check "rustc" 1 "rustc not found on PATH"
fi

# --- firecracker / jailer binaries ---
firecracker_bin="$REPO_ROOT/firecracker"
jailer_bin="$REPO_ROOT/jailer"

if [ -x "$firecracker_bin" ]; then
    check "firecracker binary" 0 "$("$firecracker_bin" --version 2>&1 | head -1)"
else
    check "firecracker binary" 1 "not executable at $firecracker_bin"
fi

if [ -x "$jailer_bin" ]; then
    check "jailer binary" 0 "$("$jailer_bin" --version 2>&1 | head -1)"
else
    check "jailer binary" 1 "not executable at $jailer_bin"
fi

# --- passwordless root for jailer (jailer needs root: chroot, cgroups, chown of /dev/kvm and /dev/net/tun) ---
if sudo -n "$jailer_bin" --version >/dev/null 2>&1; then
    check "passwordless sudo for jailer" 0 "sudo -n $jailer_bin works"
else
    check "passwordless sudo for jailer" 1 "sudo -n $jailer_bin failed; jailer needs root to chroot/chown devices/set up cgroups"
fi

# --- guest tools needed to build a bootable rootfs ---
if command -v busybox >/dev/null 2>&1; then
    check "busybox" 0 "$(busybox | head -1)"
else
    check "busybox" 1 "busybox not found on PATH"
fi

if command -v mkfs.ext4 >/dev/null 2>&1; then
    check "mkfs.ext4" 0 "$(command -v mkfs.ext4)"
else
    check "mkfs.ext4" 1 "mkfs.ext4 not found on PATH"
fi

exit "$status"
