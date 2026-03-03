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
MODLOOP=""
DOCKER_IMAGE_TAG="arcbox-rootfs-builder"

usage() {
  cat <<'EOF'
Usage: build-alpine-rootfs.sh [options]

Required options:
  --output <path>          Output path for the ext4 rootfs image

Optional:
  --size <MB>              Image size in megabytes (default: 2048)
  --alpine-version <ver>   Alpine version (default: 3.21)
  --modloop <path>         Path to Alpine modloop squashfs (provides /lib/modules)
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
    --modloop)
      MODLOOP="$2"
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
    busybox-extras \
    nfs-utils

# Enable OpenRC services across all required runlevels.
# sysinit: devfs (populate /dev), dmesg (kernel log buffer), cgroups (mount cgroup2).
# boot: modules (/etc/modules loading), sysctl (ip_forward etc), bootmisc, hostname.
# default: networking, docker, chrony, arcbox-agent, local.
RUN rc-update add devfs sysinit && \
    rc-update add procfs sysinit && \
    rc-update add sysfs sysinit && \
    rc-update add dmesg sysinit && \
    rc-update add cgroups sysinit && \
    rc-update add modules boot && \
    rc-update add sysctl boot && \
    rc-update add bootmisc boot && \
    rc-update add hostname boot && \
    rc-update add networking default && \
    rc-update add docker default && \
    rc-update add chronyd default && \
    rc-update add rpcbind boot && \
    rc-update add nfs default

# Add serial console on hvc0 (virtio console) for boot diagnostics.
RUN sed -i '/^#ttyS0/a hvc0::respawn:/sbin/getty 115200 hvc0' /etc/inittab || true

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
    xt_addrtype \
    xt_conntrack \
    xt_MASQUERADE \
    br_netfilter \
    bridge \
    veth \
    sunrpc \
    lockd \
    nfsd \
    > /etc/modules

# Configure network interfaces.
RUN printf '%s\n' \
    'auto lo' \
    'iface lo inet loopback' \
    '' \
    'auto eth0' \
    'iface eth0 inet dhcp' \
    > /etc/network/interfaces

# Provide fallback DNS resolvers until DHCP populates /etc/resolv.conf.
RUN echo -e 'nameserver 8.8.8.8\nnameserver 1.1.1.1' > /etc/resolv.conf || true

# Configure Docker daemon.
RUN mkdir -p /etc/docker && \
    printf '%s\n' \
    '{' \
    '  "storage-driver": "overlay2",' \
    '  "dns": ["8.8.8.8", "1.1.1.1"],' \
    '  "log-driver": "json-file",' \
    '  "log-opts": {' \
    '    "max-size": "10m",' \
    '    "max-file": "3"' \
    '  }' \
    '}' \
    > /etc/docker/daemon.json

# Redirect Docker and containerd logs to VirtioFS mount for host-side debugging.
# The Alpine init scripts use log_proxy with these specific variable names.
RUN mkdir -p /etc/conf.d && \
    printf '%s\n' \
    'DOCKER_LOGFILE="/arcbox/dockerd.log"' \
    > /etc/conf.d/docker && \
    printf '%s\n' \
    'log_file="/arcbox/containerd.log"' \
    > /etc/conf.d/containerd

# Configure sysctl for container networking.
RUN mkdir -p /etc/sysctl.d && \
    echo 'net.ipv4.ip_forward = 1' > /etc/sysctl.d/99-arcbox.conf

# Export Docker data directory read-only over NFSv4.
RUN echo '/var/lib/docker *(ro,fsid=0,no_subtree_check,no_root_squash,crossmnt)' > /etc/exports

# Force NFSv4-only: disable v2/v3, run 8 server threads.
RUN sed -i 's/^OPTS_RPC_NFSD=.*/OPTS_RPC_NFSD="-N 2 -N 3 8"/' /etc/conf.d/nfs

# Ensure cgroup2 unified mode for Docker.
# Alpine OpenRC cgroups service must mount the unified hierarchy before dockerd.
# The rc_cgroup_mode setting forces cgroup2 (unified) which Docker 20.10+ supports.
RUN mkdir -p /etc/rc.conf.d && \
    echo 'rc_cgroup_mode="unified"' > /etc/rc.conf.d/cgroups

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

# Mount /Users share for transparent macOS path support.
# This allows `docker run -v /Users/foo/project:/app` to work directly.
if ! mountpoint -q /Users; then
    mkdir -p /Users
    mount -t virtiofs users /Users 2>/dev/null || true
fi

# Legacy: mount host home share if /Users share is not available.
if ! mountpoint -q /Users && ! mountpoint -q /host-home; then
    mount -t virtiofs home /host-home 2>/dev/null || true
fi

# Ensure DNS resolvers are configured.
# VZ framework NAT may not provide DNS via DHCP; ensure fallback resolvers
# so dockerd can resolve registry hostnames for image pulls.
if ! grep -q '^nameserver' /etc/resolv.conf 2>/dev/null; then
    printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > /etc/resolv.conf
fi
LOCALSTART_EOF

docker build --load -t "$DOCKER_IMAGE_TAG" "$WORK_DIR"

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

# If modloop is provided, copy it into the work directory for the privileged container.
MODLOOP_MOUNT_ARG=""
if [[ -n "$MODLOOP" ]]; then
  if [[ ! -f "$MODLOOP" ]]; then
    echo "modloop file not found: $MODLOOP" >&2
    exit 1
  fi
  cp "$MODLOOP" "$WORK_DIR/modloop.sqfs"
  echo "  modloop: $(du -h "$WORK_DIR/modloop.sqfs" | awk '{print $1}')"
fi

docker run --rm --privileged \
  -v "$WORK_DIR:/work" \
  "alpine:${ALPINE_VERSION}" \
  sh -c "
    set -e
    apk add --no-cache e2fsprogs tar squashfs-tools >/dev/null 2>&1

    # Create sparse ext4 image.
    dd if=/dev/zero of=/work/rootfs.ext4 bs=1M count=0 seek=${SIZE_MB} 2>/dev/null
    mkfs.ext4 -F -L arcbox-rootfs /work/rootfs.ext4 >/dev/null

    # Mount and extract rootfs.
    mkdir -p /mnt/rootfs
    mount -o loop /work/rootfs.ext4 /mnt/rootfs
    tar -xf /work/rootfs.tar -C /mnt/rootfs

    # Remove Docker build artifacts that confuse OpenRC.
    # /.dockerenv causes OpenRC to skip services with 'keyword -docker'
    # (e.g. networking), which prevents eth0 from being configured.
    rm -f /mnt/rootfs/.dockerenv

    # Extract kernel modules from modloop if provided.
    if [ -f /work/modloop.sqfs ]; then
      echo 'Injecting kernel modules from modloop...'
      mkdir -p /mnt/modloop
      unsquashfs -f -d /mnt/modloop /work/modloop.sqfs >/dev/null 2>&1
      KVER=\$(ls /mnt/modloop/modules/ 2>/dev/null | head -1)
      if [ -n \"\$KVER\" ]; then
        mkdir -p /mnt/rootfs/lib/modules/\$KVER
        cp -a /mnt/modloop/modules/\$KVER/* /mnt/rootfs/lib/modules/\$KVER/
        echo \"  kernel modules installed: \$KVER\"
      else
        echo '  warning: no kernel version found in modloop'
      fi
    fi

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
