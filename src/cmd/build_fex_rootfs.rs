use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha256};

use arcbox_boot::manifest::{Binary, BinaryTarget};

const FEX_ROOTFS_NAME: &str = "rootfs.ero";
const FEX_ROOTFS_ARCH: &str = "arm64";
const FEX_ROOTFS_INSTALL_DIR: &str = "fex";
const DEFAULT_SUITE: &str = "bookworm";
const DEFAULT_MIRROR: &str = "http://deb.debian.org/debian";
const DEFAULT_VERSION: &str = "arcbox-fex-bookworm";

const DEBIAN_PACKAGES: &[&str] = &[
    "ca-certificates",
    "libc6",
    "libstdc++6",
    "libgcc-s1",
    "zlib1g",
    "libzstd1",
    "liblzma5",
    "libssl3",
    "libnss3",
    "libidn2-0",
    "libpsl5",
    "libnghttp2-14",
    "libcurl4",
    "libsqlite3-0",
];

const REQUIRED_ROOTFS_PATHS: &[&str] = &[
    "lib64/ld-linux-x86-64.so.2",
    "lib/x86_64-linux-gnu/libc.so.6",
    "lib/x86_64-linux-gnu/libm.so.6",
    "lib/x86_64-linux-gnu/libgcc_s.so.1",
    "usr/lib/x86_64-linux-gnu/libstdc++.so.6",
    "etc/nsswitch.conf",
    "etc/ssl/certs/ca-certificates.crt",
];

const CLEAN_PATHS: &[&str] = &[
    "var/lib/apt/lists",
    "var/cache/apt",
    "var/log",
    "usr/share/doc",
    "usr/share/man",
    "usr/share/info",
    "usr/share/lintian",
    "usr/share/locale",
    "tmp",
    "var/tmp",
    "boot",
    "home",
    "media",
    "mnt",
    "opt",
    "root",
    "srv",
];

#[derive(Args)]
pub struct BuildFexRootfsArgs {
    /// Output directory. Files are written to {output}/rootfs.ero/{version}/arm64/rootfs.ero.
    #[arg(long, default_value = "dist/bin")]
    output: PathBuf,
    /// Rootfs version used in the ArcBox binary manifest path.
    #[arg(long, default_value = DEFAULT_VERSION)]
    version: String,
    /// Append the FEX rootfs entry to this JSON manifest fragment.
    #[arg(long)]
    binaries_json: PathBuf,
    /// Debian suite to bootstrap.
    #[arg(long, default_value = DEFAULT_SUITE)]
    suite: String,
    /// Debian mirror URL.
    #[arg(long, default_value = DEFAULT_MIRROR)]
    mirror: String,
    /// EROFS compression algorithm and level.
    #[arg(long, default_value = "lz4hc,9")]
    compression: String,
}

impl BuildFexRootfsArgs {
    pub fn run(self) -> Result<()> {
        let work = tempfile::tempdir().context("failed to create FEX rootfs temp dir")?;
        let rootfs = work.path().join("rootfs");
        let image = work.path().join(FEX_ROOTFS_NAME);

        bootstrap_debian_rootfs(&self.suite, &self.mirror, &rootfs)?;
        install_config_files(work.path(), &rootfs)?;
        clean_rootfs(&rootfs)?;
        ensure_runtime_dirs(&rootfs)?;
        validate_rootfs(&rootfs)?;
        build_erofs_image(&rootfs, &image, &self.compression)?;

        let entry = stage_rootfs(&self.output, &self.version, &image)?;
        append_binaries_json(&self.binaries_json, vec![entry])?;

        println!("==> Debian FEX rootfs built from {}", self.suite);
        println!("    Output: {}", self.output.display());
        println!("    Manifest: {}", self.binaries_json.display());

        cleanup_root_owned_temp(work.path())?;

        Ok(())
    }
}

fn bootstrap_debian_rootfs(suite: &str, mirror: &str, rootfs: &Path) -> Result<()> {
    println!("==> Bootstrapping Debian {suite} x86_64 FEX rootfs");
    let include = DEBIAN_PACKAGES.join(",");
    let include_arg = format!("--include={include}");
    let status = Command::new("sudo")
        .args([
            "debootstrap",
            "--arch=amd64",
            "--variant=minbase",
            &include_arg,
            suite,
            path_str(rootfs)?,
            mirror,
        ])
        .status()
        .context("failed to run debootstrap")?;
    if !status.success() {
        bail!("debootstrap failed for Debian suite {suite}");
    }
    Ok(())
}

fn install_config_files(work: &Path, rootfs: &Path) -> Result<()> {
    println!("==> Installing FEX rootfs config files");
    let config_dir = work.join("config");
    std::fs::create_dir_all(&config_dir)?;

    let configs = [
        (
            "nsswitch.conf",
            "passwd: files\ngroup: files\nshadow: files\nhosts: files dns\nnetworks: files\nprotocols: db files\nservices: db files\nethers: db files\nrpc: db files\n",
            "etc/nsswitch.conf",
        ),
        ("hosts", "127.0.0.1 localhost\n::1 localhost\n", "etc/hosts"),
        (
            "passwd",
            "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n",
            "etc/passwd",
        ),
        ("group", "root:x:0:\nnogroup:x:65534:\n", "etc/group"),
    ];

    for (name, contents, dest) in configs {
        let src = config_dir.join(name);
        std::fs::write(&src, contents)?;
        let dest_path = rootfs.join(dest);
        let parent = dest_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent: {dest}"))?;
        sudo("mkdir", &["-p", path_str(parent)?])?;
        sudo("cp", &[path_str(&src)?, path_str(&dest_path)?])?;
    }

    Ok(())
}

fn clean_rootfs(rootfs: &Path) -> Result<()> {
    println!("==> Cleaning Debian FEX rootfs");
    for path in CLEAN_PATHS {
        sudo("rm", &["-rf", path_str(&rootfs.join(path))?])?;
    }
    Ok(())
}

fn ensure_runtime_dirs(rootfs: &Path) -> Result<()> {
    for path in ["tmp", "var/tmp", "proc", "sys", "dev"] {
        sudo("mkdir", &["-p", path_str(&rootfs.join(path))?])?;
    }
    sudo("chmod", &["1777", path_str(&rootfs.join("tmp"))?])?;
    sudo("chmod", &["1777", path_str(&rootfs.join("var/tmp"))?])?;
    Ok(())
}

fn validate_rootfs(rootfs: &Path) -> Result<()> {
    println!("==> Validating FEX rootfs essentials");
    for path in REQUIRED_ROOTFS_PATHS {
        let candidate = rootfs.join(path);
        if !candidate.exists() {
            bail!("FEX rootfs is missing required path: {path}");
        }
    }
    Ok(())
}

fn build_erofs_image(rootfs: &Path, image: &Path, compression: &str) -> Result<()> {
    println!("==> Building FEX rootfs EROFS image");
    let status = Command::new("mkfs.erofs")
        .arg(format!("-z{compression}"))
        .arg("-b4096")
        .arg(image)
        .arg(rootfs)
        .status()
        .context("failed to run mkfs.erofs for FEX rootfs")?;
    if !status.success() {
        bail!("mkfs.erofs failed for FEX rootfs");
    }
    Ok(())
}

fn stage_rootfs(output: &Path, version: &str, src: &Path) -> Result<Binary> {
    let dest = output
        .join(FEX_ROOTFS_NAME)
        .join(version)
        .join(FEX_ROOTFS_ARCH)
        .join(FEX_ROOTFS_NAME);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, &dest)
        .with_context(|| format!("failed to copy {} to {}", src.display(), dest.display()))?;

    let mut targets = BTreeMap::new();
    targets.insert(
        FEX_ROOTFS_ARCH.to_string(),
        BinaryTarget {
            path: manifest_path(version),
            sha256: sha256_file(&dest)?,
        },
    );

    Ok(Binary {
        name: FEX_ROOTFS_NAME.to_string(),
        version: version.to_string(),
        targets,
        install_dir: Some(FEX_ROOTFS_INSTALL_DIR.to_string()),
    })
}

fn append_binaries_json(path: &Path, mut entries: Vec<Binary>) -> Result<()> {
    let mut existing = if path.exists() {
        let bytes =
            std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_slice::<Vec<Binary>>(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?
    } else {
        Vec::new()
    };

    existing.append(&mut entries);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&existing)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn sudo(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new("sudo")
        .arg(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run sudo {program}"))?;
    if !status.success() {
        bail!("sudo {program} failed");
    }
    Ok(())
}

fn cleanup_root_owned_temp(path: &Path) -> Result<()> {
    if path.exists() {
        sudo("rm", &["-rf", path_str(path)?])?;
    }
    Ok(())
}

fn manifest_path(version: &str) -> String {
    format!("bin/{FEX_ROOTFS_NAME}/{version}/{FEX_ROOTFS_ARCH}/{FEX_ROOTFS_NAME}")
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        FEX_ROOTFS_ARCH, FEX_ROOTFS_INSTALL_DIR, FEX_ROOTFS_NAME, REQUIRED_ROOTFS_PATHS,
        manifest_path,
    };

    #[test]
    fn manifest_path_uses_fex_install_layout() {
        assert_eq!(
            manifest_path("arcbox-fex-bookworm-test"),
            "bin/rootfs.ero/arcbox-fex-bookworm-test/arm64/rootfs.ero"
        );
    }

    #[test]
    fn rootfs_manifest_identity_matches_runtime_expectation() {
        assert_eq!(FEX_ROOTFS_NAME, "rootfs.ero");
        assert_eq!(FEX_ROOTFS_ARCH, "arm64");
        assert_eq!(FEX_ROOTFS_INSTALL_DIR, "fex");
    }

    #[test]
    fn required_paths_include_glibc_loader_and_nss() {
        assert!(REQUIRED_ROOTFS_PATHS.contains(&"lib64/ld-linux-x86-64.so.2"));
        assert!(REQUIRED_ROOTFS_PATHS.contains(&"lib/x86_64-linux-gnu/libc.so.6"));
        assert!(REQUIRED_ROOTFS_PATHS.contains(&"etc/nsswitch.conf"));
    }
}
