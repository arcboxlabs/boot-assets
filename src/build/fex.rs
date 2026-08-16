use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs_err as fs;
use goblin::elf::{Elf, program_header::PT_INTERP};
use xshell::{Shell, cmd};

use arcbox_boot::manifest::Binary;

use super::vendored::{append_binaries_json, apply_patches, stage_file};

const FEX_ARCH: &str = "arm64";
const FEX_BINARIES: &[&str] = &["FEX"];

#[derive(Debug, Clone)]
pub struct BuildFexOpts {
    pub repo: String,
    pub source_ref: String,
    pub output: PathBuf,
    pub version: String,
    pub binaries_json: PathBuf,
    pub patches_dir: PathBuf,
}

pub fn build_fex(opts: &BuildFexOpts) -> Result<()> {
    let sh = Shell::new()?;
    let work = tempfile::tempdir().context("failed to create FEX build temp dir")?;
    let source = work.path().join("FEX");
    let build = work.path().join("build");

    clone_fex(&sh, &opts.repo, &opts.source_ref, &source)?;
    apply_patches(&sh, "FEX", &source, &opts.patches_dir)?;
    configure_fex(&sh, &source, &build)?;
    run_ninja(&sh, &build)?;

    let staged = stage_fex(&build, &opts.output, &opts.version)?;
    append_binaries_json(&opts.binaries_json, staged)?;

    println!("==> FEX runtime built from {}", opts.source_ref);
    println!("    Output: {}", opts.output.display());
    println!("    Manifest: {}", opts.binaries_json.display());

    Ok(())
}

fn clone_fex(sh: &Shell, repo: &str, source_ref: &str, source: &Path) -> Result<()> {
    println!("==> Cloning FEX {source_ref}");
    cmd!(
        sh,
        "git clone --depth 1 --branch {source_ref} --recurse-submodules --shallow-submodules {repo} {source}"
    )
    .run()
    .with_context(|| format!("git clone failed for FEX ref {source_ref}"))
}

fn configure_fex(sh: &Shell, source: &Path, build: &Path) -> Result<()> {
    println!("==> Configuring FEX");
    let cmake_args = [
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
    ];
    cmd!(sh, "cmake -S {source} -B {build} -G Ninja {cmake_args...}")
        .run()
        .context("FEX cmake configure failed")
}

fn run_ninja(sh: &Shell, build: &Path) -> Result<()> {
    println!("==> Building FEX");
    cmd!(sh, "ninja -C {build} {FEX_BINARIES...}")
        .run()
        .context("FEX ninja build failed")
}

fn stage_fex(build: &Path, output: &Path, version: &str) -> Result<Vec<Binary>> {
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
        entries.push(stage_file(output, version, FEX_ARCH, name, None, &src)?);
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
/// The FEX build must produce a valid ELF binary. Parse it directly so the
/// check is independent of host tooling.
fn assert_static_executable(path: &Path) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let elf = Elf::parse(&bytes)
        .with_context(|| format!("failed to parse {} as an ELF binary", path.display()))?;

    if elf
        .program_headers
        .iter()
        .any(|header| header.p_type == PT_INTERP)
    {
        bail!(
            "{} is dynamically linked (has PT_INTERP); FEX must be statically \
             linked to work as a binfmt_misc interpreter inside containers",
            path.display()
        );
    }
    Ok(())
}
