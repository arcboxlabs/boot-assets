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

/// Core statically-linked binaries built from source inside Docker.
const CORE_STATIC_BINARIES: &[&str] = &["busybox", "mkfs.btrfs", "iptables"];

const K3S_HOST_UTILITIES: &[&str] = &["ebtables", "ethtool", "socat"];

/// NFS client utilities: Alpine packages to install, and binaries to extract.
const NFS_PACKAGES: &[&str] = &["nfs-utils"];
const NFS_BINARIES: &[&str] = &["mount.nfs", "mount.nfs4"];
const EROFS_BLOCK_SIZE: &str = "4096";
const EROFS_XATTR_TOLERANCE: &str = "-1";

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

fn k3s_host_utilities_apk_packages() -> String {
    K3S_HOST_UTILITIES.join(" ")
}

/// Total number of binary build steps (core static + k3s host utilities).
fn total_build_steps() -> usize {
    CORE_STATIC_BINARIES.len() + K3S_HOST_UTILITIES.len() + NFS_BINARIES.len()
}

fn k3s_host_utilities_stage_script() -> String {
    let start_index = CORE_STATIC_BINARIES.len() + 1;
    let total = total_build_steps();
    let mut script = format!(
        "# {start_index}-{total}. k3s host utilities from Alpine packages.\nfor bin in {} ; do\n  src=\"$(command -v \"$bin\")\"\n  cp \"$src\" \"/out/$bin\"\n  case \"$bin\" in\n",
        k3s_host_utilities_apk_packages()
    );
    for (offset, binary) in K3S_HOST_UTILITIES.iter().enumerate() {
        script.push_str(&format!("    {binary}) idx={} ;;\n", start_index + offset));
    }
    script.push_str(&format!(
        "  esac\n  echo \"[$idx/{total}] $bin OK\"\ndone\n"
    ));
    script
}

fn k3s_host_utilities_out_paths() -> String {
    K3S_HOST_UTILITIES
        .iter()
        .map(|binary| format!("/out/{binary}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn nfs_apk_packages() -> String {
    NFS_PACKAGES.join(" ")
}

fn nfs_stage_script() -> String {
    let start_index = CORE_STATIC_BINARIES.len() + K3S_HOST_UTILITIES.len() + 1;
    let total = total_build_steps();
    let mut script = format!(
        "# {start_index}-{total}. NFS client utilities from Alpine packages.\nfor bin in {} ; do\n  src=\"$(command -v \"$bin\")\"\n  cp \"$src\" \"/out/$bin\"\n  case \"$bin\" in\n",
        NFS_BINARIES.join(" ")
    );
    for (offset, binary) in NFS_BINARIES.iter().enumerate() {
        script.push_str(&format!("    {binary}) idx={} ;;\n", start_index + offset));
    }
    script.push_str(&format!(
        "  esac\n  echo \"[$idx/{total}] $bin OK\"\ndone\n"
    ));
    script
}

fn nfs_out_paths() -> String {
    NFS_BINARIES
        .iter()
        .map(|binary| format!("/out/{binary}"))
        .collect::<Vec<_>>()
        .join(" ")
}

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
    let utility_packages = k3s_host_utilities_apk_packages();
    let utility_stage_script = k3s_host_utilities_stage_script();
    let utility_out_paths = k3s_host_utilities_out_paths();
    let nfs_packages = nfs_apk_packages();
    let nfs_stage_script = nfs_stage_script();
    let nfs_out_paths = nfs_out_paths();
    let nfs_binaries_list = NFS_BINARIES.join(" ");
    let total = total_build_steps();

    // Step 1: Build core static binaries and stage packaged k3s host utilities.
    println!("==> Building rootfs binaries via Docker ({docker_platform})");
    let docker_script = format!(
        r#"
set -e

apk add --no-cache \
  build-base git autoconf automake libtool pkgconf \
  linux-headers \
  util-linux-dev util-linux-static \
  zlib-dev zlib-static \
  lzo-dev \
  zstd-dev zstd-static \
  busybox-static ca-certificates \
  {utility_packages} \
  {nfs_packages}

# 1. busybox (pre-built static from Alpine)
cp /bin/busybox.static /out/busybox
echo "[1/{total}] busybox (static) OK"

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
echo "[2/{total}] mkfs.btrfs (static) OK"

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
echo "[3/{total}] iptables-legacy (static) OK"

{utility_stage_script}

{nfs_stage_script}

# Shared libraries needed by packaged utilities.
mkdir -p /out/lib
cp -L /lib/ld-musl-*.so.1 /out/lib/
for bin in {utility_out_paths} {nfs_out_paths}; do
  ldd "$bin" | awk '/=>/ {{ print $3 }} /^\// {{ print $1 }}' | while read -r lib; do
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
for bin in {utility_packages} {nfs_binaries_list}; do
  printf "  %-16s " "$bin"
  if ldd "/out/$bin" >/dev/null 2>&1; then
    echo "dynamic OK"
  else
    echo "static OK"
  fi
done
ls -lh /out/busybox /out/mkfs.btrfs /out/iptables {utility_out_paths} {nfs_out_paths}
"#
    );

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
            &docker_script,
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
    build_erofs_image_with_docker(docker_platform, &rootfs, &opts.output, &opts.compression)?;

    let size = humanize_size(std::fs::metadata(&opts.output)?.len());
    println!();
    println!("==> EROFS rootfs built: {} ({size})", opts.output.display());
    println!("    Compression: {}", opts.compression);
    println!("    Block size: {} bytes", EROFS_BLOCK_SIZE);
    println!(
        "    Contents: busybox + mkfs.btrfs + iptables-legacy + ebtables + ethtool + socat + mount.nfs + CA certs + trampoline"
    );
    println!("    Core boot tools are static; packaged utilities include required shared libs");

    Ok(())
}

fn build_erofs_image_with_docker(
    docker_platform: &str,
    rootfs: &Path,
    output: &Path,
    compression: &str,
) -> Result<()> {
    let output_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid output filename: {}", output.display()))?;
    let output_dir = output
        .parent()
        .ok_or_else(|| anyhow::anyhow!("output path has no parent: {}", output.display()))?;
    std::fs::create_dir_all(output_dir)?;

    // Install erofs-utils inside the container first.
    let install_and_run =
        format!("apk add --no-cache erofs-utils >/dev/null && exec mkfs.erofs \"$@\"",);

    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            docker_platform,
            "-v",
            &format!("{}:/rootfs:ro", rootfs.display()),
            "-v",
            &format!("{}:/out", output_dir.display()),
            "alpine:3.19",
            "sh",
            "-c",
            &install_and_run,
            // Everything after here becomes positional args ($@) for mkfs.erofs.
            "--",
        ])
        .arg(mkfs_erofs_block_flag())
        .arg(format!("-x{EROFS_XATTR_TOLERANCE}"))
        .arg(format!("-z{compression}"))
        .arg(format!("/out/{output_name}"))
        .arg("/rootfs")
        .status()
        .context("failed to run docker for mkfs.erofs")?;
    if !status.success() {
        bail!("docker mkfs.erofs failed");
    }

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
    for binary in NFS_BINARIES {
        copy_executable(&staging.join(binary), &sbin_dir.join(binary))?;
    }
    for link in IPTABLES_SYMLINKS {
        std::os::unix::fs::symlink("iptables", sbin_dir.join(link))?;
    }

    // /sbin/init — trampoline
    std::fs::write(sbin_dir.join("init"), INIT_SCRIPT)?;
    set_executable(&sbin_dir.join("init"))?;

    // /lib — dynamic loader and shared libs for packaged utilities.
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

fn mkfs_erofs_block_flag() -> String {
    format!("-b{EROFS_BLOCK_SIZE}")
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

#[cfg(test)]
mod tests {
    use super::mkfs_erofs_block_flag;

    #[test]
    fn mkfs_erofs_block_flag_uses_4k_syntax() {
        assert_eq!(mkfs_erofs_block_flag(), "-b4096");
    }

    #[test]
    fn erofs_xattr_tolerance_disables_xattrs() {
        assert_eq!(super::EROFS_XATTR_TOLERANCE, "-1");
    }
}
