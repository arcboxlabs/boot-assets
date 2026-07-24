use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Returns the schema version for a given asset version.
///
/// Schema version equals the major component of the asset version
/// (e.g. "0.2.3" → 0, "1.0.0" → 1).
pub fn schema_version_for(asset_version: &str) -> u32 {
    asset_version
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Top-level boot asset manifest.
///
/// Supports multiple target architectures and host-side binaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub asset_version: String,
    pub built_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha: Option<String>,
    /// Per-architecture boot targets (e.g. "arm64", "x86_64").
    pub targets: BTreeMap<String, Target>,
    /// Host-side binaries downloaded to ~/.arcbox/bin/.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binaries: Vec<Binary>,
}

/// Boot target for a single architecture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub kernel: FileEntry,
    pub rootfs: FileEntry,
    pub kernel_cmdline: String,
    /// Read-only EROFS image of the guest container-runtime binaries
    /// (dockerd, containerd, the shim, runc, …), attached to the VM as a
    /// block device.
    ///
    /// Exec'ing those over the VirtioFS share costs a FUSE round-trip per
    /// exec — measured 7-10x more than exec from block-backed storage, and
    /// paid on every container start. Optional so both skew directions stay
    /// safe: a newer consumer reading an older manifest sees `None` and
    /// keeps using the VirtioFS copies, and an older consumer ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<FileEntry>,
}

/// A file entry with path and checksum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Host-side binary with per-architecture variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binary {
    pub name: String,
    pub version: String,
    /// Per-architecture file entries (e.g. "arm64" -> { path, sha256 }).
    pub targets: BTreeMap<String, BinaryTarget>,
    /// Subdirectory under the VirtioFS share root (e.g. "kernel" → /arcbox/kernel/).
    /// Defaults to "bin" when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_dir: Option<String>,
}

/// A single architecture variant of a binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryTarget {
    pub path: String,
    pub sha256: String,
}
