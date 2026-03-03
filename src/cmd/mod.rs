mod build_release;
mod build_rootfs;
mod merge_manifest;
mod sync_binaries;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "boot-assets", about = "ArcBox boot asset builder")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build minimal EROFS rootfs from Alpine static binaries.
    BuildRootfs(build_rootfs::BuildRootfsArgs),
    /// Assemble a release tarball (kernel + rootfs.erofs + manifest.json).
    BuildRelease(build_release::BuildReleaseArgs),
    /// Merge multiple single-arch manifests into one multi-arch manifest.
    MergeManifest(merge_manifest::MergeManifestArgs),
    /// Download upstream binaries and stage them for R2 upload.
    SyncBinaries(sync_binaries::SyncBinariesArgs),
}

impl Cli {
    pub fn run(self) -> Result<()> {
        match self.command {
            Commands::BuildRootfs(args) => args.run(),
            Commands::BuildRelease(args) => args.run(),
            Commands::MergeManifest(args) => args.run(),
            Commands::SyncBinaries(args) => args.run(),
        }
    }
}
