#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev

# Interactive debug console (opt-in via `arcbox.debug_console` on the kernel
# cmdline; the host wires the matching virtio-console socket). Spawn a root
# shell on the console BEFORE the blocking virtiofs mount below, so an operator
# can diagnose early-boot hangs that occur before the agent ever runs (the
# dominant HV cold-boot failure mode). Backgrounded in its own session so rcS
# continues into the normal boot; inert unless the cmdline opts in.
if /bin/busybox grep -q arcbox.debug_console /proc/cmdline; then
  /bin/busybox printf 'arcbox: debug console enabled, spawning shell on /dev/hvc0\n' > /dev/console 2>/dev/null || true
  /bin/busybox setsid /bin/busybox sh </dev/hvc0 >/dev/hvc0 2>&1 &
fi

/bin/busybox mkdir -p /arcbox
/bin/busybox mount -t virtiofs arcbox /arcbox

# One-shot guest system init (mounts, networking, /etc). On success `init` exits
# 0 and busybox init respawns the long-running agent via the inittab entry. A
# non-zero exit means a critical writable layer (/etc, /run, /var, /tmp) failed to
# mount, so the agent would run broken on the read-only EROFS rootfs — power off
# (force: bypass init, which is blocked waiting on this sysinit) with a clear
# console message so the host relaunches into a fresh boot instead of respawning
# a broken agent.
if ! {{ AGENT_BIN }} init; then
  /bin/busybox printf 'arcbox: guest system init failed; powering off for a clean retry (see /arcbox/log/agent.log)\n' > /dev/console 2>/dev/null || true
  /bin/busybox poweroff -f
fi
