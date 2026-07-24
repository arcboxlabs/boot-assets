//! Read-only EROFS image of the guest container-runtime binaries.
//!
//! The guest reaches `dockerd`/`containerd`/the shim/`runc` over the
//! host-backed VirtioFS share today, which costs a FUSE round-trip per exec —
//! measured 7-10x more than exec'ing the same binary from block-backed
//! storage, and paid on every container start (so on every `docker build`
//! step). Packing them into a read-only image the VM attaches as a block
//! device moves that cost off VirtioFS on both hypervisor backends.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs_err as fs;
use xshell::{Shell, cmd};

use crate::build::rootfs::mkfs_erofs_block_flag;
use arcbox_boot::manifest::Binary;
use arcbox_boot::util::path_safe_version;

/// Resolves where `sync-binaries` left a binary's local copy.
///
/// It writes `{output}/{name}/{version}/{arch}/{name}`, but binaries staged
/// by other means (FEX, which the release workflow copies straight in) sit
/// flat at `{output}/{name}`. Checking both keeps the caller agnostic; the
/// guest needs a *flat* tree either way, which is what the staging below
/// produces.
fn locate_binary(bin_dir: &Path, binary: &Binary, arch: &str) -> Option<PathBuf> {
    let nested = bin_dir
        .join(&binary.name)
        .join(path_safe_version(&binary.version))
        .join(arch)
        .join(&binary.name);
    if nested.is_file() {
        return Some(nested);
    }
    let flat = bin_dir.join(&binary.name);
    flat.is_file().then_some(flat)
}

/// Packs the guest runtime binaries into a read-only EROFS image at `output`.
///
/// `bin_dir` is the `sync-binaries` output root and `binaries` the manifest
/// entries naming what to pack; each is staged flat so the guest finds
/// `dockerd`, `runc`, … directly at the mount point.
///
/// Runs `mkfs.erofs` in a container (as the rootfs build does) so the build
/// host needs no erofs-utils. The container runs natively rather than under
/// the target platform: the image only carries file bytes, so packing arm64
/// binaries from an x86_64 runner is sound and avoids qemu emulation.
pub fn build_runtime_image(
    bin_dir: &Path,
    binaries: &[Binary],
    arch: &str,
    output: &Path,
    compression: &str,
) -> Result<()> {
    let sh = Shell::new()?;

    let output_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid output filename: {}", output.display()))?;
    let output_dir = output
        .parent()
        .ok_or_else(|| anyhow::anyhow!("output path has no parent: {}", output.display()))?;
    fs::create_dir_all(output_dir)?;

    // Stage a flat tree: the guest resolves the runtime binaries directly at
    // the mount point, so packing `sync-binaries`' nested layout verbatim
    // would produce an image the guest silently ignores.
    let stage_dir = tempfile::tempdir().context("failed to create runtime image staging dir")?;
    let stage = stage_dir.path();
    let mut staged = 0_usize;
    for binary in binaries {
        // Not every manifest binary exists for every arch (FEX is arm64-only),
        // and those simply do not belong in this arch's image.
        if !binary.targets.contains_key(arch) {
            continue;
        }
        let src = locate_binary(bin_dir, binary, arch).ok_or_else(|| {
            anyhow::anyhow!(
                "runtime binary {} ({arch}) not found under {}",
                binary.name,
                bin_dir.display()
            )
        })?;
        fs::copy(&src, stage.join(&binary.name))?;
        staged += 1;
    }
    if staged == 0 {
        bail!(
            "no runtime binaries staged for {arch} from {}",
            bin_dir.display()
        );
    }

    let script = "apk add --no-cache erofs-utils >/dev/null && exec mkfs.erofs \"$@\"";
    let stage_mount = format!("{}:/bin-src:ro", stage.display());
    let output_mount = format!("{}:/out", output_dir.display());
    // Pin the block size like the rootfs image does: mkfs.erofs otherwise
    // defaults to the *build host's* page size, and an image built with a
    // larger block than the guest's page size will not mount.
    let block_flag = mkfs_erofs_block_flag();
    let compression_flag = format!("-z{compression}");
    let output_path = format!("/out/{output_name}");

    println!("==> Building runtime image ({staged} binaries, {arch}, {compression})");
    cmd!(
        sh,
        "docker run --rm -v {stage_mount} -v {output_mount} alpine:3.19 sh -c {script} -- -T0 {block_flag} {compression_flag} {output_path} /bin-src"
    )
    .run()
    .context("docker mkfs.erofs (runtime image) failed")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn binary(name: &str, version: &str, arch: &str) -> Binary {
        let mut targets = BTreeMap::new();
        targets.insert(
            arch.to_string(),
            arcbox_boot::manifest::BinaryTarget {
                path: format!("bin/{name}/{version}/{arch}/{name}"),
                sha256: "0".repeat(64),
            },
        );
        Binary {
            name: name.to_string(),
            version: version.to_string(),
            targets,
            install_dir: None,
        }
    }

    #[test]
    fn locates_sync_binaries_nested_layout() {
        // sync-binaries writes {name}/{version}/{arch}/{name}; packing the
        // root verbatim would yield an image whose mount point has no
        // executables, which the guest silently ignores.
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("dockerd").join("29.6.1").join("arm64");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("dockerd"), b"elf").unwrap();

        let found = locate_binary(dir.path(), &binary("dockerd", "29.6.1", "arm64"), "arm64");
        assert_eq!(found.as_deref(), Some(nested.join("dockerd").as_path()));
    }

    #[test]
    fn falls_back_to_flat_layout() {
        // FEX is copied in flat by the release workflow rather than synced.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("FEXInterpreter"), b"elf").unwrap();

        let found = locate_binary(
            dir.path(),
            &binary("FEXInterpreter", "2605", "arm64"),
            "arm64",
        );
        assert_eq!(
            found.as_deref(),
            Some(dir.path().join("FEXInterpreter").as_path())
        );
    }

    #[test]
    fn reports_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        assert!(locate_binary(dir.path(), &binary("runc", "29.6.1", "arm64"), "arm64").is_none());
    }
}
