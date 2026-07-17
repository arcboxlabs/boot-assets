set -e

apk add --no-cache \
  build-base git autoconf automake libtool pkgconf \
  linux-headers \
  util-linux-dev util-linux-static \
  zlib-dev zlib-static \
  lzo-dev \
  zstd-dev zstd-static \
  lz4-dev lz4-static \
  busybox-static ca-certificates \
  {{ utility_packages }} \
  {{ nfs_packages }}

# 1. busybox (pre-built static from Alpine)
cp /bin/busybox.static /out/busybox
echo "[1/{{ total }}] busybox (static) OK"

# 2. mkfs.btrfs (static build from source)
cd /tmp
git clone --depth 1 --branch v6.12 https://github.com/kdave/btrfs-progs.git
cd btrfs-progs
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
wget -q https://www.netfilter.org/projects/iptables/files/iptables-1.8.11.tar.xz
tar -xf iptables-1.8.11.tar.xz
cd iptables-1.8.11
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
git clone --depth 1 --branch v1.9.2 https://github.com/erofs/erofs-utils.git
cd erofs-utils
./autogen.sh
./configure --disable-fuse
# libtool swallows a plain -static; -all-static is its fully-static flag.
make LDFLAGS="-all-static" -j$(nproc)
strip mkfs/mkfs.erofs
cp mkfs/mkfs.erofs /out/
echo "[4/{{ total }}] mkfs.erofs (static) OK"

{{ utility_stage_script }}

{{ nfs_stage_script }}

# Shared libraries needed by packaged utilities.
mkdir -p /out/lib
cp -L /lib/ld-musl-*.so.1 /out/lib/
for bin in {{ utility_out_paths }} {{ nfs_out_paths }}; do
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
for bin in busybox mkfs.btrfs iptables mkfs.erofs; do
  printf "  %-16s " "$bin"
  if ldd "/out/$bin" >/dev/null 2>&1; then
    echo "DYNAMIC (WARNING)"
  else
    echo "static OK"
  fi
done
for bin in {{ utility_packages }} {{ nfs_binaries_list }}; do
  printf "  %-16s " "$bin"
  if ldd "/out/$bin" >/dev/null 2>&1; then
    echo "dynamic OK"
  else
    echo "static OK"
  fi
done
ls -lh /out/busybox /out/mkfs.btrfs /out/iptables /out/mkfs.erofs {{ utility_out_paths }} {{ nfs_out_paths }}
