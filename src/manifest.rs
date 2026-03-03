use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Schema version for the new multi-target manifest format.
pub const SCHEMA_VERSION: u32 = 7;

/// Top-level boot asset manifest (schema v7).
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
}

/// A single architecture variant of a binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryTarget {
    pub path: String,
    pub sha256: String,
}
