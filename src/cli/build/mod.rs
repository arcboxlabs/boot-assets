mod fex;
mod release;
mod rootfs;

use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct BuildArgs {
    #[command(subcommand)]
    command: BuildCommands,
}

#[derive(Subcommand)]
enum BuildCommands {
    /// Build FEX from source and append runtime entries to binaries JSON.
    Fex(fex::BuildFexArgs),
    /// Build minimal EROFS rootfs from Alpine static binaries.
    Rootfs(rootfs::BuildRootfsArgs),
    /// Assemble a release tarball (kernel + rootfs.erofs + manifest.json).
    Release(release::BuildReleaseArgs),
}

impl BuildArgs {
    pub fn run(self) -> Result<()> {
        match self.command {
            BuildCommands::Fex(args) => args.run(),
            BuildCommands::Rootfs(args) => args.run(),
            BuildCommands::Release(args) => args.run(),
        }
    }
}
