//! Read-only EROFS image of the guest container-runtime binaries.
//!
//! The guest reaches `dockerd`/`containerd`/the shim/`runc` over the
//! host-backed VirtioFS share today, which costs a FUSE round-trip per exec —
//! measured 7-10x more than exec'ing the same binary from block-backed
//! storage, and paid on every container start (so on every `docker build`
//! step). Packing them into a read-only image the VM attaches as a block
//! device moves that cost off VirtioFS on both hypervisor backends.

use std::path::Path;

use anyhow::{Context, Result};
use xshell::{Shell, cmd};

/// Packs `bin_dir` into a read-only EROFS image at `output`.
///
/// Runs `mkfs.erofs` in a container (as the rootfs build does) so the build
/// host needs no erofs-utils. The container runs natively rather than under
/// the target platform: the image only carries file bytes, so packing arm64
/// binaries from an x86_64 runner is sound and avoids qemu emulation.
pub fn build_runtime_image(bin_dir: &Path, output: &Path, compression: &str) -> Result<()> {
    let sh = Shell::new()?;

    let output_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid output filename: {}", output.display()))?;
    let output_dir = output
        .parent()
        .ok_or_else(|| anyhow::anyhow!("output path has no parent: {}", output.display()))?;
    std::fs::create_dir_all(output_dir)?;

    // Unlike the rootfs image this carries no device nodes, so the source
    // tree can be packed straight from the read-only bind mount.
    let script = "apk add --no-cache erofs-utils >/dev/null && exec mkfs.erofs \"$@\"";
    let bin_mount = format!("{}:/bin-src:ro", bin_dir.display());
    let output_mount = format!("{}:/out", output_dir.display());
    let compression_flag = format!("-z{compression}");
    let output_path = format!("/out/{output_name}");

    println!(
        "==> Building runtime image from {} ({compression})",
        bin_dir.display()
    );
    cmd!(
        sh,
        "docker run --rm -v {bin_mount} -v {output_mount} alpine:3.19 sh -c {script} -- -T0 {compression_flag} {output_path} /bin-src"
    )
    .run()
    .context("docker mkfs.erofs (runtime image) failed")?;

    Ok(())
}
