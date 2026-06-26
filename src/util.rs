#[cfg(feature = "build")]
use anyhow::{Context, Result as AnyhowResult};
#[cfg(any(feature = "download", feature = "build"))]
use camino::Utf8PathBuf;
#[cfg(feature = "build")]
use fs_err as fs;
#[cfg(feature = "build")]
use minijinja::Environment;
#[cfg(feature = "build")]
use serde::Serialize;
#[cfg(any(feature = "download", feature = "build"))]
use serde::de::DeserializeOwned;
#[cfg(any(feature = "download", feature = "build"))]
use std::path::Path;
#[cfg(any(feature = "download", feature = "build"))]
use url::Url;

#[cfg(feature = "download")]
use crate::error::{Error, Result};

/// Returns the architecture string used by ArcBox manifests for the current host.
pub fn current_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    }
}

/// Sanitizes a version string for use in object keys and local paths.
///
/// SemVer build metadata uses `+`, which some CDNs interpret as a space when it
/// appears unescaped in a URL. ArcBox object paths use `-` instead.
pub fn path_safe_version(version: &str) -> String {
    version.replace('+', "-")
}

/// Builds a CDN/object path for a runtime binary.
#[cfg(any(feature = "download", feature = "build"))]
pub fn binary_object_path(name: &str, version: &str, arch: &str) -> String {
    Utf8PathBuf::from("bin")
        .join(name)
        .join(version)
        .join(arch)
        .join(name)
        .into_string()
}

/// Builds a CDN/object path for a boot asset file.
#[cfg(any(feature = "download", feature = "build"))]
pub fn asset_object_path(version: &str, arch: &str, filename: &str) -> String {
    Utf8PathBuf::from("asset")
        .join(format!("v{version}"))
        .join(arch)
        .join(filename)
        .into_string()
}

/// Builds the CDN/object path for a versioned manifest.
#[cfg(any(feature = "download", feature = "build"))]
pub fn manifest_object_path(version: &str) -> String {
    Utf8PathBuf::from("asset")
        .join(format!("v{version}"))
        .join("manifest.json")
        .into_string()
}

/// Joins a relative object path onto the configured CDN base URL.
#[cfg(feature = "download")]
pub fn cdn_url(base: &str, path: &str) -> Result<String> {
    let mut base = base.to_string();
    if !base.ends_with('/') {
        base.push('/');
    }
    Url::parse(&base)
        .map_err(|e| Error::InvalidConfig(format!("invalid CDN base URL '{base}': {e}")))?
        .join(path)
        .map(|url| url.to_string())
        .map_err(|e| Error::InvalidConfig(format!("invalid CDN path '{path}': {e}")))
}

/// Computes the SHA256 digest of a file using synchronous I/O.
#[cfg(feature = "build")]
pub fn sha256_file(path: &Path) -> AnyhowResult<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file =
        fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(hasher.finalize()))
}

/// Reads and parses a JSON file with path context on failures.
#[cfg(feature = "build")]
pub fn read_json_file<T: DeserializeOwned>(path: &Path) -> AnyhowResult<T> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

/// Serializes a value as pretty JSON and writes it to disk.
#[cfg(feature = "build")]
pub fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> AnyhowResult<()> {
    let json = serde_json::to_string_pretty(value)?;
    fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))
}

/// Creates a gzip-compressed tar archive containing files from `work_dir`.
#[cfg(feature = "build")]
pub fn create_tar_gz(output: &Path, work_dir: &Path, files: &[&str]) -> AnyhowResult<()> {
    let file = fs::File::create(output)?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut archive = tar::Builder::new(gz);
    for name in files {
        let file_path = work_dir.join(name);
        archive
            .append_path_with_name(&file_path, name)
            .with_context(|| format!("failed to add {name} to tarball"))?;
    }
    archive.finish()?;
    Ok(())
}

/// Computes the SHA256 digest of a file using async I/O.
#[cfg(feature = "download")]
pub async fn sha256_file_async(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(hasher.finalize()))
}

#[cfg(any(feature = "download", feature = "build"))]
pub(crate) fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

/// Reads and parses a JSON file using async I/O.
#[cfg(feature = "download")]
pub async fn read_json_file_async<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = tokio::fs::read(path).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::Other(format!("failed to parse {}: {e}", path.display())))
}

/// Copies a file and marks the destination executable on Unix.
#[cfg(feature = "build")]
pub fn copy_executable(src: &Path, dst: &Path) -> AnyhowResult<()> {
    fs::copy(src, dst)?;
    set_executable(dst)?;
    Ok(())
}

/// Marks a file executable on Unix.
#[cfg(feature = "build")]
pub fn set_executable(path: &Path) -> AnyhowResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Marks a file executable on Unix using async I/O.
#[cfg(feature = "download")]
pub async fn set_executable_async(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(path).await?.permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(path, perms).await?;
    }
    Ok(())
}

/// Renders an embedded MiniJinja template with a named source for better errors.
#[cfg(feature = "build")]
pub fn render_template<S: serde::Serialize>(
    name: &'static str,
    source: &'static str,
    context: S,
) -> AnyhowResult<String> {
    Environment::new()
        .render_named_str(name, source, context)
        .with_context(|| format!("failed to render {name}"))
}
