use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::rootfs::{BuildRootfsOpts, build_rootfs};

#[derive(Args)]
pub struct BuildRootfsArgs {
    /// Output EROFS image path.
    #[arg(long)]
    output: PathBuf,
    /// Target architecture.
    #[arg(long, default_value = "arm64")]
    arch: String,
    /// EROFS compression algorithm.
    #[arg(long, default_value = "lz4hc")]
    compression: String,
}

impl BuildRootfsArgs {
    pub fn run(self) -> Result<()> {
        build_rootfs(&BuildRootfsOpts {
            output: self.output,
            arch: self.arch,
            compression: self.compression,
        })
    }
}
