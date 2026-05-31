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
const FEX_BINARIES: &[&str] = &["FEX", "FEXServer"];

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
}

impl BuildFexRuntimeArgs {
    pub fn run(self) -> Result<()> {
        let version = self.version.unwrap_or_else(|| self.source_ref.clone());
        let work = tempfile::tempdir().context("failed to create FEX build temp dir")?;
        let source = work.path().join("FEX");
        let build = work.path().join("build");
        let install = work.path().join("install");

        clone_fex(&self.repo, &self.source_ref, &source)?;
        configure_fex(&source, &build)?;
        build_and_install_fex(&build, &install)?;

        let staged = stage_fex_runtime(&install, &self.output, &version)?;
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
            "-DCMAKE_INSTALL_PREFIX=/usr",
            "-DCMAKE_BUILD_TYPE=Release",
            "-DBUILD_TESTING=False",
            "-DBUILD_FEXCONFIG=False",
            // Target the Apple Silicon baseline (M1) for FEX's own host code.
            // FEX defaults `TUNE_CPU=native`, tuning for the build machine — the
            // SVE-capable aarch64 CI runner — so the compiler emits SVE
            // instructions (e.g. `cnth`) into FEX's binary. Apple Silicon
            // (M1–M4) implements NEON but not SVE, so such an instruction is
            // illegal and the FEX interpreter dies with SIGILL the moment it
            // runs. `apple-m1` is the deployment floor every supported Mac
            // satisfies; FEX's own `NeedDisabledSVE.py` does not cover this
            // build-host≠run-host case. `stage_fex_runtime` enforces the result
            // with `assert_no_sve_instructions`.
            "-DTUNE_CPU=apple-m1",
            // Use FEX's bundled fmt rather than the system shared library so the
            // `-static` link consumes a static archive. FEX resolves xxHash via
            // `find_library` (not `find_package`), so it cannot be bundled this
            // way; the release workflow instead installs a static `libxxhash.a`
            // and removes the shared `.so` before this build.
            "-DCMAKE_DISABLE_FIND_PACKAGE_fmt=True",
            "-DENABLE_ASSERTIONS=False",
            "-DENABLE_LTO=True",
            "-DUSE_LINKER=lld",
            // Statically link FEX so the binfmt-pinned interpreter has no
            // external loader/library dependencies. Required for execution
            // inside OCI container mount namespaces, which do not expose
            // /arcbox: a dynamic FEX would have the kernel resolve its
            // PT_INTERP against the container rootfs and fail with ENOENT.
            // `stage_fex_runtime` enforces this with `assert_static_executable`.
            "-DCMAKE_EXE_LINKER_FLAGS=-static",
        ])
        .env("CC", "clang")
        .env("CXX", "clang++")
        .status()
        .context("failed to run cmake for FEX")?;
    if !status.success() {
        bail!("FEX cmake configure failed");
    }
    Ok(())
}

fn build_and_install_fex(build: &Path, install: &Path) -> Result<()> {
    println!("==> Building FEX runtime targets");
    let status = Command::new("ninja")
        .arg("-C")
        .arg(build)
        .args(FEX_BINARIES)
        .status()
        .context("failed to run ninja for FEX")?;
    if !status.success() {
        bail!("FEX ninja build failed");
    }

    println!("==> Installing FEX into staging root");
    let status = Command::new("ninja")
        .arg("-C")
        .arg(build)
        .arg("install")
        .env("DESTDIR", install)
        .status()
        .context("failed to run ninja install for FEX")?;
    if !status.success() {
        bail!("FEX ninja install failed");
    }
    Ok(())
}

fn stage_fex_runtime(install: &Path, output: &Path, version: &str) -> Result<Vec<Binary>> {
    let install_usr_bin = install.join("usr/bin");
    let mut entries = Vec::new();

    for binary in FEX_BINARIES {
        let src = install_usr_bin.join(binary);
        if !src.is_file() {
            bail!("FEX install did not produce {}", src.display());
        }
        // FEX is statically linked (see `configure_fex`), so there is no
        // loader/library closure to stage and the binfmt-pinned interpreter
        // is self-contained inside OCI container namespaces. Fail loudly if
        // the build silently produced a dynamic binary.
        assert_static_executable(&src)?;
        // FEX runs on Apple Silicon, which has no SVE. Fail the build if any SVE
        // instruction leaked into FEX's own code despite the `TUNE_CPU` pin —
        // otherwise the interpreter SIGILLs the first time it runs (see
        // `configure_fex`).
        assert_no_sve_instructions(&src)?;
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

/// AArch64 SVE/SVE2 mnemonics whose operands are general-purpose or predicate
/// registers, so they carry no scalable `z` operand to key off of (e.g.
/// `cnth x9`). Compilers emit these when vectorising for an SVE-capable target.
const SVE_MNEMONICS: &[&str] = &[
    "cntb", "cnth", "cntw", "cntd", "rdvl", "addvl", "addpl", "ptrue", "ptrues", "pfalse", "rdffr",
    "setffr", "wrffr", "incb", "inch", "incw", "incd", "decb", "dech", "decw", "decd", "whilelo",
    "whilels", "whilelt", "whilele", "whilege", "whilegt", "whilehi", "whilehs",
];

/// Fails if `path` contains any AArch64 SVE/SVE2 instruction.
///
/// FEX must execute on Apple Silicon (M1–M4), which implements NEON but not
/// SVE. If FEX's own code is compiled for an SVE-capable CPU (see the
/// `TUNE_CPU` pin in [`configure_fex`]), the interpreter dies with `SIGILL`
/// the first time such an instruction runs. This guard disassembles the binary
/// and rejects it if any SVE mnemonic or scalable `z` register operand appears.
///
/// A missing or failing `objdump` is treated as non-fatal (the build does not
/// hard-fail on absent tooling), mirroring [`assert_static_executable`].
fn assert_no_sve_instructions(path: &Path) -> Result<()> {
    let output = match Command::new("objdump")
        .args(["-d", "--no-show-raw-insn"])
        .arg(path)
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(_) => {
            eprintln!(
                "warning: objdump failed for {}; skipping SVE check",
                path.display()
            );
            return Ok(());
        }
        Err(e) => {
            eprintln!(
                "warning: could not run objdump to verify {} is SVE-free: {e}",
                path.display()
            );
            return Ok(());
        }
    };

    let disasm = String::from_utf8_lossy(&output.stdout);
    if let Some(line) = disasm.lines().find(|line| disasm_line_uses_sve(line)) {
        bail!(
            "{} contains an SVE instruction unsupported on Apple Silicon (M1–M4): `{}`. \
             Check the TUNE_CPU pin in configure_fex.",
            path.display(),
            line.trim()
        );
    }
    Ok(())
}

/// True if one `objdump -d --no-show-raw-insn` line is an SVE instruction.
fn disasm_line_uses_sve(line: &str) -> bool {
    // Lines look like "  <addr>:\t<mnemonic> <operands>"; skip labels/blanks.
    let Some((_, code)) = line.split_once(":\t") else {
        return false;
    };
    let Some(mnemonic) = code.split_whitespace().next() else {
        return false;
    };
    if SVE_MNEMONICS.contains(&mnemonic) {
        return true;
    }
    // A scalable `z0`..`z31` vector register only appears in SVE instructions.
    // Drop any trailing `<symbol>` annotation first to avoid matching names.
    let operands = code.split('<').next().unwrap_or(code);
    operands
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(is_sve_z_register)
}

/// True if `token` names a scalable vector register `z0`..`z31`.
fn is_sve_z_register(token: &str) -> bool {
    token
        .strip_prefix('z')
        .and_then(|num| num.parse::<u8>().ok())
        .is_some_and(|n| n <= 31)
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

#[cfg(test)]
mod tests {
    use super::disasm_line_uses_sve;

    #[test]
    fn detects_sve_count_mnemonic() {
        // The exact instruction the Apple Silicon SIGILL was traced to.
        assert!(disasm_line_uses_sve("  5e5438:\tcnth\tx9"));
        assert!(disasm_line_uses_sve("  400570:\trdvl\tx0, #1"));
    }

    #[test]
    fn detects_scalable_z_register_operand() {
        assert!(disasm_line_uses_sve("  400580:\tld1w\t{z0.s}, p0/z, [x0]"));
        assert!(disasm_line_uses_sve("  4005a0:\tfadd\tz1.s, z1.s, z2.s"));
    }

    #[test]
    fn ignores_neon_and_scalar() {
        assert!(!disasm_line_uses_sve("  400560:\tadd\tv0.4s, v1.4s, v2.4s"));
        assert!(!disasm_line_uses_sve("  400564:\tmov\tx0, x1"));
        assert!(!disasm_line_uses_sve("  400568:\tldr\tq0, [sp, #16]"));
    }

    #[test]
    fn ignores_symbol_names_containing_z() {
        // A branch to a symbol whose name looks like a `z` register must not
        // be mistaken for an SVE operand.
        assert!(!disasm_line_uses_sve("  40058c:\tbl\t400abc <z16_unused>"));
        assert!(!disasm_line_uses_sve(
            "  400590:\tbl\t400def <zlib_inflate>"
        ));
    }

    #[test]
    fn ignores_labels_and_blanks() {
        assert!(!disasm_line_uses_sve("0000000000400550 <_start>:"));
        assert!(!disasm_line_uses_sve(""));
    }
}
