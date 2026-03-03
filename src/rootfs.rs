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

    // Step 1: Extract static binaries via Docker.
    println!("==> Extracting static binaries via Docker ({docker_platform})");
    let docker_script = r#"
set -e
apk add --no-cache busybox-static btrfs-progs iptables ca-certificates
cp /bin/busybox.static /out/busybox
if [ -f /sbin/mkfs.btrfs ]; then cp /sbin/mkfs.btrfs /out/mkfs.btrfs
elif [ -f /usr/sbin/mkfs.btrfs ]; then cp /usr/sbin/mkfs.btrfs /out/mkfs.btrfs
else echo "mkfs.btrfs not found" >&2; exit 1; fi
IPTABLES_BIN=""
for c in /sbin/iptables-legacy /usr/sbin/iptables-legacy /sbin/iptables /usr/sbin/iptables; do
  if [ -f "$c" ]; then IPTABLES_BIN="$c"; break; fi
done
if [ -z "$IPTABLES_BIN" ]; then echo "iptables not found" >&2; exit 1; fi
cp "$IPTABLES_BIN" /out/iptables
cp /lib/ld-musl-*.so.1 /out/
cp /etc/ssl/certs/ca-certificates.crt /out/ca-certificates.crt
"#;

    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            docker_platform,
            "-v",
            &format!("{}:/out", staging_path.display()),
            "alpine:latest",
            "sh",
            "-c",
            docker_script,
        ])
        .status()
        .context("failed to run docker")?;
    if !status.success() {
        bail!("docker extraction failed");
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
    println!("    Contents: busybox + mkfs.btrfs + iptables-legacy + CA certs + trampoline");

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
    for link in IPTABLES_SYMLINKS {
        std::os::unix::fs::symlink("iptables", sbin_dir.join(link))?;
    }

    // /sbin/init — trampoline
    std::fs::write(sbin_dir.join("init"), INIT_SCRIPT)?;
    set_executable(&sbin_dir.join("init"))?;

    // /lib — musl libc
    let lib_dir = rootfs.join("lib");
    std::fs::create_dir_all(&lib_dir)?;
    for entry in std::fs::read_dir(staging)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("ld-musl-") && name_str.ends_with(".so.1") {
            copy_executable(&entry.path(), &lib_dir.join(&name))?;
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
    match Command::new("mkfs.erofs").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
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
