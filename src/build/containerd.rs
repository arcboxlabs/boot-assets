//! Builds the guest `containerd` from upstream source with our patches applied.
//!
//! Every other guest runtime binary (dockerd, the shim, runc, docker-init)
//! comes from Docker's static package via `upstream.toml`. containerd does not,
//! because we need one fix that is not in any released containerd yet:
//! containerd#13805, which stops the overlay snapshotter appending `index=off`
//! over a configured `index=on`. Without it, `index=on,nfs_export=on` — the
//! mount options ArcBox needs to export live container rootfs over NFS — is
//! silently overridden and every overlay mount fails with `EINVAL`.
//!
//! The fix merged to containerd `main` on 2026-07-18, five days after v2.3.3
//! was tagged, and the backports to `release/2.0`/`2.2`/`2.3`
//! (containerd#13878/13879/13880) are open and unreviewed. This module is the
//! bridge until one of them ships in a containerd release Docker picks up.
//!
//! **This exists to be deleted.** When the backport lands in the containerd
//! release Docker bundles, drop this module, the patch, and the workflow job,
//! and restore the `containerd` entry in `upstream.toml`. The check is the
//! arity of `hasOption` in `plugins/snapshots/overlay/overlay.go`: three
//! arguments is the broken form, two is fixed.
//!
//! # Fidelity
//!
//! Docker ships **vanilla** upstream containerd — the revision its 29.7.2
//! binary reports is exactly what the `v2.3.3` tag dereferences to. So building
//! the same tag with `make STATIC=1` (which is what Docker's static package
//! uses: its binary carries cgo markers and no libc resolver symbols, matching
//! the `osusergo netgo static_build` tags that recipe adds) yields a binary
//! that differs from Docker's by exactly our patch. Keeping it that way is the
//! point: when containerd misbehaves, "is it the patch or the build?" has to
//! stay answerable.
//!
//! CGO stays enabled for the same reason. On Linux the only cgo-gated code in
//! containerd is the btrfs snapshotter, which we do not currently select
//! (dockerd picks `overlayfs` even though the guest's docker volume is btrfs) —
//! but dropping it would be a second deviation from Docker's build bought for
//! nothing, and it would quietly close a door the ArcBox NFS work may yet walk
//! through.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use xshell::{Shell, cmd};

use arcbox_boot::upstream::UpstreamSource;
use arcbox_boot::util::set_executable;

use super::sync_binaries::download_and_extract;
use super::vendored::{append_binaries_json, apply_patches, assert_static_executable, stage_file};

/// Only `containerd` itself is built here. The shim stays on Docker's copy:
/// the patch does not touch it, and the two only need to agree on the shim
/// API, which is unchanged within a release.
const CONTAINERD_BINARY: &str = "containerd";

#[derive(Debug, Clone)]
pub struct BuildContainerdOpts {
    pub repo: String,
    /// Upstream tag to build, e.g. `v2.3.3`. Must be the containerd version
    /// bundled in the Docker static package pinned by `upstream.toml`, or the
    /// guest ends up running a containerd that never shipped with its dockerd.
    pub source_ref: String,
    /// Target architecture (`arm64` / `x86_64`). The build is native — this
    /// only names the CDN path — so the runner must already be this arch.
    pub arch: String,
    pub output: PathBuf,
    /// Version recorded in the release manifest and used as the CDN key
    /// (`bin/containerd/{version}/{arch}/containerd`).
    ///
    /// Carries three things, and each earns its place — see
    /// [`crate::cli`]'s `build containerd` args: the sibling binaries' Docker
    /// package version, the patch-set generation, and the asset release. The
    /// first two keep this object off the vanilla key `sync-binaries`
    /// publishes; the third keeps successive releases off each other's. Go
    /// builds are not bit-reproducible and the B2 sync is `--size-only`, so a
    /// reused key leaves the CDN serving bytes that do not match the sha256 in
    /// the manifest.
    pub version: String,
    /// Version compiled into the binary and reported by `containerd
    /// --version`, e.g. `v2.3.3-arcbox.1`. Passed explicitly rather than left
    /// to the Makefile's `git describe`, which is unreliable in the shallow
    /// clone below — and, more importantly, so the running daemon's logs say
    /// out loud that this is not stock containerd.
    pub internal_version: String,
    pub binaries_json: PathBuf,
    pub patches_dir: PathBuf,
    /// Where to fetch the *vanilla* containerd Docker bundles, so the build
    /// can prove it is about to build the same release. Derived from
    /// `upstream.toml`'s dockerd entry — see [`assert_bundled_by_docker`].
    pub vanilla_source: UpstreamSource,
}

pub fn build_containerd(opts: &BuildContainerdOpts) -> Result<()> {
    let sh = Shell::new()?;
    let work = tempfile::tempdir().context("failed to create containerd build temp dir")?;
    let source = work.path().join("containerd");

    assert_bundled_by_docker(&opts.vanilla_source, &opts.source_ref, work.path())?;
    clone(&sh, &opts.repo, &opts.source_ref, &source)?;
    apply_patches(&sh, "containerd", &source, &opts.patches_dir)?;
    make_static(&sh, &source, &opts.internal_version)?;

    let built = source.join("bin").join(CONTAINERD_BINARY);
    if !built.is_file() {
        bail!("containerd build did not produce {}", built.display());
    }
    // `make STATIC=1` asks for a static link, but a cgo build that only
    // partially honours it still links cleanly here and only fails at guest
    // boot, where the EROFS rootfs has no dynamic loader to offer it.
    assert_static_executable("containerd", &built)?;

    let staged = stage_file(
        &opts.output,
        &opts.version,
        &opts.arch,
        CONTAINERD_BINARY,
        None,
        &built,
    )?;
    append_binaries_json(&opts.binaries_json, vec![staged])?;

    println!(
        "==> containerd {} built from {} ({})",
        opts.version, opts.source_ref, opts.arch
    );
    println!("    Output: {}", opts.output.display());
    println!("    Manifest: {}", opts.binaries_json.display());

    Ok(())
}

/// Fails unless the Docker package pinned in `upstream.toml` really bundles
/// the containerd release we are about to build.
///
/// The two are pinned independently — the Docker version lives in
/// `upstream.toml`, the containerd tag in `DEFAULT_CONTAINERD_REF` — and a
/// Docker bump that forgets the second would build the *old* containerd and
/// publish it labelled as belonging to the new Docker package. Nothing
/// downstream could tell: the manifest, the CDN key and the sha256 would all
/// be internally consistent, and the guest would just be running a containerd
/// its dockerd never shipped with.
///
/// A comment asking for both to move together is not a check, so this asks
/// Docker's own binary. `containerd --version` prints
/// `containerd github.com/containerd/containerd/v2 <version> <revision>`.
fn assert_bundled_by_docker(source: &UpstreamSource, source_ref: &str, work: &Path) -> Result<()> {
    let extract = source.extract.as_deref().ok_or_else(|| {
        anyhow!(
            "vanilla containerd source {} has no extract path",
            source.url
        )
    })?;
    let vanilla = work.join("vanilla-containerd");

    println!("==> Checking which containerd {} bundles", source.url);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(download_and_extract(&source.url, extract, &vanilla))?;
    set_executable(&vanilla)?;

    let output = Command::new(&vanilla)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run {} --version", vanilla.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version exited with {}",
            vanilla.display(),
            output.status
        );
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    let bundled = reported.split_whitespace().nth(2).ok_or_else(|| {
        anyhow!("could not read a version out of `containerd --version`: {reported:?}")
    })?;

    if bundled != source_ref {
        bail!(
            "the pinned Docker package bundles containerd {bundled}, but this build \
             targets {source_ref}. Point --source-ref at {bundled} (and re-check that \
             the patches still apply) when bumping the Docker version in upstream.toml."
        );
    }
    println!("    bundled containerd is {bundled}, as expected");
    Ok(())
}

fn clone(sh: &Shell, repo: &str, source_ref: &str, source: &Path) -> Result<()> {
    println!("==> Cloning containerd {source_ref}");
    cmd!(
        sh,
        "git clone --depth 1 --branch {source_ref} {repo} {source}"
    )
    .run()
    .with_context(|| format!("git clone failed for containerd ref {source_ref}"))
}

/// Runs containerd's own static-release recipe.
///
/// `STATIC=1` is what adds `osusergo netgo static_build` and `-extldflags
/// -static`; leaving CGO at its default keeps the btrfs snapshotter compiled
/// in, matching Docker's package. `VERSION` is overridden (the Makefile
/// declares it with `?=`) so the shallow clone's `git describe` never decides
/// it; `REVISION` is deliberately left alone so it keeps reporting the
/// upstream commit, with the Makefile's own `.m` dirty marker appended by the
/// applied patch.
fn make_static(sh: &Shell, source: &Path, internal_version: &str) -> Result<()> {
    println!("==> Building containerd (make STATIC=1)");
    let target = format!("bin/{CONTAINERD_BINARY}");
    let version_arg = format!("VERSION={internal_version}");
    cmd!(sh, "make -C {source} STATIC=1 {version_arg} {target}")
        .run()
        .context("containerd static build failed")
}
