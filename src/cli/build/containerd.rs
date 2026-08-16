use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use clap::Args;

use arcbox_boot::upstream::UpstreamConfig;

use crate::build::containerd::{BuildContainerdOpts, build_containerd};

const DEFAULT_CONTAINERD_REPO: &str = "https://github.com/containerd/containerd.git";
/// The containerd release bundled in the Docker static package that
/// `upstream.toml` pins. Bump this in the same change as the Docker version,
/// or the guest gets a containerd its dockerd never shipped with.
const DEFAULT_CONTAINERD_REF: &str = "v2.3.3";
/// Directory (relative to CWD) of vendored `*.patch` files applied after clone.
const DEFAULT_PATCHES_DIR: &str = "patches/containerd";

#[derive(Args)]
pub struct BuildContainerdArgs {
    /// containerd git repository URL.
    #[arg(long, default_value = DEFAULT_CONTAINERD_REPO)]
    repo: String,
    /// containerd git tag to build. Must match the containerd bundled in the
    /// Docker static package pinned by `upstream.toml`.
    #[arg(long, default_value = DEFAULT_CONTAINERD_REF)]
    source_ref: String,
    /// Target architecture (`arm64` or `x86_64`). The build is native, so this
    /// must match the runner — it only names the CDN path.
    #[arg(long)]
    arch: String,
    /// Output directory. Files are written to
    /// {output}/{name}/{version}/{arch}/{name}.
    #[arg(long, default_value = "dist/bin")]
    output: PathBuf,
    /// Upstream declaration file the sibling binaries' version is read from.
    ///
    /// The manifest version is derived rather than passed so there is one
    /// source of truth: bumping the Docker package in `upstream.toml` moves
    /// containerd's CDN key with it, instead of leaving a second place to
    /// forget.
    #[arg(long, default_value = "upstream.toml")]
    upstream: PathBuf,
    /// Patch-set generation, appended as `-arcbox.<N>`.
    ///
    /// Bump it when the patches change without a Docker bump. The suffix is
    /// not cosmetic: the vanilla object for the same Docker version already
    /// exists on the CDN, Go builds are not bit-reproducible, and the B2 sync
    /// is `--size-only`, so a reused key serves bytes that do not match the
    /// manifest's sha256.
    #[arg(long, default_value_t = 1)]
    patch_level: u32,
    /// Version compiled into the binary (`containerd --version`). Defaults to
    /// the source ref with an `-arcbox` suffix so a running daemon's logs say
    /// plainly that this is not stock containerd.
    #[arg(long)]
    internal_version: Option<String>,
    /// Append the containerd entry to this JSON manifest fragment.
    #[arg(long)]
    binaries_json: PathBuf,
    /// Directory of `*.patch` files applied to the containerd source.
    #[arg(long, default_value = DEFAULT_PATCHES_DIR)]
    patches_dir: PathBuf,
}

impl BuildContainerdArgs {
    pub fn run(self) -> Result<()> {
        let package_version = docker_package_version(&self.upstream)?;
        let version = format!("{package_version}-arcbox.{}", self.patch_level);
        let internal_version = self
            .internal_version
            .unwrap_or_else(|| format!("{}-arcbox.{}", self.source_ref, self.patch_level));
        build_containerd(&BuildContainerdOpts {
            repo: self.repo,
            source_ref: self.source_ref,
            arch: self.arch,
            output: self.output,
            version,
            internal_version,
            binaries_json: self.binaries_json,
            patches_dir: self.patches_dir,
        })
    }
}

/// Reads the Docker package version the guest runtime is pinned to.
///
/// `dockerd` is the anchor: it is the binary containerd has to be
/// release-compatible with, and unlike containerd it is still declared in
/// `upstream.toml`.
fn docker_package_version(upstream: &Path) -> Result<String> {
    let config = UpstreamConfig::from_file(upstream).map_err(anyhow::Error::msg)?;
    config
        .binaries
        .iter()
        .find(|binary| binary.name == "dockerd")
        .map(|binary| binary.version.clone())
        .ok_or_else(|| {
            anyhow!(
                "no `dockerd` entry in {} to take the containerd package version from",
                upstream.display()
            )
        })
}
