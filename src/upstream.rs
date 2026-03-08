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
    /// Source format. Defaults to `tgz` for backward compatibility.
    #[serde(default)]
    pub format: UpstreamSourceFormat,
    /// Download URL (typically a .tar.gz or .tgz).
    pub url: String,
    /// Path inside the archive to extract (e.g. "docker/dockerd").
    ///
    /// Required for `tgz`, omitted for `binary`.
    #[serde(default)]
    pub extract: Option<String>,
}

/// Supported upstream artifact formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamSourceFormat {
    /// A `.tar.gz` / `.tgz` archive where a single file is extracted.
    #[default]
    Tgz,
    /// A direct binary download.
    Binary,
}

impl UpstreamConfig {
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
        config.validate(path)?;
        Ok(config)
    }
}

impl UpstreamConfig {
    fn validate(&self, path: &std::path::Path) -> Result<(), String> {
        for binary in &self.binaries {
            for (arch, source) in &binary.source {
                match source.format {
                    UpstreamSourceFormat::Tgz => {
                        if source.extract.as_deref().is_none_or(str::is_empty) {
                            return Err(format!(
                                "invalid {}: binary '{}' arch '{}' requires 'extract' for format=tgz",
                                path.display(),
                                binary.name,
                                arch
                            ));
                        }
                    }
                    UpstreamSourceFormat::Binary => {
                        if source
                            .extract
                            .as_deref()
                            .is_some_and(|value| !value.is_empty())
                        {
                            return Err(format!(
                                "invalid {}: binary '{}' arch '{}' must not set 'extract' for format=binary",
                                path.display(),
                                binary.name,
                                arch
                            ));
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{UpstreamConfig, UpstreamSourceFormat};

    #[test]
    fn parse_defaults_to_tgz_format() {
        let config: UpstreamConfig = toml::from_str(
            r#"
[[binaries]]
name = "dockerd"
version = "27.5.1"

[binaries.source.arm64]
url = "https://example.invalid/docker.tgz"
extract = "docker/dockerd"
"#,
        )
        .unwrap();

        let source = &config.binaries[0].source["arm64"];
        assert_eq!(source.format, UpstreamSourceFormat::Tgz);
        assert_eq!(source.extract.as_deref(), Some("docker/dockerd"));
    }

    #[test]
    fn parse_binary_source_without_extract() {
        let config: UpstreamConfig = toml::from_str(
            r#"
[[binaries]]
name = "k3s"
version = "v1.34.3+k3s1"

[binaries.source.arm64]
format = "binary"
url = "https://example.invalid/k3s-arm64"
"#,
        )
        .unwrap();

        let source = &config.binaries[0].source["arm64"];
        assert_eq!(source.format, UpstreamSourceFormat::Binary);
        assert_eq!(source.extract, None);
    }
}
