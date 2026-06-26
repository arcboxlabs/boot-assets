use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;

use crate::build::release::merge_manifests;
use arcbox_boot::manifest::Manifest;
use arcbox_boot::util::{read_json_file, write_json_pretty};

#[derive(Args)]
pub struct MergeManifestArgs {
    /// Manifest files to merge (at least 2).
    #[arg(required = true)]
    manifests: Vec<PathBuf>,
    /// Output path for the merged manifest.
    #[arg(long, default_value = "manifest.json")]
    output: PathBuf,
}

impl MergeManifestArgs {
    pub fn run(self) -> Result<()> {
        let mut manifests = self
            .manifests
            .iter()
            .map(|p| {
                read_json_file::<Manifest>(p)
                    .with_context(|| format!("failed to load manifest {}", p.display()))
            })
            .collect::<Result<Vec<_>>>()?;

        let mut base = manifests.remove(0);
        for other in &manifests {
            merge_manifests(&mut base, other)?;
        }

        write_json_pretty(&self.output, &base)?;

        println!(
            "==> Merged {} manifests -> {} ({} targets, {} binaries)",
            self.manifests.len(),
            self.output.display(),
            base.targets.len(),
            base.binaries.len(),
        );

        Ok(())
    }
}
