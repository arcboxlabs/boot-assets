use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::build::sync_binaries::{sync_binaries, SyncBinariesOpts};

const DEFAULT_ARCHES: &[&str] = &["arm64", "x86_64"];

#[derive(Args)]
pub struct SyncBinariesArgs {
    /// Path to upstream.toml.
    #[arg(long, default_value = "upstream.toml")]
    config: PathBuf,
    /// Output directory. Binaries are written to {output}/{name}/{version}/{arch}/{name}.
    #[arg(long, default_value = "dist/bin")]
    output: PathBuf,
    /// CDN base URL to check for existing binaries (skip download if present).
    #[arg(long)]
    cdn_base_url: Option<String>,
    /// Only process these architectures (comma-separated). Default: arm64,x86_64.
    #[arg(long, value_delimiter = ',')]
    arch: Option<Vec<String>>,
    /// Write binary manifest entries to this JSON file (for build release --binaries-json).
    #[arg(long)]
    binaries_json: Option<PathBuf>,
}

impl SyncBinariesArgs {
    pub fn run(self) -> Result<()> {
        sync_binaries(&SyncBinariesOpts {
            config: self.config,
            output: self.output,
            cdn_base_url: self.cdn_base_url,
            arches: self.arch.unwrap_or_else(|| {
                DEFAULT_ARCHES
                    .iter()
                    .map(|arch| (*arch).to_string())
                    .collect()
            }),
            binaries_json: self.binaries_json,
        })
    }
}
