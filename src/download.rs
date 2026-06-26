use std::path::Path;

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};
use crate::manifest::{Binary, BinaryTarget, Manifest};
use crate::util::{cdn_url, current_arch, set_executable_async, sha256_file_async};

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
    /// Binaries without `install_dir` are placed at `dest_dir/{name}`.
    /// Binaries with `install_dir` are placed as siblings of `dest_dir`
    /// (e.g. dest_dir=/arcbox/bin, install_dir="kernel" → /arcbox/kernel/{name}).
    ///
    /// For each binary in the manifest:
    /// 1. If the file already exists and its SHA256 matches, skip it.
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
            let Some(bt) = binary.targets.get(arch) else {
                continue;
            };
            let dest_path = if let Some(ref sub) = binary.install_dir {
                // Resolve install_dir as a sibling of dest_dir.
                // e.g. dest_dir=/arcbox/bin/, install_dir="kernel" → /arcbox/kernel/vmlinux
                let base = dest_dir.parent().unwrap_or(dest_dir);
                let sub_dir = base.join(sub);
                tokio::fs::create_dir_all(&sub_dir).await?;
                sub_dir.join(&binary.name)
            } else {
                dest_dir.join(&binary.name)
            };

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
                && let Ok(actual) = sha256_file_async(&dest_path).await
                && actual == bt.sha256
            {
                pg(PreparePhase::Cached);
                continue;
            }

            // All manifest paths are relative to cdn_base_url.
            let url = cdn_url(cdn_base_url, &bt.path)?;

            download_and_verify(&url, &dest_path, &bt.sha256, &binary.name, |dl, tot| {
                pg(PreparePhase::Downloading {
                    downloaded: dl,
                    total: tot,
                });
            })
            .await?;

            set_executable_async(&dest_path).await?;

            pg(PreparePhase::Ready);
        }

        Ok(())
    }

    /// Validate that all required binaries for `arch` exist with correct
    /// checksums. Uses the same path resolution as [`prepare_binaries`].
    pub async fn validate_binaries(&self, arch: &str, dest_dir: &Path) -> Result<()> {
        for binary in &self.binaries {
            let Some(bt) = binary.targets.get(arch) else {
                continue;
            };
            let path = if let Some(ref sub) = binary.install_dir {
                let base = dest_dir.parent().unwrap_or(dest_dir);
                base.join(sub).join(&binary.name)
            } else {
                dest_dir.join(&binary.name)
            };

            if !path.exists() {
                return Err(Error::Other(format!(
                    "binary '{}' not found at {}",
                    binary.name,
                    path.display()
                )));
            }

            let actual = sha256_file_async(&path).await?;
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
    use sha2::{Digest, Sha256};
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::manifest::{Binary, BinaryTarget, Manifest};

    #[tokio::test]
    async fn validate_binaries_skips_binaries_without_target_arch() {
        let manifest = Manifest {
            schema_version: 1,
            asset_version: "1.0.0".to_string(),
            built_at: "2026-01-01T00:00:00Z".to_string(),
            source_repo: None,
            source_ref: None,
            source_sha: None,
            targets: BTreeMap::new(),
            binaries: vec![Binary {
                name: "FEX".to_string(),
                version: "FEX-2605".to_string(),
                targets: BTreeMap::from([(
                    "arm64".to_string(),
                    BinaryTarget {
                        path: "bin/FEX/FEX-2605/arm64/FEX".to_string(),
                        sha256: "unused".to_string(),
                    },
                )]),
                install_dir: Some("runtime/bin".to_string()),
            }],
        };

        let temp = tempfile::tempdir().unwrap();
        manifest
            .validate_binaries("x86_64", temp.path())
            .await
            .unwrap();
    }
}
