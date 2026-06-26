use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::build::fex::{build_fex, BuildFexOpts};

const DEFAULT_FEX_REPO: &str = "https://github.com/FEX-Emu/FEX.git";
const DEFAULT_FEX_REF: &str = "FEX-2605";
/// Directory (relative to CWD) of vendored `*.patch` files applied to the FEX
/// source after clone.
const DEFAULT_PATCHES_DIR: &str = "patches/fex";

#[derive(Args)]
pub struct BuildFexArgs {
    /// FEX git repository URL.
    #[arg(long, default_value = DEFAULT_FEX_REPO)]
    repo: String,
    /// FEX git ref/tag to build.
    #[arg(long, default_value = DEFAULT_FEX_REF)]
    source_ref: String,
    /// Output directory. Files are written to {output}/{name}/{version}/arm64/{name}.
    #[arg(long, default_value = "dist/bin")]
    output: PathBuf,
    /// Runtime version used in the ArcBox binary manifest path
    /// (`bin/FEX/{version}/arm64/FEX`). Defaults to the source ref.
    ///
    /// Uploads must pass a version unique to the release (e.g.
    /// `FEX-2605-0.5.12`): FEX builds are not bit-reproducible, so reusing a
    /// CDN key for a rebuilt binary desyncs the cached/size-matched object
    /// from the sha256 pinned in the release manifest.
    #[arg(long)]
    version: Option<String>,
    /// Append FEX entries to this JSON manifest fragment.
    #[arg(long)]
    binaries_json: PathBuf,
    /// Directory of `*.patch` files applied to the FEX source after clone.
    #[arg(long, default_value = DEFAULT_PATCHES_DIR)]
    patches_dir: PathBuf,
}

impl BuildFexArgs {
    pub fn run(self) -> Result<()> {
        let version = self.version.unwrap_or_else(|| self.source_ref.clone());
        build_fex(&BuildFexOpts {
            repo: self.repo,
            source_ref: self.source_ref,
            output: self.output,
            version,
            binaries_json: self.binaries_json,
            patches_dir: self.patches_dir,
        })
    }
}
