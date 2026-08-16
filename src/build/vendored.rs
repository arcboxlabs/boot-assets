//! Shared steps for components we build from patched upstream source.
//!
//! Both FEX and containerd follow the same shape: clone a pinned upstream ref,
//! apply the `*.patch` files vendored under `patches/<component>/`, build, then
//! stage the result into the CDN layout and describe it for the release
//! manifest. Only the build step itself differs, so everything around it lives
//! here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs_err as fs;
use goblin::elf::{Elf, program_header::PT_INTERP};
use xshell::{Shell, cmd};

use arcbox_boot::manifest::{Binary, BinaryTarget};
use arcbox_boot::util::{
    binary_object_path, read_json_file, set_executable, sha256_file, write_json_pretty,
};

/// Applies every `*.patch` in `patches_dir` to `source`, in sorted filename
/// order.
///
/// These are vendored source changes not (yet) in the upstream release we
/// build from — dropping FEX's FEXServer dependency, or carrying a containerd
/// fix that landed on `main` after the release tag was cut. A missing or empty
/// patch directory is an error rather than a no-op: it almost always means a
/// path was mistyped, and silently building stock source would ship a binary
/// that looks right and behaves like the version we were trying to get away
/// from.
pub fn apply_patches(sh: &Shell, component: &str, source: &Path, patches_dir: &Path) -> Result<()> {
    if !patches_dir.is_dir() {
        bail!(
            "{component} patches dir not found: {}",
            patches_dir.display()
        );
    }
    let mut patches: Vec<PathBuf> = fs::read_dir(patches_dir)
        .with_context(|| format!("failed to read patches dir {}", patches_dir.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read an entry in {}", patches_dir.display()))?
        .into_iter()
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("patch"))
        .collect();
    patches.sort();
    if patches.is_empty() {
        bail!("no .patch files in {}", patches_dir.display());
    }

    for patch in &patches {
        // `git -C` changes directory, so the patch path must be absolute.
        let abs = fs::canonicalize(patch)
            .with_context(|| format!("failed to resolve patch {}", patch.display()))?;
        println!("==> Applying patch {}", patch.display());
        cmd!(sh, "git -C {source} apply --verbose {abs}")
            .run()
            .with_context(|| format!("git apply failed for {}", patch.display()))?;
    }
    Ok(())
}

/// Fails if `path` is a dynamically-linked ELF (carries a `PT_INTERP`).
///
/// Every binary built here has to be self-contained, and each for its own
/// reason:
///
/// - **FEX** is pinned as a `binfmt_misc` interpreter. The kernel resolves its
///   `PT_INTERP` against the *container's* mount namespace — an amd64 image
///   rootfs that has never heard of FEX's loader — so a dynamic build execs
///   with `ENOENT`.
/// - **containerd** runs in a guest whose rootfs is EROFS built from Alpine
///   static binaries, with no glibc loader at all. A cgo build that only
///   *partially* statically links still compiles, links, and passes CI, then
///   fails to exec at guest boot — about as far from the cause as a failure
///   can land.
///
/// Both builds ask for a static link; this checks the request took effect.
/// The ELF is parsed directly so the guard does not depend on host tooling.
pub fn assert_static_executable(component: &str, path: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let elf = Elf::parse(&bytes)
        .with_context(|| format!("failed to parse {} as an ELF binary", path.display()))?;

    if elf
        .program_headers
        .iter()
        .any(|header| header.p_type == PT_INTERP)
    {
        bail!(
            "{} is dynamically linked (has PT_INTERP); {component} must be \
             statically linked to run in the guest",
            path.display()
        );
    }
    Ok(())
}

/// Copies `src` into the CDN layout at `{output}/{name}/{version}/{arch}/{name}`
/// and returns its manifest entry.
///
/// `version` must be unique per release for anything built from source: these
/// builds are not bit-reproducible, the B2 sync runs `--size-only`, and
/// Cloudflare caches objects for hours — so reusing a key for different bytes
/// leaves the CDN serving a binary that no longer matches the sha256 pinned in
/// the manifest.
pub fn stage_file(
    output: &Path,
    version: &str,
    arch: &str,
    name: &str,
    install_dir: Option<&str>,
    src: &Path,
) -> Result<Binary> {
    let dest = output.join(name).join(version).join(arch).join(name);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, &dest)
        .with_context(|| format!("failed to copy {} to {}", src.display(), dest.display()))?;

    set_executable(&dest)?;

    let mut targets = BTreeMap::new();
    targets.insert(
        arch.to_string(),
        BinaryTarget {
            path: binary_object_path(name, version, arch),
            sha256: sha256_file(&dest)?,
        },
    );

    Ok(Binary {
        name: name.to_string(),
        version: version.to_string(),
        targets,
        install_dir: install_dir.map(str::to_string),
    })
}

/// Appends `entries` to the JSON manifest fragment at `path`, creating it if
/// absent.
pub fn append_binaries_json(path: &Path, mut entries: Vec<Binary>) -> Result<()> {
    let mut existing = if path.exists() {
        read_json_file::<Vec<Binary>>(path)?
    } else {
        Vec::new()
    };

    existing.append(&mut entries);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_json_pretty(path, &existing)
}
