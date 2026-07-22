set -e

apk add --no-cache \
  build-base curl autoconf automake libtool pkgconf \
  linux-headers \
  util-linux-dev util-linux-static \
  zlib-dev zlib-static \
  lzo-dev \
  zstd-dev zstd-static \
  lz4-dev lz4-static \
  busybox-static ca-certificates \
  {{ nfs_packages }}

# 1. busybox (pre-built static from Alpine)
cp /bin/busybox.static /out/busybox
echo "[1/{{ total }}] busybox (static) OK"

# 2. mkfs.btrfs (static build from source; tag tarball + retries — a git
# clone cannot resume or retry after a mid-transfer reset)
cd /tmp
curl -sfL --retry 8 --retry-all-errors -o btrfs-progs.tar.gz \
  https://github.com/kdave/btrfs-progs/archive/refs/tags/v7.1.tar.gz
tar -xzf btrfs-progs.tar.gz
cd btrfs-progs-7.1
./autogen.sh
LDFLAGS="-static" ./configure \
  --disable-documentation --disable-python \
  --disable-zoned --disable-libudev \
  --disable-convert --disable-backtrace
make -j$(nproc) mkfs.btrfs
strip mkfs.btrfs
cp mkfs.btrfs /out/
echo "[2/{{ total }}] mkfs.btrfs (static) OK"

# 3. iptables-legacy (static build from source)
cd /tmp
curl -sfL --retry 8 --retry-all-errors -O https://www.netfilter.org/projects/iptables/files/iptables-1.8.13.tar.xz
tar -xf iptables-1.8.13.tar.xz
cd iptables-1.8.13
# Fix musl header conflict: linux/if_ether.h and netinet/if_ether.h
# both define struct ethhdr without mutual guards. Disable the kernel
# UAPI definition and force-include the userspace header so ethhdr is
# always available regardless of source include order.
CPPFLAGS="-D__UAPI_DEF_ETHHDR=0 -include netinet/if_ether.h" \
./configure \
  --enable-static --disable-shared \
  --disable-nftables --disable-connlabel
make LDFLAGS="-all-static" -j$(nproc)
strip iptables/xtables-legacy-multi
cp iptables/xtables-legacy-multi /out/iptables
echo "[3/{{ total }}] iptables-legacy (static) OK"

# 4. mkfs.erofs (static build from source; containerd's erofs snapshotter
#    differ prefers erofs-utils >= 1.8.2, newer than the Alpine package)
cd /tmp
curl -sfL --retry 8 --retry-all-errors -o erofs-utils.tar.gz \
  https://github.com/erofs/erofs-utils/archive/refs/tags/v1.9.2.tar.gz
tar -xzf erofs-utils.tar.gz
cd erofs-utils-1.9.2
./autogen.sh
./configure --disable-fuse
# libtool swallows a plain -static; -all-static is its fully-static flag.
make LDFLAGS="-all-static" -j$(nproc)
strip mkfs/mkfs.erofs
cp mkfs/mkfs.erofs /out/
echo "[4/{{ total }}] mkfs.erofs (static) OK"

# 5-6. mkfs.ext4 + e2fsck (static e2fsprogs; the arcbox ext4 metadata
# volume is formatted/repaired guest-side). e2fsprogs ships a committed
# ./configure — no autogen — and vendors its own libuuid/libblkid, which
# --enable-libuuid/--enable-libblkid force so the static link never
# depends on the Alpine util-linux packages installed for btrfs-progs.
cd /tmp
curl -sfL --retry 8 --retry-all-errors -o e2fsprogs.tar.gz \
  https://github.com/tytso/e2fsprogs/archive/refs/tags/v1.47.3.tar.gz
tar -xzf e2fsprogs.tar.gz
cd e2fsprogs-1.47.3
LDFLAGS="-static" ./configure \
  --enable-libuuid --enable-libblkid \
  --disable-nls --disable-uuidd --disable-fsck \
  --disable-e2initrd-helper --disable-fuse2fs --disable-defrag \
  --disable-debugfs --disable-imager --disable-resizer
make -j$(nproc) libs
make -j$(nproc) -C misc mke2fs
make -j$(nproc) -C e2fsck e2fsck
strip misc/mke2fs e2fsck/e2fsck
# mke2fs switches to ext4 defaults when invoked as mkfs.ext4 (argv[0]).
cp misc/mke2fs /out/mkfs.ext4
cp e2fsck/e2fsck /out/e2fsck
echo "[5/{{ total }}] mkfs.ext4 (static) OK"
echo "[6/{{ total }}] e2fsck (static) OK"

{{ nfs_stage_script }}

# Shared libraries needed by packaged utilities.
mkdir -p /out/lib
cp -L /lib/ld-musl-*.so.1 /out/lib/
for bin in {{ nfs_out_paths }}; do
  ldd "$bin" | awk '/=>/ { print $3 } /^\// { print $1 }' | while read -r lib; do
    if [ -f "$lib" ]; then
      cp -L "$lib" "/out/lib/$(basename "$lib")"
    fi
  done
done

# CA certificates
cp /etc/ssl/certs/ca-certificates.crt /out/ca-certificates.crt

# Verify rootfs binaries and utility dependencies.
echo "=== Verification ==="
# Core binaries must be static — no shared libs are staged for them, so a
# dynamic one would pass the build and fail in the guest with exit 127.
for bin in busybox mkfs.btrfs iptables mkfs.erofs mkfs.ext4 e2fsck; do
  printf "  %-16s " "$bin"
  if ldd "/out/$bin" >/dev/null 2>&1; then
    echo "DYNAMIC (ERROR)"
    exit 1
  else
    echo "static OK"
  fi
done
for bin in {{ nfs_binaries_list }}; do
  printf "  %-16s " "$bin"
  if ldd "/out/$bin" >/dev/null 2>&1; then
    echo "dynamic OK"
  else
    echo "static OK"
  fi
done
ls -lh /out/busybox /out/mkfs.btrfs /out/iptables /out/mkfs.erofs /out/mkfs.ext4 /out/e2fsck {{ nfs_out_paths }}
