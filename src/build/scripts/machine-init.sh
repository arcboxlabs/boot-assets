#!/bin/busybox sh
# Machine boot shim: PID 1 for distro machines, entered via
# `init=/sbin/arcbox-machine-init` on the kernel command line.
#
# Stages a pulled distro rootfs into a writable machine root and hands PID 1
# to the distro's own init:
#   overlay( lower = arcbox.machine_rootfs, ro squashfs
#            upper = arcbox.machine_data,   btrfs, first-boot formatted )
#   → arcbox-agent machine-init (networking) + arcbox-agent serve (vsock RPC)
#   → pivot_root + exec distro /sbin/init
#
# The `arcbox.machine_*` cmdline keys are owned by the arcbox repo
# (common/arcbox-constants/src/cmdline.rs); design in
# arcbox internal-docs/plans/machine-boot-shim.md. busybox `switch_root` is
# deliberately NOT used: it requires an initramfs root, and this shim boots
# from the EROFS block device — `pivot_root` is the block-root equivalent.

bb=/bin/busybox

fail() {
  $bb printf 'arcbox-machine-init: %s; powering off\n' "$1" > /dev/console 2>/dev/null || true
  $bb poweroff -f
}

# The kernel auto-mounts devtmpfs (CONFIG_DEVTMPFS_MOUNT) and may already
# have proc/sys, so these mounts are best-effort — a redundant mount returns
# EBUSY, which is success for our purposes. Verify each pseudo-fs is actually
# present afterward so a genuinely missing one still fails loudly.
$bb mount -t proc proc /proc 2>/dev/null
$bb mount -t sysfs sysfs /sys 2>/dev/null
$bb mount -t devtmpfs devtmpfs /dev 2>/dev/null
[ -e /proc/cmdline ] || fail 'proc unavailable'
[ -d /sys/class ] || fail 'sysfs unavailable'
[ -c /dev/null ] || fail 'devtmpfs unavailable'

# Interactive debug console (opt-in via arcbox.debug_console), same as rcS.
if $bb grep -q arcbox.debug_console /proc/cmdline; then
  $bb printf 'arcbox-machine-init: debug console enabled, spawning shell on /dev/hvc0\n' > /dev/console 2>/dev/null || true
  $bb setsid $bb sh </dev/hvc0 >/dev/hvc0 2>&1 &
fi

# Parse the machine contract from the kernel command line.
rootfs_dev=
rootfs_type=squashfs
data_dev=
for tok in $($bb cat /proc/cmdline); do
  case "$tok" in
    arcbox.machine_rootfs=*) rootfs_dev="${tok#arcbox.machine_rootfs=}" ;;
    arcbox.machine_rootfs_type=*) rootfs_type="${tok#arcbox.machine_rootfs_type=}" ;;
    arcbox.machine_data=*) data_dev="${tok#arcbox.machine_data=}" ;;
  esac
done
[ -n "$rootfs_dev" ] || fail 'arcbox.machine_rootfs missing from cmdline'
[ -n "$data_dev" ] || fail 'arcbox.machine_data missing from cmdline'

$bb mount -t virtiofs arcbox /arcbox || fail 'mount virtiofs arcbox failed'

# The EROFS root is read-only, so all scratch mount points live on a tmpfs
# over /mnt (a directory the boot-assets EROFS already ships). /newroot lives
# here too and becomes the machine root after pivot_root.
scratch=/mnt
$bb mount -t tmpfs tmpfs "$scratch" || fail 'mount tmpfs scratch failed'
$bb mkdir -p "$scratch/lower" "$scratch/data" "$scratch/newroot"
newroot="$scratch/newroot"

$bb mount -t "$rootfs_type" -o ro "$rootfs_dev" "$scratch/lower" \
  || fail "mount $rootfs_dev ($rootfs_type) failed"

# First boot: format the data disk when the btrfs magic (offset 65600,
# superblock at 64 KiB + 64) is absent.
if ! $bb dd if="$data_dev" bs=1 skip=65600 count=8 2>/dev/null | $bb grep -q '_BHRfS_M'; then
  /sbin/mkfs.btrfs -q "$data_dev" || fail "mkfs.btrfs $data_dev failed"
fi
$bb mount -t btrfs "$data_dev" "$scratch/data" || fail "mount $data_dev failed"
$bb mkdir -p "$scratch/data/upper" "$scratch/data/work"
$bb mount -t overlay overlay \
  -o "lowerdir=$scratch/lower,upperdir=$scratch/data/upper,workdir=$scratch/data/work" "$newroot" \
  || fail 'overlay mount failed'

# Stage host bits into the machine root: the VirtioFS share (agent binary +
# logs) and a static busybox for the agent's DHCP path — only when the
# distro does not ship one (Alpine does, systemd distros do not).
$bb mkdir -p "$newroot/arcbox"
$bb mount -o move /arcbox "$newroot/arcbox" || fail 'move /arcbox failed'
if [ ! -e "$newroot/bin/busybox" ]; then
  $bb mkdir -p "$newroot/bin"
  $bb cp /bin/busybox "$newroot/bin/busybox"
fi

# User mounts: replay the arcbox.machine_mounts= table (comma-separated
# tag=guest_path[:ro] entries) as virtiofs mounts inside the new root. A
# failed mount degrades that share only — the machine still boots and the
# host can inspect the failure over exec.
mounts_table=
for tok in $($bb cat /proc/cmdline); do
  case "$tok" in
    arcbox.machine_mounts=*) mounts_table="${tok#arcbox.machine_mounts=}" ;;
  esac
done
if [ -n "$mounts_table" ]; then
  $bb printf '%s\n' "$mounts_table" | $bb tr ',' '\n' | while read -r entry; do
    [ -n "$entry" ] || continue
    tag="${entry%%=*}"
    dest="${entry#*=}"
    opts=
    case "$dest" in
      *:ro) dest="${dest%:ro}"; opts='-o ro' ;;
    esac
    $bb mkdir -p "$newroot$dest"
    # shellcheck disable=SC2086 # $opts is intentionally word-split
    if ! $bb mount -t virtiofs $opts "$tag" "$newroot$dest"; then
      $bb printf 'arcbox-machine-init: mount %s -> %s failed; continuing\n' "$tag" "$dest" > /dev/console 2>/dev/null || true
    fi
  done
fi

# Hand the pseudo-filesystems over before anything runs inside the new root
# (the agent needs /dev for vsock and /proc for interface discovery). All
# three are load-bearing: a machine without them fails start in obscure
# ways, so power off cleanly instead.
$bb mkdir -p "$newroot/proc" "$newroot/sys" "$newroot/dev"
$bb mount -o move /proc "$newroot/proc" || fail 'move /proc failed'
$bb mount -o move /sys "$newroot/sys" || fail 'move /sys failed'
$bb mount -o move /dev "$newroot/dev" || fail 'move /dev failed'

# One-shot machine init (DHCP, resolver fallback) — non-fatal: a machine
# without networking is still reachable over vsock for diagnosis. Then the
# long-running agent, backgrounded; it survives the pivot (its root fd
# already points into the overlay) and is reparented to the distro init.
$bb chroot "$newroot" /arcbox/bin/arcbox-agent machine-init \
  || $bb printf 'arcbox-machine-init: machine-init failed; continuing\n' > "$newroot/dev/console" 2>/dev/null || true
# Redirect from inside the chroot: /dev was moved into the new root, so the
# shell (still on the old root) can't reliably open $newroot/dev/null, but
# the chrooted shell resolves /dev/null within the new root's devtmpfs. The
# agent logs to /arcbox/log/agent.log (VirtioFS), so discarding stdio here
# loses nothing.
$bb chroot "$newroot" /bin/busybox sh -c \
  '/arcbox/bin/arcbox-agent serve </dev/null >/dev/null 2>&1 &'

# Make the overlay the real root and the distro init PID 1. The old root
# lands on the distro's /mnt and is lazily detached.
cd "$newroot" || fail 'cd newroot failed'
$bb pivot_root . mnt || fail 'pivot_root failed'
$bb umount -l /mnt 2>/dev/null || true
exec $bb chroot . /sbin/init </dev/console >/dev/console 2>&1
