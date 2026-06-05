use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::{Digest, Sha256};

use arcbox_boot::manifest::{Binary, BinaryTarget};

const DEFAULT_FEX_REPO: &str = "https://github.com/FEX-Emu/FEX.git";
const DEFAULT_FEX_REF: &str = "FEX-2605";
const FEX_ARCH: &str = "arm64";
const FEX_BINARIES: &[&str] = &["FEX"];
/// Directory (relative to CWD) of vendored `*.patch` files applied to the FEX
/// source after clone.
const DEFAULT_PATCHES_DIR: &str = "patches/fex";

#[derive(Args)]
pub struct BuildFexRuntimeArgs {
    /// FEX git repository URL.
    #[arg(long, default_value = DEFAULT_FEX_REPO)]
    repo: String,
    /// FEX git ref/tag to build.
    #[arg(long, default_value = DEFAULT_FEX_REF)]
    source_ref: String,
    /// Output directory. Files are written to {output}/{name}/{version}/arm64/{name}.
    #[arg(long, default_value = "dist/bin")]
    output: PathBuf,
    /// Runtime version used in the ArcBox binary manifest path.
    #[arg(long)]
    version: Option<String>,
    /// Append FEX entries to this JSON manifest fragment.
    #[arg(long)]
    binaries_json: PathBuf,
    /// Directory of `*.patch` files applied to the FEX source after clone.
    #[arg(long, default_value = DEFAULT_PATCHES_DIR)]
    patches_dir: PathBuf,
}

impl BuildFexRuntimeArgs {
    pub fn run(self) -> Result<()> {
        let version = self.version.unwrap_or_else(|| self.source_ref.clone());
        let work = tempfile::tempdir().context("failed to create FEX build temp dir")?;
        let source = work.path().join("FEX");
        let build = work.path().join("build");

        clone_fex(&self.repo, &self.source_ref, &source)?;
        apply_patches(&source, &self.patches_dir)?;
        configure_fex(&source, &build)?;
        build_fex(&build)?;

        let staged = stage_fex_runtime(&build, &self.output, &version)?;
        append_binaries_json(&self.binaries_json, staged)?;

        println!("==> FEX runtime built from {}", self.source_ref);
        println!("    Output: {}", self.output.display());
        println!("    Manifest: {}", self.binaries_json.display());

        Ok(())
    }
}

fn clone_fex(repo: &str, source_ref: &str, source: &Path) -> Result<()> {
    println!("==> Cloning FEX {source_ref}");
    let status = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--branch",
            source_ref,
            "--recurse-submodules",
            "--shallow-submodules",
            repo,
        ])
        .arg(source)
        .status()
        .context("failed to run git clone for FEX")?;
    if !status.success() {
        bail!("git clone failed for FEX ref {source_ref}");
    }
    Ok(())
}

/// Applies every `*.patch` in `patches_dir` to the cloned FEX `source`, in
/// sorted filename order. These are vendored source changes not upstream (e.g.
/// dropping the FEXServer dependency).
fn apply_patches(source: &Path, patches_dir: &Path) -> Result<()> {
    if !patches_dir.is_dir() {
        bail!("FEX patches dir not found: {}", patches_dir.display());
    }
    let mut patches: Vec<PathBuf> = std::fs::read_dir(patches_dir)
        .with_context(|| format!("failed to read patches dir {}", patches_dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("patch"))
        .collect();
    patches.sort();
    if patches.is_empty() {
        bail!("no .patch files in {}", patches_dir.display());
    }

    for patch in &patches {
        // `git -C` changes directory, so the patch path must be absolute.
        let abs = std::fs::canonicalize(patch)
            .with_context(|| format!("failed to resolve patch {}", patch.display()))?;
        println!("==> Applying patch {}", patch.display());
        let status = Command::new("git")
            .arg("-C")
            .arg(source)
            .args(["apply", "--verbose"])
            .arg(&abs)
            .status()
            .with_context(|| format!("failed to run git apply for {}", abs.display()))?;
        if !status.success() {
            bail!("git apply failed for {}", patch.display());
        }
    }
    Ok(())
}

fn configure_fex(source: &Path, build: &Path) -> Result<()> {
    println!("==> Configuring FEX");
    let status = Command::new("cmake")
        .args([
            "-S",
            path_str(source)?,
            "-B",
            path_str(build)?,
            "-G",
            "Ninja",
            "-DCMAKE_BUILD_TYPE=Release",
            "-DCMAKE_C_COMPILER=clang",
            "-DCMAKE_CXX_COMPILER=clang++",
            "-DBUILD_FEXCONFIG=False",
            "-DENABLE_CCACHE=False",
            // No-SVE baseline: stops the compiler auto-vectorising FEX's own code
            // with SVE, which is ungated and SIGILLs on Apple Silicon.
            "-DTUNE_CPU=apple-m1",
            // static-pie: self-contained binfmt interpreter for container
            // namespaces, no loader/library closure (lld reads ThinLTO bitcode,
            // GNU ld can't).
            "-DUSE_LINKER=lld",
            "-DCMAKE_EXE_LINKER_FLAGS=-static-pie",
        ])
        .status()
        .context("failed to run cmake for FEX")?;
    if !status.success() {
        bail!("FEX cmake configure failed");
    }
    Ok(())
}

fn build_fex(build: &Path) -> Result<()> {
    println!("==> Building FEX");
    let status = Command::new("ninja")
        .arg("-C")
        .arg(build)
        .args(FEX_BINARIES)
        .status()
        .context("failed to run ninja for FEX")?;
    if !status.success() {
        bail!("FEX ninja build failed");
    }
    Ok(())
}

fn stage_fex_runtime(build: &Path, output: &Path, version: &str) -> Result<Vec<Binary>> {
    let bin_dir = build.join("Bin");
    let mut entries = Vec::new();

    for binary in FEX_BINARIES {
        let src = bin_dir.join(binary);
        if !src.is_file() {
            bail!("FEX build did not produce {}", src.display());
        }
        // FEX is statically linked (see `configure_fex`), so there is no
        // loader/library closure to stage and the binfmt-pinned interpreter
        // is self-contained inside OCI container namespaces. Fail loudly if
        // the build silently produced a dynamic binary.
        assert_static_executable(&src)?;
        let name = src
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid FEX binary filename: {}", src.display()))?;
        entries.push(stage_file(output, version, name, None, &src)?);
    }

    Ok(entries)
}

/// Fails if `path` is a dynamically-linked ELF (carries a `PT_INTERP`).
///
/// A dynamic FEX cannot serve as a `binfmt_misc` interpreter inside an OCI
/// container: the kernel resolves the interpreter's `PT_INTERP` against the
/// container's mount namespace (the amd64 image rootfs), which does not
/// contain FEX's loader, so exec fails with `ENOENT`. The static link in
/// [`configure_fex`] removes that dependency; this guard ensures it actually
/// took effect.
///
/// A missing or failing `readelf` is treated as non-fatal (the build does not
/// hard-fail on absent tooling), but a confirmed dynamic binary is an error.
fn assert_static_executable(path: &Path) -> Result<()> {
    let output = match Command::new("readelf").arg("-l").arg(path).output() {
        Ok(output) if output.status.success() => output,
        Ok(_) => {
            eprintln!(
                "warning: readelf failed for {}; skipping static-link check",
                path.display()
            );
            return Ok(());
        }
        Err(e) => {
            eprintln!(
                "warning: could not run readelf to verify {} is static: {e}",
                path.display()
            );
            return Ok(());
        }
    };

    if String::from_utf8_lossy(&output.stdout).contains("INTERP") {
        bail!(
            "{} is dynamically linked (has PT_INTERP); FEX must be statically \
             linked to work as a binfmt_misc interpreter inside containers",
            path.display()
        );
    }
    Ok(())
}

fn stage_file(
    output: &Path,
    version: &str,
    name: &str,
    install_dir: Option<&str>,
    src: &Path,
) -> Result<Binary> {
    let dest = output.join(name).join(version).join(FEX_ARCH).join(name);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, &dest)
        .with_context(|| format!("failed to copy {} to {}", src.display(), dest.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms)?;
    }

    let mut targets = BTreeMap::new();
    targets.insert(
        FEX_ARCH.to_string(),
        BinaryTarget {
            path: format!("bin/{name}/{version}/{FEX_ARCH}/{name}"),
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

fn append_binaries_json(path: &Path, mut entries: Vec<Binary>) -> Result<()> {
    let mut existing = if path.exists() {
        let bytes =
            std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_slice::<Vec<Binary>>(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?
    } else {
        Vec::new()
    };

    existing.append(&mut entries);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&existing)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {}", path.display()))
}
