use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const BUSYBOX_SYMLINKS: &[&str] = &[
    "sh", "mount", "umount", "mkdir", "cat", "echo", "sleep", "ln", "chmod", "chown", "cp", "mv",
    "rm", "ls", "ip", "hostname", "sysctl",
];

const IPTABLES_SYMLINKS: &[&str] = &[
    "iptables-save",
    "iptables-restore",
    "ip6tables",
    "ip6tables-save",
    "ip6tables-restore",
];

const K3S_HOST_UTILITIES: &[&str] = &["ebtables", "ethtool", "socat"];
const EROFS_BLOCK_SIZE: &str = "4096";

const MOUNT_DIRS: &[&str] = &[
    "tmp", "run", "proc", "sys", "dev", "mnt", "arcbox", "Users", "etc", "var",
];

const INIT_SCRIPT: &str = r#"#!/bin/busybox sh
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox mkdir -p /arcbox
/bin/busybox mount -t virtiofs arcbox /arcbox
exec /arcbox/bin/arcbox-agent
"#;

#[derive(Debug, Clone)]
pub struct BuildRootfsOpts {
    pub output: PathBuf,
    pub arch: String,
    pub compression: String,
}

pub fn build_rootfs(opts: &BuildRootfsOpts) -> Result<()> {
    let (docker_platform, _alpine_arch) = match opts.arch.as_str() {
        "arm64" => ("linux/arm64", "aarch64"),
        "x86_64" => ("linux/amd64", "x86_64"),
        other => bail!("unsupported arch: {other}"),
    };

    let staging = tempfile::tempdir().context("failed to create temp dir")?;
    let staging_path = staging.path();

    // Step 1: Build core static binaries and stage packaged k3s host utilities.
    println!("==> Building rootfs binaries via Docker ({docker_platform})");
    let docker_script = r#"
set -e

apk add --no-cache \
  build-base git autoconf automake libtool pkgconf \
  linux-headers \
  util-linux-dev util-linux-static \
  zlib-dev zlib-static \
  lzo-dev \
  zstd-dev zstd-static \
  busybox-static ca-certificates \
  ebtables ethtool socat

# 1. busybox (pre-built static from Alpine)
cp /bin/busybox.static /out/busybox
echo "[1/6] busybox (static) OK"

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
echo "[2/6] mkfs.btrfs (static) OK"

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
echo "[3/6] iptables-legacy (static) OK"

# 4-6. k3s host utilities from Alpine packages.
for bin in ebtables ethtool socat; do
  src="$(command -v "$bin")"
  cp "$src" "/out/$bin"
  case "$bin" in
    ebtables) idx=4 ;;
    ethtool) idx=5 ;;
    socat) idx=6 ;;
  esac
  echo "[$idx/6] $bin OK"
done

# Shared libraries needed by packaged utilities.
mkdir -p /out/lib
cp -L /lib/ld-musl-*.so.1 /out/lib/
for bin in /out/ebtables /out/ethtool /out/socat; do
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
for bin in busybox mkfs.btrfs iptables; do
  printf "  %-16s " "$bin"
  if ldd "/out/$bin" >/dev/null 2>&1; then
    echo "DYNAMIC (WARNING)"
  else
    echo "static OK"
  fi
done
for bin in ebtables ethtool socat; do
  printf "  %-16s " "$bin"
  if ldd "/out/$bin" >/dev/null 2>&1; then
    echo "dynamic OK"
  else
    echo "static OK"
  fi
done
ls -lh /out/busybox /out/mkfs.btrfs /out/iptables /out/ebtables /out/ethtool /out/socat
"#;

    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            docker_platform,
            "-v",
            &format!("{}:/out", staging_path.display()),
            "alpine:3.19",
            "sh",
            "-c",
            docker_script,
        ])
        .status()
        .context("failed to run docker")?;
    if !status.success() {
        bail!("docker static build failed");
    }

    // Step 2: Build rootfs staging directory.
    println!("==> Building EROFS rootfs staging directory");
    let rootfs = staging_path.join("rootfs");
    build_rootfs_tree(&rootfs, staging_path)?;

    // Step 3: Create EROFS image.
    println!("==> Creating EROFS image");
    check_mkfs_erofs()?;

    if let Some(parent) = opts.output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let status = Command::new("mkfs.erofs")
        .arg(format!("-b{EROFS_BLOCK_SIZE}"))
        .arg(format!("-z{}", opts.compression))
        .arg(&opts.output)
        .arg(&rootfs)
        .status()
        .context("failed to run mkfs.erofs")?;
    if !status.success() {
        bail!("mkfs.erofs failed");
    }

    let size = humanize_size(std::fs::metadata(&opts.output)?.len());
    println!();
    println!("==> EROFS rootfs built: {} ({size})", opts.output.display());
    println!("    Compression: {}", opts.compression);
    println!("    Block size: {} bytes", EROFS_BLOCK_SIZE);
    println!(
        "    Contents: busybox + mkfs.btrfs + iptables-legacy + ebtables + ethtool + socat + CA certs + trampoline"
    );
    println!("    Core boot tools are static; k3s host utilities include required shared libs");

    Ok(())
}

fn build_rootfs_tree(rootfs: &Path, staging: &Path) -> Result<()> {
    // /bin — busybox + symlinks
    let bin_dir = rootfs.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    copy_executable(&staging.join("busybox"), &bin_dir.join("busybox"))?;
    for cmd in BUSYBOX_SYMLINKS {
        std::os::unix::fs::symlink("busybox", bin_dir.join(cmd))?;
    }

    // /sbin — system binaries
    let sbin_dir = rootfs.join("sbin");
    std::fs::create_dir_all(&sbin_dir)?;
    copy_executable(&staging.join("mkfs.btrfs"), &sbin_dir.join("mkfs.btrfs"))?;
    copy_executable(&staging.join("iptables"), &sbin_dir.join("iptables"))?;
    for binary in K3S_HOST_UTILITIES {
        copy_executable(&staging.join(binary), &sbin_dir.join(binary))?;
    }
    for link in IPTABLES_SYMLINKS {
        std::os::unix::fs::symlink("iptables", sbin_dir.join(link))?;
    }

    // /sbin/init — trampoline
    std::fs::write(sbin_dir.join("init"), INIT_SCRIPT)?;
    set_executable(&sbin_dir.join("init"))?;

    // /lib — dynamic loader and shared libs for packaged k3s host utilities.
    let lib_dir = rootfs.join("lib");
    std::fs::create_dir_all(&lib_dir)?;
    let staged_lib_dir = staging.join("lib");
    if staged_lib_dir.is_dir() {
        for entry in std::fs::read_dir(staged_lib_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                std::fs::copy(&path, lib_dir.join(entry.file_name()))?;
            }
        }
    }

    // /cacerts
    let cacerts_dir = rootfs.join("cacerts");
    std::fs::create_dir_all(&cacerts_dir)?;
    std::fs::copy(
        staging.join("ca-certificates.crt"),
        cacerts_dir.join("ca-certificates.crt"),
    )?;

    // Mount point directories
    for dir in MOUNT_DIRS {
        std::fs::create_dir_all(rootfs.join(dir))?;
    }

    Ok(())
}

fn copy_executable(src: &Path, dst: &Path) -> Result<()> {
    std::fs::copy(src, dst)?;
    set_executable(dst)?;
    Ok(())
}

fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

fn check_mkfs_erofs() -> Result<()> {
    match Command::new("mkfs.erofs").arg("-V").output() {
        Ok(_) => Ok(()),
        _ => bail!(
            "mkfs.erofs not found. Install erofs-utils:\n  \
             macOS:  brew install erofs-utils\n  \
             Ubuntu: apt install erofs-utils"
        ),
    }
}

fn humanize_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1}M", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}K", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}
