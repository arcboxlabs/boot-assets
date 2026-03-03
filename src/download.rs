use std::path::Path;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};
use crate::manifest::{Binary, BinaryTarget, Manifest};

/// Progress information for binary preparation.
#[derive(Debug, Clone)]
pub struct PrepareProgress {
    /// Name of the binary currently being processed.
    pub name: String,
    /// 1-based index of the current binary.
    pub current: usize,
    /// Total number of binaries.
    pub total: usize,
    /// Phase description.
    pub phase: PreparePhase,
}

#[derive(Debug, Clone)]
pub enum PreparePhase {
    /// Checking if the binary is already cached and valid.
    Checking,
    /// Downloading the binary.
    Downloading { downloaded: u64, total: Option<u64> },
    /// Verifying checksum after download.
    Verifying,
    /// Binary is ready (was cached or freshly downloaded).
    Ready,
    /// Binary was skipped (already cached and checksum matches).
    Cached,
}

/// Progress callback type.
pub type ProgressCallback = Box<dyn Fn(PrepareProgress) + Send + Sync>;

const HTTP_TIMEOUT_SECS: u64 = 300;
const USER_AGENT: &str = "arcbox-boot-assets/0.1";

impl Manifest {
    /// Returns the target entry for the current host architecture, if present.
    pub fn target_for_current_arch(&self) -> Option<(&str, &crate::manifest::Target)> {
        let arch = current_arch();
        self.targets.get(arch).map(|t| (arch, t))
    }

    /// Prepare all binaries for the given architecture into `dest_dir`.
    ///
    /// For each binary in the manifest:
    /// 1. If the file already exists at `dest_dir/{name}` and its SHA256
    ///    matches, skip it.
    /// 2. Otherwise, download from `{cdn_base_url}/{path}` and verify
    ///    the checksum.
    ///
    /// All manifest paths are relative to `cdn_base_url`.
    /// Files are written atomically (temp + rename).
    pub async fn prepare_binaries(
        &self,
        arch: &str,
        cdn_base_url: &str,
        dest_dir: &Path,
        progress: Option<ProgressCallback>,
    ) -> Result<()> {
        tokio::fs::create_dir_all(dest_dir).await?;

        let total = self.binaries.len();
        for (idx, binary) in self.binaries.iter().enumerate() {
            let bt = binary.target_for_arch(arch)?;
            let dest_path = dest_dir.join(&binary.name);

            let pg = |phase: PreparePhase| {
                if let Some(ref cb) = progress {
                    cb(PrepareProgress {
                        name: binary.name.clone(),
                        current: idx + 1,
                        total,
                        phase,
                    });
                }
            };

            pg(PreparePhase::Checking);

            // Check cache: if file exists and checksum matches, skip download.
            if dest_path.exists()
                && let Ok(actual) = sha256_file(&dest_path).await
                && actual == bt.sha256
            {
                pg(PreparePhase::Cached);
                continue;
            }

            // All manifest paths are relative to cdn_base_url.
            let url = format!("{}/{}", cdn_base_url.trim_end_matches('/'), bt.path);

            download_and_verify(&url, &dest_path, &bt.sha256, &binary.name, |dl, tot| {
                pg(PreparePhase::Downloading {
                    downloaded: dl,
                    total: tot,
                });
            })
            .await?;

            // Mark executable on Unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = tokio::fs::metadata(&dest_path).await?.permissions();
                perms.set_mode(0o755);
                tokio::fs::set_permissions(&dest_path, perms).await?;
            }

            pg(PreparePhase::Ready);
        }

        Ok(())
    }

    /// Validate that all required binaries for `arch` exist in `dest_dir`
    /// with correct checksums.
    pub async fn validate_binaries(&self, arch: &str, dest_dir: &Path) -> Result<()> {
        for binary in &self.binaries {
            let bt = binary.target_for_arch(arch)?;
            let path = dest_dir.join(&binary.name);

            if !path.exists() {
                return Err(Error::Other(format!(
                    "binary '{}' not found at {}",
                    binary.name,
                    path.display()
                )));
            }

            let actual = sha256_file(&path).await?;
            if actual != bt.sha256 {
                return Err(Error::ChecksumMismatch {
                    name: binary.name.clone(),
                    expected: bt.sha256.clone(),
                    actual,
                });
            }
        }
        Ok(())
    }
}

impl Binary {
    /// Returns the target entry for the given architecture.
    pub fn target_for_arch(&self, arch: &str) -> Result<&BinaryTarget> {
        self.targets
            .get(arch)
            .ok_or_else(|| Error::BinaryArchNotFound {
                name: self.name.clone(),
                arch: arch.to_string(),
            })
    }
}

/// Returns the architecture string for the current host.
pub(crate) fn current_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    }
}

pub(crate) async fn sha256_file(path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path).await?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

pub(crate) async fn download_and_verify(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    name: &str,
    on_progress: impl Fn(u64, Option<u64>),
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::Download(format!("failed to create HTTP client: {e}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Download(format!("request failed for {url}: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::Download(format!(
            "HTTP {} for {url}",
            response.status()
        )));
    }

    let total = response.content_length();
    let mut downloaded: u64 = 0;

    // Write to temp file, then rename atomically.
    let temp_path = dest.with_extension("tmp");
    let mut file = tokio::fs::File::create(&temp_path).await?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Download(format!("stream error: {e}")))?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        on_progress(downloaded, total);
    }

    file.flush().await?;
    drop(file);

    let actual_sha = format!("{:x}", hasher.finalize());
    if actual_sha != expected_sha256 {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(Error::ChecksumMismatch {
            name: name.to_string(),
            expected: expected_sha256.to_string(),
            actual: actual_sha,
        });
    }

    tokio::fs::rename(&temp_path, dest).await?;
    Ok(())
}
