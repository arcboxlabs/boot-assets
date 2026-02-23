#!/usr/bin/env bash
# build-alpine-rootfs.sh — Build a standard Alpine rootfs ext4 image using Docker.
#
# This replaces the two-stage initramfs + squashfs approach with a single ext4
# root filesystem image. The image contains a full Alpine userspace with OpenRC,
# Docker (dockerd + containerd + runc), networking, NTP, and the arcbox-agent
# service.
#
# The ext4 image is created entirely inside Docker containers so the script
# works on macOS (no root/loop device required on the host).
#
# Usage:
#   ./build-alpine-rootfs.sh --output rootfs.ext4
#   ./build-alpine-rootfs.sh --output rootfs.ext4 --size 4096
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

OUTPUT=""
SIZE_MB=2048
ALPINE_VERSION="3.21"
DOCKER_IMAGE_TAG="arcbox-rootfs-builder"

usage() {
  cat <<'EOF'
Usage: build-alpine-rootfs.sh [options]

Required options:
  --output <path>          Output path for the ext4 rootfs image

Optional:
  --size <MB>              Image size in megabytes (default: 2048)
  --alpine-version <ver>   Alpine version (default: 3.21)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      OUTPUT="$2"
      shift 2
      ;;
    --size)
      SIZE_MB="$2"
      shift 2
      ;;
    --alpine-version)
      ALPINE_VERSION="$2"
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

if [[ -z "$OUTPUT" ]]; then
  usage >&2
  exit 1
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required but not found in PATH" >&2
  exit 1
fi

WORK_DIR="$(mktemp -d /tmp/arcbox-alpine-rootfs.XXXXXX)"

cleanup() {
  rm -rf "$WORK_DIR"
  # Remove intermediate Docker image (best-effort).
  docker rmi "$DOCKER_IMAGE_TAG" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "========================================"
echo "  ArcBox Alpine Rootfs Build"
echo "========================================"
echo ""
echo "  Alpine Version: $ALPINE_VERSION"
echo "  Image Size:     ${SIZE_MB}MB"
echo "  Output:         $OUTPUT"
echo ""

# ---------------------------------------------------------------------------
# Phase 1: Build the rootfs container image via Docker.
# ---------------------------------------------------------------------------
echo "==> building rootfs container image"

cat > "$WORK_DIR/Dockerfile" <<DOCKERFILE_EOF
FROM alpine:${ALPINE_VERSION}

# Install core packages.
RUN apk add --no-cache \
    openrc \
    docker docker-openrc \
    iptables ip6tables \
    chrony \
    e2fsprogs \
    busybox-extras

# Enable OpenRC services.
RUN rc-update add docker default && \
    rc-update add chronyd default && \
    rc-update add networking default

# Configure kernel modules to load at boot.
RUN printf '%s\n' \
    vsock \
    vmw_vsock_virtio_transport_common \
    vmw_vsock_virtio_transport \
    overlay \
    ip_tables \
    iptable_nat \
    iptable_filter \
    nf_conntrack \
    > /etc/modules

# Configure network interfaces.
RUN printf '%s\n' \
    'auto lo' \
    'iface lo inet loopback' \
    '' \
    'auto eth0' \
    'iface eth0 inet dhcp' \
    > /etc/network/interfaces

# Configure Docker daemon.
RUN mkdir -p /etc/docker && \
    printf '%s\n' \
    '{' \
    '  "storage-driver": "overlay2"' \
    '}' \
    > /etc/docker/daemon.json

# Configure sysctl for container networking.
RUN mkdir -p /etc/sysctl.d && \
    echo 'net.ipv4.ip_forward = 1' > /etc/sysctl.d/99-arcbox.conf

# Create mount points.
RUN mkdir -p /arcbox /host-home

# Install arcbox-agent OpenRC service.
COPY arcbox-agent.initd /etc/init.d/arcbox-agent
RUN chmod +x /etc/init.d/arcbox-agent && \
    rc-update add arcbox-agent default

# Install VirtioFS mount script (runs at boot via local service).
COPY arcbox.start /etc/local.d/arcbox.start
RUN chmod +x /etc/local.d/arcbox.start && \
    rc-update add local default

# Ensure hostname is set.
RUN echo 'arcbox-vm' > /etc/hostname
DOCKERFILE_EOF

# Write the arcbox-agent OpenRC init script.
cat > "$WORK_DIR/arcbox-agent.initd" <<'INITD_EOF'
#!/sbin/openrc-run
# OpenRC service script for arcbox-agent.

description="ArcBox guest agent"

command="/arcbox/bin/arcbox-agent"
command_background=true
pidfile="/run/arcbox-agent.pid"
output_log="/arcbox/agent.log"
error_log="/arcbox/agent.log"

depend() {
    need net docker
    after docker
}

start_pre() {
    # Mount VirtioFS arcbox share if not already mounted.
    if ! mountpoint -q /arcbox; then
        ebegin "Mounting VirtioFS arcbox share"
        mount -t virtiofs arcbox /arcbox
        eend $?
    fi

    # Check that the agent binary exists.
    if [ ! -x "$command" ]; then
        eerror "arcbox-agent binary not found at $command"
        return 1
    fi
}
INITD_EOF

# Write the VirtioFS mount script for /etc/local.d/.
cat > "$WORK_DIR/arcbox.start" <<'LOCALSTART_EOF'
#!/bin/sh
# Mount VirtioFS shares on boot.
# This runs via the OpenRC "local" service.

# Mount arcbox share.
if ! mountpoint -q /arcbox; then
    mount -t virtiofs arcbox /arcbox 2>/dev/null || true
fi

# Mount host home share.
if ! mountpoint -q /host-home; then
    mount -t virtiofs home /host-home 2>/dev/null || true
fi
LOCALSTART_EOF

docker build -t "$DOCKER_IMAGE_TAG" "$WORK_DIR"

# ---------------------------------------------------------------------------
# Phase 2: Export rootfs tarball from the container.
# ---------------------------------------------------------------------------
echo "==> exporting rootfs tarball"

CONTAINER_ID="$(docker create "$DOCKER_IMAGE_TAG" /bin/true)"
docker export "$CONTAINER_ID" > "$WORK_DIR/rootfs.tar"
docker rm "$CONTAINER_ID" >/dev/null

echo "  rootfs tarball: $(du -h "$WORK_DIR/rootfs.tar" | awk '{print $1}')"

# ---------------------------------------------------------------------------
# Phase 3: Create ext4 image using a privileged Alpine container.
#
# We cannot create loop devices on macOS, so we run a privileged container
# that creates the ext4 image, mounts it via loopback, and extracts the
# rootfs tarball into it.
# ---------------------------------------------------------------------------
echo "==> creating ext4 image (${SIZE_MB}MB)"

mkdir -p "$(dirname "$OUTPUT")"

docker run --rm --privileged \
  -v "$WORK_DIR:/work" \
  "alpine:${ALPINE_VERSION}" \
  sh -c "
    set -e
    apk add --no-cache e2fsprogs tar >/dev/null 2>&1

    # Create sparse ext4 image.
    dd if=/dev/zero of=/work/rootfs.ext4 bs=1M count=0 seek=${SIZE_MB} 2>/dev/null
    mkfs.ext4 -F -L arcbox-rootfs /work/rootfs.ext4 >/dev/null

    # Mount and extract rootfs.
    mkdir -p /mnt/rootfs
    mount -o loop /work/rootfs.ext4 /mnt/rootfs
    tar -xf /work/rootfs.tar -C /mnt/rootfs

    # Sync and unmount.
    sync
    umount /mnt/rootfs

    echo 'ext4 image created successfully'
  "

# Move the image to the final output path.
mv "$WORK_DIR/rootfs.ext4" "$OUTPUT"

echo ""
echo "========================================"
echo "  Build Complete!"
echo "========================================"
echo ""
echo "  Output: $OUTPUT"
ls -lh "$OUTPUT"
