use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use clap::Args;

use arcbox_boot::upstream::{UpstreamConfig, UpstreamSource};

use crate::build::containerd::{BuildContainerdOpts, build_containerd};

const DEFAULT_CONTAINERD_REPO: &str = "https://github.com/containerd/containerd.git";
/// The containerd release bundled in the Docker static package that
/// `upstream.toml` pins. Bump this in the same change as the Docker version,
/// or the guest gets a containerd its dockerd never shipped with.
const DEFAULT_CONTAINERD_REF: &str = "v2.3.3";
/// Directory (relative to CWD) of vendored `*.patch` files applied after clone.
const DEFAULT_PATCHES_DIR: &str = "patches/containerd";
/// Member of the Docker static tarball holding the containerd we replace.
const VANILLA_CONTAINERD_MEMBER: &str = "docker/containerd";

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
    /// Bump it when the patches change without a Docker bump.
    #[arg(long, default_value_t = 1)]
    patch_level: u32,
    /// Asset release this build belongs to (e.g. `0.8.6`), appended last.
    ///
    /// Load-bearing, and the reason the Docker version alone is not enough:
    /// every release rebuilds containerd in a fresh temp dir, so the bytes
    /// differ while the size usually does not — and the B2 sync is
    /// `--size-only`. Reusing a key across releases would leave the CDN
    /// serving the previous release's binary against the new release's
    /// sha256, which fails closed in the daemon's checksum check. Same reason
    /// FEX stamps the release into its version.
    #[arg(long)]
    release_version: String,
    /// Version compiled into the binary (`containerd --version`). Defaults to
    /// the source ref plus the same patch-level and release suffixes the
    /// manifest version carries.
    ///
    /// It has to say two things: that this is not stock containerd, and which
    /// build it is. The bytes genuinely differ per release — that is why the
    /// CDN key stamps one in — so a version stopping at `-arcbox.1` would
    /// leave a guest unable to report which build it is running, losing
    /// exactly the diagnostic this component argues hardest for keeping.
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
        let docker = docker_package(&self.upstream, &self.arch)?;
        let version = format!(
            "{}-arcbox.{}-{}",
            docker.version, self.patch_level, self.release_version
        );
        let internal_version = self.internal_version.unwrap_or_else(|| {
            format!(
                "{}-arcbox.{}-{}",
                self.source_ref, self.patch_level, self.release_version
            )
        });
        build_containerd(&BuildContainerdOpts {
            repo: self.repo,
            source_ref: self.source_ref,
            arch: self.arch,
            output: self.output,
            version,
            internal_version,
            binaries_json: self.binaries_json,
            patches_dir: self.patches_dir,
            vanilla_source: docker.vanilla_containerd,
        })
    }
}

/// What the pinned Docker package contributes to this build.
struct DockerPackage {
    /// The package version the sibling guest binaries carry.
    version: String,
    /// Where to get the package's own containerd, for the compatibility check
    /// in `build_containerd`.
    vanilla_containerd: UpstreamSource,
}

/// Reads the Docker package the guest runtime is pinned to.
///
/// `dockerd` is the anchor: it is the binary containerd has to be
/// release-compatible with, and unlike containerd it is still declared in
/// `upstream.toml`. Its source doubles as the source of the *vanilla*
/// containerd — same tarball, different member — which is what lets the build
/// check its own `--source-ref` against reality rather than trusting that
/// whoever bumped Docker remembered to bump it too.
fn docker_package(upstream: &Path, arch: &str) -> Result<DockerPackage> {
    let config = UpstreamConfig::from_file(upstream).map_err(anyhow::Error::msg)?;
    let dockerd = config
        .binaries
        .iter()
        .find(|binary| binary.name == "dockerd")
        .ok_or_else(|| {
            anyhow!(
                "no `dockerd` entry in {} to take the containerd package version from",
                upstream.display()
            )
        })?;
    let source = dockerd
        .source
        .get(arch)
        .ok_or_else(|| anyhow!("`dockerd` in {} has no {arch} source", upstream.display()))?;

    Ok(DockerPackage {
        version: dockerd.version.clone(),
        vanilla_containerd: UpstreamSource {
            extract: Some(VANILLA_CONTAINERD_MEMBER.to_string()),
            ..source.clone()
        },
    })
}
