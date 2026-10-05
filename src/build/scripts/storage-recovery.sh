#!/bin/busybox sh
# The host selects this PID 1 with init=/sbin/arcbox-storage-recovery.
# Do not enter the normal boot sequence or mount persistent storage here.

bb=/bin/busybox
agent={{ AGENT_BIN }}

fail() {
  $bb printf 'arcbox-storage-recovery: %s; powering off\n' "$1" > /dev/console 2>/dev/null || true
  $bb poweroff -f
  exit 1
}

$bb mountpoint -q /proc || $bb mount -t proc proc /proc || fail 'mount proc failed'
$bb mountpoint -q /sys || $bb mount -t sysfs sysfs /sys || fail 'mount sysfs failed'
$bb mountpoint -q /dev || $bb mount -t devtmpfs devtmpfs /dev || fail 'mount devtmpfs failed'

case " $($bb cat /proc/cmdline) " in
  *" arcbox.storage_recovery=1 "*) ;;
  *) fail 'arcbox.storage_recovery=1 is required' ;;
esac

$bb mount -t virtiofs arcbox /arcbox || fail 'mount virtiofs arcbox failed'

# Older agents start normal services for unknown commands. Inspect the binary
# before execution; a command probe could start those services during recovery.
$bb grep -aFq 'arcbox-storage-recovery-v1' "$agent" \
  || fail 'guest agent does not support storage recovery'

exec "$agent" storage-recovery
