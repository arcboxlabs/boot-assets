//! Verification cache: skip re-hashing unchanged assets.
//!
//! After a successful verification or download, the digest is recorded with the
//! file's `(size, mtime)` in a `.verified.json` next to the assets. Subsequent
//! checks trust the recorded digest while the stat signature and expected digest
//! both still match, and fall back to a full hash otherwise.
//!
//! The cache is purely an optimization: a missing or corrupt cache file degrades
//! to the previous behavior, never to an error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CACHE_FILE_NAME: &str = ".verified.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    sha256: String,
    size: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    entries: HashMap<String, Entry>,
}

#[derive(Debug)]
pub(crate) struct VerifyCache {
    file: CacheFile,
    path: PathBuf,
    dirty: bool,
}

impl VerifyCache {
    pub(crate) async fn load(dir: &Path) -> Self {
        let path = dir.join(CACHE_FILE_NAME);
        let file = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => CacheFile::default(),
        };
        Self {
            file,
            path,
            dirty: false,
        }
    }

    pub(crate) async fn is_verified(&self, path: &Path, expected_sha256: &str) -> bool {
        let Some(entry) = self.file.entries.get(&key(path)) else {
            return false;
        };
        if entry.sha256 != expected_sha256 {
            return false;
        }
        match signature(path).await {
            Some((size, mtime_secs, mtime_nanos)) => {
                entry.size == size
                    && entry.mtime_secs == mtime_secs
                    && entry.mtime_nanos == mtime_nanos
            }
            None => false,
        }
    }

    pub(crate) async fn record(&mut self, path: &Path, sha256: &str) {
        let Some((size, mtime_secs, mtime_nanos)) = signature(path).await else {
            return;
        };
        let entry = Entry {
            sha256: sha256.to_string(),
            size,
            mtime_secs,
            mtime_nanos,
        };
        if self.file.entries.get(&key(path)) != Some(&entry) {
            self.file.entries.insert(key(path), entry);
            self.dirty = true;
        }
    }

    pub(crate) async fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Ok(bytes) = serde_json::to_vec_pretty(&self.file) else {
            return;
        };
        let tmp = self.path.with_extension("json.tmp");
        if tokio::fs::write(&tmp, bytes).await.is_ok()
            && tokio::fs::rename(&tmp, &self.path).await.is_ok()
        {
            self.dirty = false;
        }
    }
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn signature(path: &Path) -> Option<(u64, u64, u32)> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let mtime = metadata.modified().ok()?;
    let since_epoch = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((
        metadata.len(),
        since_epoch.as_secs(),
        since_epoch.subsec_nanos(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write_file(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        tokio::fs::write(&path, contents).await.unwrap();
        path
    }

    #[tokio::test]
    async fn record_then_verify_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let asset = write_file(dir.path(), "asset", b"hello").await;

        let mut cache = VerifyCache::load(dir.path()).await;
        assert!(!cache.is_verified(&asset, "abc").await);

        cache.record(&asset, "abc").await;
        cache.save().await;

        let cache = VerifyCache::load(dir.path()).await;
        assert!(cache.is_verified(&asset, "abc").await);
        assert!(!cache.is_verified(&asset, "def").await);
    }

    #[tokio::test]
    async fn modified_file_invalidates_entry() {
        let dir = tempfile::tempdir().unwrap();
        let asset = write_file(dir.path(), "asset", b"hello").await;

        let mut cache = VerifyCache::load(dir.path()).await;
        cache.record(&asset, "abc").await;
        cache.save().await;

        let past = filetime::FileTime::from_unix_time(1_000_000, 0);
        filetime::set_file_mtime(&asset, past).unwrap();

        let cache = VerifyCache::load(dir.path()).await;
        assert!(!cache.is_verified(&asset, "abc").await);
    }

    #[tokio::test]
    async fn corrupt_cache_file_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), CACHE_FILE_NAME, b"{not json").await;

        let asset = write_file(dir.path(), "asset", b"hello").await;
        let cache = VerifyCache::load(dir.path()).await;
        assert!(!cache.is_verified(&asset, "abc").await);
    }

    #[tokio::test]
    async fn missing_file_is_not_verified() {
        let dir = tempfile::tempdir().unwrap();
        let asset = write_file(dir.path(), "asset", b"hello").await;

        let mut cache = VerifyCache::load(dir.path()).await;
        cache.record(&asset, "abc").await;
        cache.save().await;

        tokio::fs::remove_file(&asset).await.unwrap();
        let cache = VerifyCache::load(dir.path()).await;
        assert!(!cache.is_verified(&asset, "abc").await);
    }
}
