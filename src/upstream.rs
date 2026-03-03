use std::collections::BTreeMap;

use serde::Deserialize;

/// Root structure of `upstream.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamConfig {
    pub binaries: Vec<UpstreamBinary>,
}

/// A single upstream binary declaration.
#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamBinary {
    pub name: String,
    pub version: String,
    /// Per-architecture source definitions.
    pub source: BTreeMap<String, UpstreamSource>,
}

/// Where to download a binary for a specific architecture.
#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamSource {
    /// Download URL (typically a .tar.gz or .tgz).
    pub url: String,
    /// Path inside the archive to extract (e.g. "docker/dockerd").
    pub extract: String,
}

impl UpstreamConfig {
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))
    }
}
