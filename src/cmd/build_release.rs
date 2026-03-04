use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::release::{BuildReleaseOpts, build_release};

#[derive(Args)]
pub struct BuildReleaseArgs {
    /// Asset version (e.g. 0.2.0).
    #[arg(long)]
    version: String,
    /// Path to pre-built kernel binary.
    #[arg(long)]
    kernel: PathBuf,
    /// Target architecture.
    #[arg(long, default_value = "arm64")]
    arch: String,
    /// Path to pre-built rootfs.erofs (skip build).
    #[arg(long)]
    rootfs: Option<PathBuf>,
    /// Output directory.
    #[arg(long, default_value = "dist")]
    output_dir: PathBuf,
    /// EROFS compression algorithm.
    #[arg(long, default_value = "lz4hc")]
    compression: String,
    /// Source repository for manifest metadata.
    #[arg(long)]
    source_repo: Option<String>,
    /// Source ref for manifest metadata.
    #[arg(long)]
    source_ref: Option<String>,
    /// Source SHA for manifest metadata.
    #[arg(long)]
    source_sha: Option<String>,
    /// Kernel version for manifest metadata.
    #[arg(long)]
    kernel_version: Option<String>,
    /// Path to JSON file with binary entries (output of `sync-binaries`).
    #[arg(long)]
    binaries_json: Option<PathBuf>,
}

impl BuildReleaseArgs {
    pub fn run(self) -> Result<()> {
        build_release(&BuildReleaseOpts {
            version: self.version,
            arch: self.arch,
            kernel_path: self.kernel,
            rootfs_erofs_path: self.rootfs,
            output_dir: self.output_dir,
            erofs_compression: self.compression,
            source_repo: self.source_repo,
            source_ref: self.source_ref,
            source_sha: self.source_sha,
            kernel_version: self.kernel_version,
            binaries_json: self.binaries_json,
        })
    }
}
