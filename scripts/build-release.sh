#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
export LANG=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

VERSION=""
ARCH="arm64"
export ALPINE_VERSION="3.21"
ALPINE_FLAVOR="lts"
ARCBOX_DIR=""
ARCBOX_REPO="unknown"
ARCBOX_REF="unknown"
OUTPUT_DIR="$ROOT_DIR/dist"
DOCKER_VERSION=""
DOCKER_SHA256=""
CONTAINERD_VERSION=""
CONTAINERD_SHA256=""
YOUKI_VERSION=""
YOUKI_SHA256=""
# Pre-built artifacts: when provided, the corresponding build steps are skipped.
PREBUILT_AGENT_BIN=""  # --agent-bin: skip cargo build arcbox-agent
PREBUILT_ROOTFS=""     # --rootfs:    skip build-alpine-rootfs.sh
PREBUILT_KERNEL_DIR="" # --kernel-dir: skip build-kernel.sh
ARCBOX_SHA_OVERRIDE="" # --arcbox-sha: override git rev-parse HEAD

usage() {
  cat <<'EOF'
Usage: build-release.sh [options]

Required options:
  --version <version>      Asset version (for example: 0.0.1-alpha.3)
  --arcbox-dir <path>      Path to arcbox source tree

Optional:
  --arch <arch>            Target architecture (default: arm64)
  --alpine-version <ver>   Alpine release version (default: 3.21)
  --alpine-flavor <name>   Alpine netboot flavor (default: lts)
  --docker-version <ver>   Docker static bundle version
  --docker-sha256 <sha>    Docker static bundle sha256
  --containerd-version <v> containerd static bundle version
  --containerd-sha256 <s>  containerd static bundle sha256
  --youki-version <ver>    youki version
  --youki-sha256 <sha>     youki tarball sha256
  --arcbox-repo <repo>     ArcBox source repository (for manifest)
  --arcbox-ref <ref>       ArcBox source ref (for manifest)
  --output-dir <dir>       Output directory (default: dist/)
  --agent-bin <path>       Use pre-built arcbox-agent binary (skip cargo build)
  --rootfs <path>          Use pre-built rootfs.ext4 (skip build-alpine-rootfs.sh)
  --kernel-dir <path>      Use pre-built kernel dir (contains kernel + kernel-build.env)
  --arcbox-sha <sha>       Override arcbox git SHA recorded in manifest
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="$2"
      shift 2
      ;;
    --arcbox-dir)
      ARCBOX_DIR="$2"
      shift 2
      ;;
    --arch)
      ARCH="$2"
      shift 2
      ;;
    --alpine-version)
      ALPINE_VERSION="$2"
      shift 2
      ;;
    --alpine-flavor)
      ALPINE_FLAVOR="$2"
      shift 2
      ;;
    --docker-version)
      DOCKER_VERSION="$2"
      shift 2
      ;;
    --docker-sha256)
      DOCKER_SHA256="$2"
      shift 2
      ;;
    --containerd-version)
      CONTAINERD_VERSION="$2"
      shift 2
      ;;
    --containerd-sha256)
      CONTAINERD_SHA256="$2"
      shift 2
      ;;
    --youki-version)
      YOUKI_VERSION="$2"
      shift 2
      ;;
    --youki-sha256)
      YOUKI_SHA256="$2"
      shift 2
      ;;
    --arcbox-repo)
      ARCBOX_REPO="$2"
      shift 2
      ;;
    --arcbox-ref)
      ARCBOX_REF="$2"
      shift 2
      ;;
    --output-dir)
      OUTPUT_DIR="$2"
      shift 2
      ;;
    --agent-bin)
      PREBUILT_AGENT_BIN="$2"
      shift 2
      ;;
    --rootfs)
      PREBUILT_ROOTFS="$2"
      shift 2
      ;;
    --kernel-dir)
      PREBUILT_KERNEL_DIR="$2"
      shift 2
      ;;
    --arcbox-sha)
      ARCBOX_SHA_OVERRIDE="$2"
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

if [[ -z "$VERSION" || -z "$ARCBOX_DIR" ]]; then
  usage >&2
  exit 1
fi

if [[ "$ARCH" != "arm64" && "$ARCH" != "amd64" ]]; then
  echo "unsupported arch: $ARCH (expected: arm64 or amd64)" >&2
  exit 1
fi

if [[ ! -f "$ARCBOX_DIR/Cargo.toml" ]]; then
  echo "invalid arcbox directory: $ARCBOX_DIR" >&2
  exit 1
fi

case "$ARCH" in
  arm64)
    TARGET_TRIPLE="aarch64-unknown-linux-musl"
    ALPINE_ARCH="aarch64"
    CARGO_LINKER_ENV="CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER"
    ;;
  amd64)
    TARGET_TRIPLE="x86_64-unknown-linux-musl"
    ALPINE_ARCH="x86_64"
    CARGO_LINKER_ENV="CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER"
    ;;
esac

RELEASE_BASE_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/${ALPINE_ARCH}"
NETBOOT_BASE_URL="${RELEASE_BASE_URL}/netboot"
INITRAMFS_URL="${NETBOOT_BASE_URL}/initramfs-${ALPINE_FLAVOR}"
NETBOOT_RELEASE_VERSION="unknown"
NETBOOT_FILE="unknown"
NETBOOT_URL="unknown"
NETBOOT_SHA256="unknown"

BUILD_ROOT="$ROOT_DIR/build/$ARCH"
BASE_DIR="$BUILD_ROOT/base"
WORK_DIR="$BUILD_ROOT/work"
mkdir -p "$BASE_DIR" "$WORK_DIR" "$OUTPUT_DIR"

echo "==> download base Alpine initramfs"
_kernel_dl_flags=(
  --arch "$ARCH"
  --alpine-version "$ALPINE_VERSION"
  --flavor "$ALPINE_FLAVOR"
  --out-dir "$BASE_DIR"
)
"$SCRIPT_DIR/download-kernel.sh" "${_kernel_dl_flags[@]}"
unset _kernel_dl_flags

echo "==> download runtime artifacts"
runtime_args=(
  --arch "$ARCH"
  --out-dir "$BASE_DIR/runtime"
)
if [[ -n "$DOCKER_VERSION" ]]; then
  runtime_args+=(--docker-version "$DOCKER_VERSION")
fi
if [[ -n "$DOCKER_SHA256" ]]; then
  runtime_args+=(--docker-sha256 "$DOCKER_SHA256")
fi
if [[ -n "$CONTAINERD_VERSION" ]]; then
  runtime_args+=(--containerd-version "$CONTAINERD_VERSION")
fi
if [[ -n "$CONTAINERD_SHA256" ]]; then
  runtime_args+=(--containerd-sha256 "$CONTAINERD_SHA256")
fi
if [[ -n "$YOUKI_VERSION" ]]; then
  runtime_args+=(--youki-version "$YOUKI_VERSION")
fi
if [[ -n "$YOUKI_SHA256" ]]; then
  runtime_args+=(--youki-sha256 "$YOUKI_SHA256")
fi
"$SCRIPT_DIR/download-runtime.sh" "${runtime_args[@]}"

if [[ -f "$BASE_DIR/netboot-metadata.env" ]]; then
  # shellcheck disable=SC1090
  source "$BASE_DIR/netboot-metadata.env"
fi
if [[ -f "$BASE_DIR/runtime/runtime-metadata.env" ]]; then
  # shellcheck disable=SC1090
  source "$BASE_DIR/runtime/runtime-metadata.env"
fi

if [[ -n "$PREBUILT_AGENT_BIN" ]]; then
  echo "==> using pre-built arcbox-agent: $PREBUILT_AGENT_BIN"
  if [[ ! -f "$PREBUILT_AGENT_BIN" ]]; then
    echo "pre-built agent binary not found: $PREBUILT_AGENT_BIN" >&2
    exit 1
  fi
  AGENT_BIN="$PREBUILT_AGENT_BIN"
else
  echo "==> build arcbox-agent"
  LINKER_VALUE="${!CARGO_LINKER_ENV:-rust-lld}"
  env "${CARGO_LINKER_ENV}=${LINKER_VALUE}" cargo build \
    --manifest-path "$ARCBOX_DIR/Cargo.toml" \
    -p arcbox-agent \
    --target "$TARGET_TRIPLE" \
    --release

  AGENT_BIN="$ARCBOX_DIR/target/$TARGET_TRIPLE/release/arcbox-agent"
  if [[ ! -f "$AGENT_BIN" ]]; then
    echo "agent binary not found: $AGENT_BIN" >&2
    exit 1
  fi
fi

if [[ -n "$PREBUILT_KERNEL_DIR" ]]; then
  echo "==> using pre-built kernel dir: $PREBUILT_KERNEL_DIR"
  KERNEL_DIR="$PREBUILT_KERNEL_DIR"
else
  echo "==> build kernel"
  KERNEL_DIR="$BUILD_ROOT/kernel"
  "$SCRIPT_DIR/build-kernel.sh" \
    --arch "$ARCH" \
    --out-dir "$KERNEL_DIR"
fi
if [[ ! -f "$KERNEL_DIR/kernel" ]]; then
  echo "kernel not found: $KERNEL_DIR/kernel" >&2
  exit 1
fi
if [[ ! -f "$KERNEL_DIR/kernel-build.env" ]]; then
  echo "kernel metadata not found: $KERNEL_DIR/kernel-build.env" >&2
  exit 1
fi
# shellcheck disable=SC1090
source "$KERNEL_DIR/kernel-build.env"

echo "==> build initramfs"
"$SCRIPT_DIR/build-alpine-initramfs.sh" \
  --base-initramfs "$BASE_DIR/initramfs-${ARCH}" \
  --output "$WORK_DIR/initramfs.cpio.gz"

if [[ -n "$PREBUILT_ROOTFS" ]]; then
  echo "==> using pre-built rootfs.ext4: $PREBUILT_ROOTFS"
  if [[ ! -f "$PREBUILT_ROOTFS" ]]; then
    echo "pre-built rootfs not found: $PREBUILT_ROOTFS" >&2
    exit 1
  fi
  cp "$PREBUILT_ROOTFS" "$WORK_DIR/rootfs.ext4"
else
  echo "==> build rootfs.ext4"
  "$SCRIPT_DIR/build-alpine-rootfs.sh" \
    --output "$WORK_DIR/rootfs.ext4"
fi

cp "$KERNEL_DIR/kernel" "$WORK_DIR/kernel"
rm -rf "$WORK_DIR/runtime"
cp -R "$BASE_DIR/runtime" "$WORK_DIR/runtime"
# Include arcbox-agent binary in the bundle so the host can place it on VirtioFS
# at /arcbox/bin/arcbox-agent for the OpenRC service inside the guest.
mkdir -p "$WORK_DIR/bin"
cp "$AGENT_BIN" "$WORK_DIR/bin/arcbox-agent"

KERNEL_SHA256="$(shasum -a 256 "$WORK_DIR/kernel" | awk '{print $1}')"
INITRAMFS_SHA256="$(shasum -a 256 "$WORK_DIR/initramfs.cpio.gz" | awk '{print $1}')"
ROOTFS_EXT4_SHA256="$(shasum -a 256 "$WORK_DIR/rootfs.ext4" | awk '{print $1}')"
if [[ -n "$ARCBOX_SHA_OVERRIDE" ]]; then
  ARCBOX_SHA="$ARCBOX_SHA_OVERRIDE"
else
  ARCBOX_SHA="$(git -C "$ARCBOX_DIR" rev-parse HEAD)"
fi
BUILT_AT="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
RUNTIME_DOCKER_VERSION="${RUNTIME_DOCKER_VERSION:-unknown}"
RUNTIME_CONTAINERD_VERSION="${RUNTIME_CONTAINERD_VERSION:-unknown}"
RUNTIME_YOUKI_VERSION="${RUNTIME_YOUKI_VERSION:-unknown}"
RUNTIME_DOCKERD_SHA256="${RUNTIME_DOCKERD_SHA256:-$(shasum -a 256 "$WORK_DIR/runtime/bin/dockerd" | awk '{print $1}')}"
RUNTIME_CONTAINERD_SHA256="${RUNTIME_CONTAINERD_SHA256:-$(shasum -a 256 "$WORK_DIR/runtime/bin/containerd" | awk '{print $1}')}"
RUNTIME_YOUKI_SHA256="${RUNTIME_YOUKI_SHA256:-$(shasum -a 256 "$WORK_DIR/runtime/bin/youki" | awk '{print $1}')}"
KERNEL_SOURCE_URL="${KERNEL_SOURCE_URL:-unknown}"

# schema_version 4: Alpine rootfs + OpenRC architecture.
# Replaces squashfs+overlay with ext4 block device rootfs.
# The VMM attaches rootfs.ext4 as a VirtIO block device (/dev/vda);
# initramfs mounts it and switch_roots to /sbin/init (Alpine OpenRC).
# Agent binary is served via VirtioFS at /arcbox/bin/arcbox-agent.
cat > "$WORK_DIR/manifest.json" <<EOF
{
  "schema_version": 4,
  "asset_version": "${VERSION}",
  "arch": "${ARCH}",
  "alpine_branch_version": "${ALPINE_VERSION}",
  "alpine_netboot_version": null,
  "netboot_bundle_file": null,
  "netboot_bundle_url": null,
  "netboot_bundle_sha256": null,
  "kernel_sha256": "${KERNEL_SHA256}",
  "initramfs_sha256": "${INITRAMFS_SHA256}",
  "rootfs_ext4_sha256": "${ROOTFS_EXT4_SHA256}",
  "modloop_sha256": null,
  "kernel_source_url": "${KERNEL_SOURCE_URL}",
  "initramfs_source_url": "${INITRAMFS_URL}",
  "modloop_source_url": null,
  "kernel_commit": null,
  "agent_commit": "${ARCBOX_SHA}",
  "built_at": "${BUILT_AT}",
  "kernel_cmdline": "console=hvc0 rdinit=/init quiet",
  "runtime_assets": [
    {
      "name": "dockerd",
      "path": "runtime/bin/dockerd",
      "version": "${RUNTIME_DOCKER_VERSION}",
      "sha256": "${RUNTIME_DOCKERD_SHA256}"
    },
    {
      "name": "containerd",
      "path": "runtime/bin/containerd",
      "version": "${RUNTIME_CONTAINERD_VERSION}",
      "sha256": "${RUNTIME_CONTAINERD_SHA256}"
    },
    {
      "name": "youki",
      "path": "runtime/bin/youki",
      "version": "${RUNTIME_YOUKI_VERSION}",
      "sha256": "${RUNTIME_YOUKI_SHA256}"
    }
  ],
  "source_repo": "${ARCBOX_REPO}",
  "source_ref": "${ARCBOX_REF}",
  "source_sha": "${ARCBOX_SHA}"
}
EOF

TARBALL="boot-assets-${ARCH}-v${VERSION}.tar.gz"

echo "==> package tarball"
tar -czf "$OUTPUT_DIR/$TARBALL" -C "$WORK_DIR" \
  kernel initramfs.cpio.gz rootfs.ext4 manifest.json runtime bin
shasum -a 256 "$OUTPUT_DIR/$TARBALL" > "$OUTPUT_DIR/$TARBALL.sha256"
cp "$WORK_DIR/manifest.json" "$OUTPUT_DIR/manifest.json"

echo "build complete"
echo "tarball:   $OUTPUT_DIR/$TARBALL"
echo "checksum:  $OUTPUT_DIR/$TARBALL.sha256"
echo "manifest:  $OUTPUT_DIR/manifest.json"
