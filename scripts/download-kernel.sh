#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
export LANG=C

ARCH="arm64"
ALPINE_VERSION="3.21"
FLAVOR="lts"
OUT_DIR=""

usage() {
  cat <<'USAGE_EOF'
Usage: download-kernel.sh [options]

Options:
  --arch <arm64|amd64>      Target architecture (default: arm64)
  --alpine-version <ver>    Alpine release version (default: 3.21)
  --flavor <flavor>         Netboot flavor suffix (default: lts)
  --out-dir <dir>           Output directory
USAGE_EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch)
      ARCH="$2"
      shift 2
      ;;
    --alpine-version)
      ALPINE_VERSION="$2"
      shift 2
      ;;
    --flavor)
      FLAVOR="$2"
      shift 2
      ;;
    --out-dir)
      OUT_DIR="$2"
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

case "$ARCH" in
  arm64)
    ALPINE_ARCH="aarch64"
    ;;
  amd64)
    ALPINE_ARCH="x86_64"
    ;;
  *)
    echo "unsupported arch: $ARCH (expected: arm64 or amd64)" >&2
    exit 1
    ;;
esac

if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="$(pwd)/build/${ARCH}/base"
fi

mkdir -p "$OUT_DIR"

RELEASE_BASE_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/${ALPINE_ARCH}"
NETBOOT_BASE_URL="${RELEASE_BASE_URL}/netboot"
LATEST_RELEASES_URL="${RELEASE_BASE_URL}/latest-releases.yaml"
INITRAMFS_URL="${NETBOOT_BASE_URL}/initramfs-${FLAVOR}"
INITRAMFS_OUT="$OUT_DIR/initramfs-${ARCH}"

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

NETBOOT_METADATA_FILE="$(mktemp /tmp/boot-assets-netboot-metadata.XXXXXX)"
cleanup() {
  rm -f "$NETBOOT_METADATA_FILE"
}
trap cleanup EXIT

download_file "$LATEST_RELEASES_URL" "$NETBOOT_METADATA_FILE"

NETBOOT_VERSION=""
NETBOOT_FILE=""
NETBOOT_SHA256=""

while IFS='=' read -r key value; do
  case "$key" in
    version) NETBOOT_VERSION="$value" ;;
    file) NETBOOT_FILE="$value" ;;
    sha256) NETBOOT_SHA256="$value" ;;
  esac
done < <(
  awk '
function emit() {
  if (cur_flavor == "alpine-netboot") {
    print "version=" cur_version;
    print "file=" cur_file;
    print "sha256=" cur_sha256;
  }
}
/^-/ {
  emit();
  cur_flavor = ""; cur_version = ""; cur_file = ""; cur_sha256 = "";
  next;
}
/^[[:space:]]+flavor:/ { cur_flavor = $2; next }
/^[[:space:]]+version:/ { cur_version = $2; next }
/^[[:space:]]+file:/ { cur_file = $2; next }
/^[[:space:]]+sha256:/ { cur_sha256 = $2; next }
END { emit() }
' "$NETBOOT_METADATA_FILE"
)

download_file "$INITRAMFS_URL" "$INITRAMFS_OUT"
INITRAMFS_SHA256="$(sha256_file "$INITRAMFS_OUT")"

echo "verified initramfs sha256: $INITRAMFS_SHA256"

cat > "$OUT_DIR/netboot-metadata.env" <<ENV_EOF
NETBOOT_BRANCH_VERSION=${ALPINE_VERSION}
NETBOOT_RELEASE_VERSION=${NETBOOT_VERSION}
NETBOOT_FILE=${NETBOOT_FILE}
NETBOOT_URL=${RELEASE_BASE_URL}/${NETBOOT_FILE}
NETBOOT_SHA256=${NETBOOT_SHA256}
NETBOOT_FLAVOR=${FLAVOR}
INITRAMFS_URL=${INITRAMFS_URL}
INITRAMFS_SHA256=${INITRAMFS_SHA256}
ENV_EOF

echo "initramfs: $INITRAMFS_OUT"
