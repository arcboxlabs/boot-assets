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

$bb mount -t proc proc /proc || fail 'mount /proc failed'
$bb mount -t sysfs sysfs /sys || fail 'mount /sys failed'
$bb mount -t devtmpfs devtmpfs /dev || fail 'mount /dev failed'

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

$bb mkdir -p /arcbox
$bb mount -t virtiofs arcbox /arcbox || fail 'mount virtiofs arcbox failed'

$bb mkdir -p /lower /data /newroot
$bb mount -t "$rootfs_type" -o ro "$rootfs_dev" /lower || fail "mount $rootfs_dev ($rootfs_type) failed"

# First boot: format the data disk when the btrfs magic (offset 65600,
# superblock at 64 KiB + 64) is absent.
if ! $bb dd if="$data_dev" bs=1 skip=65600 count=8 2>/dev/null | $bb grep -q '_BHRfS_M'; then
  /sbin/mkfs.btrfs -q "$data_dev" || fail "mkfs.btrfs $data_dev failed"
fi
$bb mount -t btrfs "$data_dev" /data || fail "mount $data_dev failed"
$bb mkdir -p /data/upper /data/work
$bb mount -t overlay overlay \
  -o "lowerdir=/lower,upperdir=/data/upper,workdir=/data/work" /newroot \
  || fail 'overlay mount failed'

# Stage host bits into the machine root: the VirtioFS share (agent binary +
# logs) and a static busybox for the agent's DHCP path — only when the
# distro does not ship one (Alpine does, systemd distros do not).
$bb mkdir -p /newroot/arcbox
$bb mount -o move /arcbox /newroot/arcbox || fail 'move /arcbox failed'
if [ ! -e /newroot/bin/busybox ]; then
  $bb mkdir -p /newroot/bin
  $bb cp /bin/busybox /newroot/bin/busybox
fi

# Hand the pseudo-filesystems over before anything runs inside the new root
# (the agent needs /dev for vsock and /proc for interface discovery).
$bb mount -o move /proc /newroot/proc || fail 'move /proc failed'
$bb mount -o move /sys /newroot/sys
$bb mount -o move /dev /newroot/dev

# One-shot machine init (DHCP, resolver fallback) — non-fatal: a machine
# without networking is still reachable over vsock for diagnosis. Then the
# long-running agent; it survives the pivot (its root fd already points into
# the overlay) and is reparented to the distro init.
$bb chroot /newroot /arcbox/bin/arcbox-agent machine-init \
  || $bb printf 'arcbox-machine-init: machine-init failed; continuing\n' > /newroot/dev/console 2>/dev/null || true
$bb chroot /newroot /arcbox/bin/arcbox-agent serve </dev/null >/dev/null 2>&1 &

# Make the overlay the real root and the distro init PID 1. The old EROFS
# root lands on /mnt and is lazily detached.
cd /newroot || fail 'cd /newroot failed'
$bb pivot_root . mnt || fail 'pivot_root failed'
$bb umount -l /mnt 2>/dev/null || true
exec $bb chroot . /sbin/init </dev/console >/dev/console 2>&1
