use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;

use crate::release::merge_manifests;
use arcbox_boot::manifest::Manifest;

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
                let bytes =
                    std::fs::read(p).with_context(|| format!("failed to read {}", p.display()))?;
                let m: Manifest = serde_json::from_slice(&bytes)
                    .with_context(|| format!("failed to parse {}", p.display()))?;
                Ok(m)
            })
            .collect::<Result<Vec<_>>>()?;

        let mut base = manifests.remove(0);
        for other in &manifests {
            merge_manifests(&mut base, other)?;
        }

        let json = serde_json::to_string_pretty(&base)?;
        std::fs::write(&self.output, &json)?;

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
