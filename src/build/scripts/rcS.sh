#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox mkdir -p /arcbox
/bin/busybox mount -t virtiofs arcbox /arcbox

# Register FEX for amd64 Linux ELF binaries when the runtime bundle provides
# {{ FEX_BINARY }}. The POCF flags match upstream FEX's x86_64 binfmt entry: pass the
# original argv[0], pass the guest binary as an opened fd, preserve file
# credentials, and pin the interpreter at registration time. FEX_ROOTFS is left
# unset so OCI containers use their own amd64 rootfs for guest libraries.
if [ -x {{ FEX_BINARY }} ]; then
  /bin/busybox mkdir -p /proc/sys/fs/binfmt_misc
  if [ ! -e /proc/sys/fs/binfmt_misc/register ]; then
    /bin/busybox mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc 2>/dev/null || true
  fi
  if [ -e /proc/sys/fs/binfmt_misc/register ]; then
    if [ -e /proc/sys/fs/binfmt_misc/FEX-x86_64 ]; then
      /bin/busybox echo -1 > /proc/sys/fs/binfmt_misc/FEX-x86_64 2>/dev/null || true
    fi
    /bin/busybox ln -snf /proc/self/fd /dev/fd
    /bin/busybox printf '%s\n' '{{ FEX_X86_64_BINFMT_ENTRY }}' > /proc/sys/fs/binfmt_misc/register 2>/dev/null || true
  fi
fi

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
