#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
export LANG=C

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

ARCH=""
MANIFEST="$ROOT_DIR/kernels/manifest.json"
OUT_DIR=""
JOBS=""

usage() {
  cat <<'USAGE_EOF'
Usage: build-kernel.sh [options]
  --arch <arm64|amd64>   Target architecture (required)
  --manifest <path>      Path to kernels/manifest.json (default: kernels/manifest.json)
  --out-dir <dir>        Output directory (default: build/<arch>/kernel)
  --jobs <N>             Parallel make jobs (default: nproc)
USAGE_EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch)
      ARCH="$2"
      shift 2
      ;;
    --manifest)
      MANIFEST="$2"
      shift 2
      ;;
    --out-dir)
      OUT_DIR="$2"
      shift 2
      ;;
    --jobs)
      JOBS="$2"
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

if [[ -z "$ARCH" ]]; then
  usage >&2
  exit 1
fi

if [[ "$ARCH" != "arm64" && "$ARCH" != "amd64" ]]; then
  echo "unsupported arch: $ARCH (expected: arm64 or amd64)" >&2
  exit 1
fi

if [[ ! -f "$MANIFEST" ]]; then
  echo "manifest not found: $MANIFEST" >&2
  exit 1
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required but not found in PATH" >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required but not found in PATH" >&2
  exit 1
fi

default_jobs() {
  if command -v nproc >/dev/null 2>&1; then
    nproc
  elif command -v sysctl >/dev/null 2>&1; then
    sysctl -n hw.ncpu
  else
    echo 4
  fi
}

if [[ -z "$JOBS" ]]; then
  JOBS="$(default_jobs)"
fi

if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="$ROOT_DIR/build/$ARCH/kernel"
fi

read_manifest() {
  python3 - "$MANIFEST" "$ARCH" <<'PY'
import json
import pathlib
import sys

manifest_path = pathlib.Path(sys.argv[1])
arch = sys.argv[2]
manifest = json.loads(manifest_path.read_text())

version = manifest.get("version")
source_url = manifest.get("source_url")
source_sha256 = manifest.get("source_sha256")
archs = manifest.get("archs", {})
config_rel = archs.get(arch, {}).get("config")
if not (version and source_url and source_sha256 and config_rel):
    raise SystemExit(f"manifest missing required fields for arch={arch}")

print(version)
print(source_url)
print(source_sha256)
print(str((manifest_path.parent / config_rel).resolve()))
PY
}

mapfile -t manifest_values < <(read_manifest)
KERNEL_VERSION="${manifest_values[0]}"
SOURCE_URL="${manifest_values[1]}"
SOURCE_SHA256="${manifest_values[2]}"
CONFIG_PATH="${manifest_values[3]}"

if [[ ! -f "$CONFIG_PATH" ]]; then
  echo "kernel config not found: $CONFIG_PATH" >&2
  exit 1
fi

case "$ARCH" in
  arm64)
    KARCH="arm64"
    CROSS_COMPILE="aarch64-linux-gnu-"
    IMAGE_PATH="arch/arm64/boot/Image"
    ;;
  amd64)
    KARCH="x86"
    CROSS_COMPILE="x86_64-linux-gnu-"
    IMAGE_PATH="arch/x86/boot/bzImage"
    ;;
esac

mkdir -p "$OUT_DIR"
TMP_DIR="$(mktemp -d /tmp/arcbox-kernel-build.XXXXXX)"
BUILD_IMAGE="arcbox-kernel-builder:${KERNEL_VERSION}"
SOURCE_TARBALL="$TMP_DIR/linux-${KERNEL_VERSION}.tar.xz"

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

download_file() {
  local url="$1"
  local output="$2"
  echo "download: $url"
  curl -fL --retry 3 --retry-delay 2 --retry-connrefused -o "$output" "$url"
}

sha256_file() {
  local file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    sha256sum "$file" | awk '{print $1}'
  fi
}

echo "==> download kernel source"
download_file "$SOURCE_URL" "$SOURCE_TARBALL"

CURRENT_SHA256="$(sha256_file "$SOURCE_TARBALL")"
if [[ "$CURRENT_SHA256" != "$SOURCE_SHA256" ]]; then
  echo "kernel source checksum mismatch: expected $SOURCE_SHA256, got $CURRENT_SHA256" >&2
  exit 1
fi
echo "verified source sha256: $CURRENT_SHA256"

echo "==> build kernel toolchain image"
docker build -f "$ROOT_DIR/kernel/Dockerfile" -t "$BUILD_IMAGE" "$ROOT_DIR"

echo "==> build kernel (${ARCH})"
docker run --rm \
  -e KERNEL_VERSION="$KERNEL_VERSION" \
  -e SOURCE_URL="$SOURCE_URL" \
  -e SOURCE_SHA256="$SOURCE_SHA256" \
  -e KARCH="$KARCH" \
  -e CROSS_COMPILE="$CROSS_COMPILE" \
  -e IMAGE_PATH="$IMAGE_PATH" \
  -e JOBS="$JOBS" \
  -e KBUILD_BUILD_TIMESTAMP="1970-01-01" \
  -e KBUILD_BUILD_USER="arcbox" \
  -e KBUILD_BUILD_HOST="arcbox" \
  -e LOCALVERSION="-arcbox" \
  -v "$SOURCE_TARBALL:/input/linux.tar.xz:ro" \
  -v "$CONFIG_PATH:/input/kernel.config:ro" \
  -v "$OUT_DIR:/out" \
  "$BUILD_IMAGE" \
  bash -lc '
    set -euo pipefail

    mkdir -p /work/src
    tar -xf /input/linux.tar.xz -C /work/src
    cd "/work/src/linux-${KERNEL_VERSION}"

    cp /input/kernel.config .config
    make ARCH="${KARCH}" CROSS_COMPILE="${CROSS_COMPILE}" olddefconfig
    make ARCH="${KARCH}" CROSS_COMPILE="${CROSS_COMPILE}" -j"${JOBS}"

    cp "${IMAGE_PATH}" /out/kernel

    KERNEL_RELEASE="$(make -s ARCH="${KARCH}" CROSS_COMPILE="${CROSS_COMPILE}" kernelrelease)"
    KERNEL_IMAGE_SHA256=$(sha256sum /out/kernel | awk "{print \$1}")

    cat > /out/kernel-build.env <<ENV_EOF
KERNEL_VERSION=${KERNEL_VERSION}
KERNEL_RELEASE=${KERNEL_RELEASE}
KERNEL_SOURCE_URL=${SOURCE_URL}
KERNEL_SOURCE_SHA256=${SOURCE_SHA256}
KERNEL_IMAGE_SHA256=${KERNEL_IMAGE_SHA256}
ENV_EOF
  '

echo "kernel ready: $OUT_DIR/kernel"
echo "metadata:    $OUT_DIR/kernel-build.env"
