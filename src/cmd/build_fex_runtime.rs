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
const FEX_LIB_DIR: &str = "fex/lib";
const FEX_RPATH: &str = "$ORIGIN/../fex/lib";
const FEX_INTERPRETER: &str = "/arcbox/fex/lib/ld-linux-aarch64.so.1";

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
            "-DENABLE_ASSERTIONS=False",
            "-DENABLE_LTO=True",
            "-DUSE_LINKER=lld",
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
    let mut closure_inputs = Vec::new();

    for binary in FEX_BINARIES {
        let src = install_usr_bin.join(binary);
        if !src.is_file() {
            bail!("FEX install did not produce {}", src.display());
        }
        closure_inputs.push(src);
    }

    let libs = collect_runtime_libraries(&closure_inputs)?;

    for src in &closure_inputs {
        patch_fex_executable(src)?;
        let name = src
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid FEX binary filename: {}", src.display()))?;
        entries.push(stage_file(output, version, name, None, src)?);
    }

    for lib in libs {
        let name = lib
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid library filename: {}", lib.display()))?;
        entries.push(stage_file(output, version, name, Some(FEX_LIB_DIR), &lib)?);
    }

    Ok(entries)
}

fn patch_fex_executable(path: &Path) -> Result<()> {
    let status = Command::new("patchelf")
        .args([
            "--set-interpreter",
            FEX_INTERPRETER,
            "--set-rpath",
            FEX_RPATH,
        ])
        .arg(path)
        .status()
        .with_context(|| format!("failed to run patchelf for {}", path.display()))?;
    if !status.success() {
        bail!("patchelf failed for {}", path.display());
    }
    Ok(())
}

fn collect_runtime_libraries(executables: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut libs = BTreeMap::new();
    for executable in executables {
        let output = Command::new("ldd")
            .arg(executable)
            .output()
            .with_context(|| format!("failed to run ldd for {}", executable.display()))?;
        if !output.status.success() {
            bail!("ldd failed for {}", executable.display());
        }

        let stdout = String::from_utf8(output.stdout).context("ldd output was not UTF-8")?;
        for line in stdout.lines() {
            if let Some(path) = parse_ldd_library(line) {
                libs.insert(path.clone(), path);
            }
        }
    }
    Ok(libs.into_values().collect())
}

fn parse_ldd_library(line: &str) -> Option<PathBuf> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("linux-vdso") {
        return None;
    }

    if let Some((_, rest)) = trimmed.split_once("=>") {
        let path = rest.split_whitespace().next()?;
        return path.starts_with('/').then(|| PathBuf::from(path));
    }

    let path = trimmed.split_whitespace().next()?;
    path.starts_with('/').then(|| PathBuf::from(path))
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
    use super::parse_ldd_library;

    #[test]
    fn parses_ldd_arrow_paths() {
        assert_eq!(
            parse_ldd_library("libc.so.6 => /lib/aarch64-linux-gnu/libc.so.6 (0x123)")
                .unwrap()
                .to_str(),
            Some("/lib/aarch64-linux-gnu/libc.so.6")
        );
    }

    #[test]
    fn parses_ldd_loader_paths() {
        assert_eq!(
            parse_ldd_library("/lib/ld-linux-aarch64.so.1 (0x123)")
                .unwrap()
                .to_str(),
            Some("/lib/ld-linux-aarch64.so.1")
        );
    }

    #[test]
    fn ignores_vdso() {
        assert!(parse_ldd_library("linux-vdso.so.1 (0x0000)").is_none());
    }
}
