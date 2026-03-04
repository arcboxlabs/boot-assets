/// Errors from boot-assets operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("architecture '{0}' not found in manifest")]
    ArchNotFound(String),

    #[error("binary '{name}' has no target for architecture '{arch}'")]
    BinaryArchNotFound { name: String, arch: String },

    #[error("sha256 mismatch for '{name}': expected {expected}, got {actual}")]
    ChecksumMismatch {
        name: String,
        expected: String,
        actual: String,
    },

    #[error("unsupported manifest schema version {version}, expected {expected}")]
    UnsupportedSchema { version: u32, expected: u32 },

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("download failed: {0}")]
    Download(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
