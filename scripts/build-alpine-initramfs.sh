#!/usr/bin/env bash
# build-alpine-initramfs.sh — Build a minimal Alpine-standard initramfs for ArcBox.
# chmod +x boot-assets/scripts/build-alpine-initramfs.sh
#
# Phase 0 of the boot-assets refactor: ext4 block device rootfs.
#
# This initramfs is intentionally minimal. Its only job is:
#   1. Mount /proc, /sys, /dev.
#   2. Load bootstrap kernel modules (virtio, ext4, vsock, virtiofs, net).
#   3. Mount /dev/vda (ext4 rootfs) at /newroot.
#   4. exec switch_root /newroot /sbin/init  → standard Alpine OpenRC.
#
# Everything else (VirtioFS shares, networking, cgroups, Docker, arcbox-agent)
# is handled by OpenRC services inside the rootfs.
set -euo pipefail

BASE_INITRAMFS=""
MODLOOP=""
OUTPUT=""

usage() {
  cat <<'USAGE_EOF'
Usage: build-alpine-initramfs.sh [options]

Required options:
  --base-initramfs <path>  Path to base Alpine initramfs (provides busybox)
  --modloop <path>         Path to Alpine modloop image (provides kernel .ko files)
  --output <path>          Output initramfs path
USAGE_EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base-initramfs)
      BASE_INITRAMFS="$2"
      shift 2
      ;;
    --modloop)
      MODLOOP="$2"
      shift 2
      ;;
    --output)
      OUTPUT="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "$BASE_INITRAMFS" || -z "$MODLOOP" || -z "$OUTPUT" ]]; then
  usage >&2
  exit 1
fi

for file in "$BASE_INITRAMFS" "$MODLOOP"; do
  if [[ ! -f "$file" ]]; then
    echo "required file not found: $file" >&2
    exit 1
  fi
done

if ! command -v unsquashfs >/dev/null 2>&1; then
  echo "unsquashfs is required but not found in PATH" >&2
  exit 1
fi

WORK_DIR="$(mktemp -d /tmp/arcbox-initramfs.XXXXXX)"
MODLOOP_EXTRACT="$(mktemp -d /tmp/arcbox-modloop.XXXXXX)"

cleanup() {
  rm -rf "$WORK_DIR" "$MODLOOP_EXTRACT"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Extract the Alpine base initramfs (provides /bin/busybox).
# ---------------------------------------------------------------------------
echo "extract base initramfs: $BASE_INITRAMFS"
(
  cd "$WORK_DIR"
  if ! gunzip -c "$BASE_INITRAMFS" | cpio -idm 2>/dev/null; then
    cpio -idm < "$BASE_INITRAMFS" 2>/dev/null
  fi
)

# ---------------------------------------------------------------------------
# Extract Alpine modloop and copy only bootstrap modules into initramfs.
# ---------------------------------------------------------------------------
echo "extract modloop: $MODLOOP"
unsquashfs -f -d "$MODLOOP_EXTRACT" "$MODLOOP" >/dev/null 2>&1

KERNEL_VERSION="$(ls "$WORK_DIR/lib/modules/" 2>/dev/null | head -1 || true)"
if [[ -z "$KERNEL_VERSION" ]]; then
  KERNEL_VERSION="$(ls "$MODLOOP_EXTRACT/modules/" 2>/dev/null | head -1 || true)"
fi
if [[ -z "$KERNEL_VERSION" ]]; then
  echo "unable to detect kernel version from initramfs/modloop" >&2
  exit 1
fi
echo "kernel version: $KERNEL_VERSION"

copy_module() {
  local src_dir="$1"
  local dest_dir="$2"
  local mod_file="$3"
  local src="$src_dir/$mod_file"
  if [[ -f "$src" ]]; then
    mkdir -p "$dest_dir"
    cp "$src" "$dest_dir/"
  fi
}

MODS_SRC="$MODLOOP_EXTRACT/modules/$KERNEL_VERSION/kernel"
MODS_DST="$WORK_DIR/lib/modules/$KERNEL_VERSION/kernel"

# VirtIO core.
copy_module "$MODS_SRC/drivers/virtio" "$MODS_DST/drivers/virtio" virtio.ko
copy_module "$MODS_SRC/drivers/virtio" "$MODS_DST/drivers/virtio" virtio_ring.ko
copy_module "$MODS_SRC/drivers/virtio" "$MODS_DST/drivers/virtio" virtio_pci.ko
copy_module "$MODS_SRC/drivers/virtio" "$MODS_DST/drivers/virtio" virtio_pci_modern_dev.ko
copy_module "$MODS_SRC/drivers/virtio" "$MODS_DST/drivers/virtio" virtio_pci_legacy_dev.ko
copy_module "$MODS_SRC/drivers/virtio" "$MODS_DST/drivers/virtio" virtio_mmio.ko

# VirtIO console.
copy_module "$MODS_SRC/drivers/char" "$MODS_DST/drivers/char" virtio_console.ko

# VirtIO network.
copy_module "$MODS_SRC/drivers/net" "$MODS_DST/drivers/net" net_failover.ko
copy_module "$MODS_SRC/drivers/net" "$MODS_DST/drivers/net" virtio_net.ko

# VirtIO block (needed to access /dev/vda).
copy_module "$MODS_SRC/drivers/block" "$MODS_DST/drivers/block" virtio_blk.ko

# VirtioFS (needed for VirtioFS shares mounted later by OpenRC).
copy_module "$MODS_SRC/fs/fuse" "$MODS_DST/fs/fuse" fuse.ko
copy_module "$MODS_SRC/fs/fuse" "$MODS_DST/fs/fuse" virtiofs.ko

# vSock transport: must be loaded before switch_root because the kernel does
# not re-probe the virtio-vsock device after switch_root (no udev).
copy_module "$MODS_SRC/net/vmw_vsock" "$MODS_DST/net/vmw_vsock" vsock.ko
copy_module "$MODS_SRC/net/vmw_vsock" "$MODS_DST/net/vmw_vsock" vmw_vsock_virtio_transport_common.ko
copy_module "$MODS_SRC/net/vmw_vsock" "$MODS_DST/net/vmw_vsock" vmw_vsock_virtio_transport.ko

# ext4 filesystem (needed to mount /dev/vda rootfs).
copy_module "$MODS_SRC/fs/ext4" "$MODS_DST/fs/ext4" ext4.ko
copy_module "$MODS_SRC/fs/jbd2" "$MODS_DST/fs/jbd2" jbd2.ko
copy_module "$MODS_SRC/fs" "$MODS_DST/fs" mbcache.ko
copy_module "$MODS_SRC/lib" "$MODS_DST/lib" crc16.ko

# Module metadata so modprobe can resolve dependencies.
mkdir -p "$WORK_DIR/lib/modules/$KERNEL_VERSION"
cp "$MODLOOP_EXTRACT/modules/$KERNEL_VERSION/modules.dep" \
   "$WORK_DIR/lib/modules/$KERNEL_VERSION/modules.dep" 2>/dev/null \
   || echo "warning: modules.dep not found in modloop" >&2
cp "$MODLOOP_EXTRACT/modules/$KERNEL_VERSION/modules.alias" \
   "$WORK_DIR/lib/modules/$KERNEL_VERSION/modules.alias" 2>/dev/null || true

# ---------------------------------------------------------------------------
# Write the /init script.
# Minimal: mount virtio block device → switch_root to Alpine OpenRC.
# ---------------------------------------------------------------------------
cat > "$WORK_DIR/init" <<'INIT_EOF'
#!/bin/sh
# ArcBox initramfs init — mount ext4 rootfs from /dev/vda, switch_root to OpenRC.

/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox mkdir -p /dev/pts
/bin/busybox mount -t devpts devpts /dev/pts 2>/dev/null || true

# Wait for /dev/hvc0 and redirect console.
i=0
while [ "$i" -lt 20 ]; do
  [ -c /dev/hvc0 ] && break
  i=$((i + 1))
  /bin/busybox sleep 0.1
done
[ -c /dev/hvc0 ] && exec </dev/hvc0 >/dev/hvc0 2>&1

log() { echo "initramfs: $*"; }

KERNEL_VERSION="$(/bin/busybox uname -r)"
MODULE_DIR="/lib/modules/$KERNEL_VERSION"

load_ko() {
  local name="$1"
  local relpath="$2"
  /sbin/modprobe "$name" 2>/dev/null && return 0
  local full_path="$MODULE_DIR/$relpath"
  [ -f "$full_path" ] && /bin/busybox insmod "$full_path" 2>/dev/null && return 0
  return 0
}

log "Loading bootstrap kernel modules..."

# VirtIO core.
load_ko virtio                 "kernel/drivers/virtio/virtio.ko"
load_ko virtio_ring            "kernel/drivers/virtio/virtio_ring.ko"
load_ko virtio_pci_legacy_dev  "kernel/drivers/virtio/virtio_pci_legacy_dev.ko"
load_ko virtio_pci_modern_dev  "kernel/drivers/virtio/virtio_pci_modern_dev.ko"
load_ko virtio_pci             "kernel/drivers/virtio/virtio_pci.ko"
load_ko virtio_mmio            "kernel/drivers/virtio/virtio_mmio.ko"

# VirtIO console.
load_ko virtio_console         "kernel/drivers/char/virtio_console.ko"

# VirtIO block (for /dev/vda).
load_ko virtio_blk             "kernel/drivers/block/virtio_blk.ko"

# ext4 filesystem (for rootfs).
load_ko crc16                  "kernel/lib/crc16.ko"
load_ko mbcache                "kernel/fs/mbcache.ko"
load_ko jbd2                   "kernel/fs/jbd2/jbd2.ko"
load_ko ext4                   "kernel/fs/ext4/ext4.ko"

# vSock transport: must be loaded before switch_root (no udev to re-probe).
load_ko vsock                           "kernel/net/vmw_vsock/vsock.ko"
load_ko vmw_vsock_virtio_transport_common "kernel/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko"
load_ko vmw_vsock_virtio_transport      "kernel/net/vmw_vsock/vmw_vsock_virtio_transport.ko"

# VirtioFS (loaded now so OpenRC services can mount shares immediately).
load_ko fuse                   "kernel/fs/fuse/fuse.ko"
load_ko virtiofs               "kernel/fs/fuse/virtiofs.ko"

# VirtIO network.
load_ko net_failover           "kernel/drivers/net/net_failover.ko"
load_ko virtio_net             "kernel/drivers/net/virtio_net.ko"

# Wait for /dev/vda to appear (up to 2 seconds).
log "Waiting for /dev/vda..."
i=0
while [ "$i" -lt 20 ]; do
  [ -b /dev/vda ] && break
  i=$((i + 1))
  /bin/busybox sleep 0.1
done
if [ ! -b /dev/vda ]; then
  log "FATAL: /dev/vda not found after 2s"
  exec /bin/busybox sh
fi

# Mount ext4 rootfs.
log "Mounting /dev/vda as ext4..."
/bin/busybox mkdir -p /newroot
if ! /bin/busybox mount -t ext4 -o rw /dev/vda /newroot; then
  log "FATAL: cannot mount /dev/vda"
  exec /bin/busybox sh
fi

# Move already-mounted filesystems into the new root.
/bin/busybox mkdir -p /newroot/proc /newroot/sys /newroot/dev
/bin/busybox mount --move /proc /newroot/proc
/bin/busybox mount --move /sys  /newroot/sys
/bin/busybox mount --move /dev  /newroot/dev

# Pre-mount cgroup2 unified hierarchy so dockerd finds it immediately.
# OpenRC's cgroups service (sysinit) will detect this and skip re-mounting.
/bin/busybox mkdir -p /newroot/sys/fs/cgroup
/bin/busybox mount -t cgroup2 cgroup2 /newroot/sys/fs/cgroup 2>/dev/null || true

# Write fallback DNS resolvers so dockerd can resolve registries.
# DHCP may update this later, but dockerd needs DNS at startup.
if [ ! -f /newroot/etc/resolv.conf ] || ! /bin/busybox grep -q '^nameserver' /newroot/etc/resolv.conf 2>/dev/null; then
  echo 'nameserver 8.8.8.8' > /newroot/etc/resolv.conf
  echo 'nameserver 1.1.1.1' >> /newroot/etc/resolv.conf
fi

log "Switching root to /dev/vda (OpenRC)..."
exec /bin/busybox switch_root /newroot /sbin/init
INIT_EOF
chmod 755 "$WORK_DIR/init"

# ---------------------------------------------------------------------------
# Package the initramfs.
# ---------------------------------------------------------------------------
mkdir -p "$(dirname "$OUTPUT")"
(
  cd "$WORK_DIR"
  find . | cpio -o -H newc 2>/dev/null | gzip > "$OUTPUT"
)

echo "initramfs ready: $OUTPUT"
ls -lh "$OUTPUT"
