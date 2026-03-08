use std::path::{Path, PathBuf};

use crate::download::{
    PreparePhase, PrepareProgress, ProgressCallback, current_arch, download_and_verify, sha256_file,
};
use crate::error::{Error, Result};
use crate::manifest::{Manifest, schema_version_for};

const DEFAULT_CDN_BASE_URL: &str = "https://boot.arcboxcdn.com";

/// Configuration for the asset manager.
#[derive(Debug, Clone)]
pub struct AssetManagerConfig {
    /// CDN base URL (default: https://boot.arcboxcdn.com).
    pub cdn_base_url: String,
    /// Boot asset version (e.g. "0.2.0").
    pub version: String,
    /// Target architecture ("arm64" or "x86_64"). Auto-detected if empty.
    pub arch: String,
    /// Local cache directory (e.g. ~/.arcbox/boot).
    pub cache_dir: PathBuf,
    /// Override kernel path (skip download).
    pub custom_kernel: Option<PathBuf>,
}

impl Default for AssetManagerConfig {
    fn default() -> Self {
        let cache_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".arcbox")
            .join("boot");

        Self {
            cdn_base_url: DEFAULT_CDN_BASE_URL.to_string(),
            version: String::new(),
            arch: current_arch().to_string(),
            cache_dir,
            custom_kernel: None,
        }
    }
}

/// Resolved boot assets ready for VM startup.
#[derive(Debug, Clone)]
pub struct PreparedAssets {
    /// Path to the kernel image.
    pub kernel: PathBuf,
    /// Path to the EROFS rootfs image.
    pub rootfs: PathBuf,
    /// Kernel command line from manifest.
    pub kernel_cmdline: String,
    /// Boot asset version.
    pub version: String,
    /// Parsed manifest.
    pub manifest: Manifest,
}

/// Manages downloading, caching, and verifying boot assets.
pub struct AssetManager {
    config: AssetManagerConfig,
}

impl AssetManager {
    pub fn new(mut config: AssetManagerConfig) -> Result<Self> {
        if config.version.is_empty() {
            return Err(Error::InvalidConfig("version must not be empty".into()));
        }
        if config.arch.is_empty() {
            config.arch = current_arch().to_string();
        }
        Ok(Self { config })
    }

    /// Access the configuration.
    pub fn config(&self) -> &AssetManagerConfig {
        &self.config
    }

    /// Download manifest + kernel + rootfs if not cached.
    ///
    /// Does NOT download binaries — use [`prepare_binaries`] for that.
    pub async fn prepare(&self, progress: Option<ProgressCallback>) -> Result<PreparedAssets> {
        let version_dir = self.config.cache_dir.join(&self.config.version);
        tokio::fs::create_dir_all(&version_dir).await?;

        // Step 1: Fetch and validate manifest.
        let manifest = self.fetch_manifest(&version_dir).await?;

        let expected_schema = schema_version_for(&self.config.version);
        if manifest.schema_version != expected_schema {
            return Err(Error::UnsupportedSchema {
                version: manifest.schema_version,
                expected: expected_schema,
            });
        }

        let target = manifest
            .targets
            .get(&self.config.arch)
            .ok_or_else(|| Error::ArchNotFound(self.config.arch.clone()))?;

        // Step 2: Resolve kernel.
        let kernel_path = if let Some(ref custom) = self.config.custom_kernel {
            if !custom.is_file() {
                return Err(Error::Other(format!(
                    "custom kernel is not a regular file: {}",
                    custom.display()
                )));
            }
            custom.clone()
        } else {
            let dest = version_dir.join("kernel");
            self.ensure_file(
                &target.kernel.path,
                &target.kernel.sha256,
                &dest,
                "kernel",
                &progress,
            )
            .await?;
            dest
        };

        // Step 3: Download rootfs.
        let rootfs_path = version_dir.join("rootfs.erofs");
        self.ensure_file(
            &target.rootfs.path,
            &target.rootfs.sha256,
            &rootfs_path,
            "rootfs",
            &progress,
        )
        .await?;

        Ok(PreparedAssets {
            kernel: kernel_path,
            rootfs: rootfs_path,
            kernel_cmdline: target.kernel_cmdline.clone(),
            version: self.config.version.clone(),
            manifest,
        })
    }

    /// Prepare host-side binaries (dockerd, containerd, shim, runc) into `dest_dir`.
    ///
    /// Separate from [`prepare`] — call this in the runtime init phase.
    pub async fn prepare_binaries(
        &self,
        dest_dir: &Path,
        progress: Option<ProgressCallback>,
    ) -> Result<()> {
        let version_dir = self.config.cache_dir.join(&self.config.version);
        tokio::fs::create_dir_all(&version_dir).await?;

        let manifest = self.fetch_manifest(&version_dir).await?;
        let expected_schema = schema_version_for(&self.config.version);
        if manifest.schema_version != expected_schema {
            return Err(Error::UnsupportedSchema {
                version: manifest.schema_version,
                expected: expected_schema,
            });
        }

        manifest
            .prepare_binaries(
                &self.config.arch,
                &self.config.cdn_base_url,
                dest_dir,
                progress,
            )
            .await
    }

    /// Fetch manifest from CDN or load from cache.
    async fn fetch_manifest(&self, version_dir: &Path) -> Result<Manifest> {
        let manifest_path = version_dir.join("manifest.json");

        // If cached, load and return.
        if manifest_path.exists() {
            return self.load_cached_manifest(version_dir).await;
        }

        // Download from CDN (no pre-known checksum; validated via schema after parsing).
        let url = format!(
            "{}/asset/v{}/manifest.json",
            self.config.cdn_base_url.trim_end_matches('/'),
            self.config.version
        );
        self.download_raw(&url, &manifest_path).await?;

        self.load_cached_manifest(version_dir).await
    }

    /// Load and parse a cached manifest.json.
    async fn load_cached_manifest(&self, version_dir: &Path) -> Result<Manifest> {
        let manifest_path = version_dir.join("manifest.json");
        let bytes = tokio::fs::read(&manifest_path).await?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Other(format!("failed to parse manifest: {e}")))?;
        Ok(manifest)
    }

    /// Ensure a file exists with correct checksum; download if missing or mismatched.
    async fn ensure_file(
        &self,
        relative_path: &str,
        expected_sha256: &str,
        dest: &Path,
        name: &str,
        progress: &Option<ProgressCallback>,
    ) -> Result<()> {
        // Check cache.
        if dest.exists()
            && let Ok(actual) = sha256_file(dest).await
            && actual == expected_sha256
        {
            if let Some(cb) = progress {
                cb(PrepareProgress {
                    name: name.to_string(),
                    current: 0,
                    total: 0,
                    phase: PreparePhase::Cached,
                });
            }
            return Ok(());
        }

        let url = format!(
            "{}/{}",
            self.config.cdn_base_url.trim_end_matches('/'),
            relative_path
        );

        download_and_verify(&url, dest, expected_sha256, name, |dl, tot| {
            if let Some(cb) = progress {
                cb(PrepareProgress {
                    name: name.to_string(),
                    current: 0,
                    total: 0,
                    phase: PreparePhase::Downloading {
                        downloaded: dl,
                        total: tot,
                    },
                });
            }
        })
        .await
    }

    /// Raw download without checksum verification (for manifest).
    async fn download_raw(&self, url: &str, dest: &Path) -> Result<()> {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("arcbox-boot/0.2")
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

        let temp_path = dest.with_extension("tmp");
        let mut file = tokio::fs::File::create(&temp_path).await?;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::Download(format!("stream error: {e}")))?;
            file.write_all(&chunk).await?;
        }

        file.flush().await?;
        drop(file);

        tokio::fs::rename(&temp_path, dest).await?;
        Ok(())
    }
}
