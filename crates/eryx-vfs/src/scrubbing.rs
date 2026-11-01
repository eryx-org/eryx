//! Storage wrapper that scrubs secret placeholders from file writes.
//!
//! This module provides a [`ScrubbingStorage`] wrapper that can be used with
//! any [`VfsStorage`] implementation to automatically scrub secret placeholders
//! before writing files.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::{DirEntry, InMemoryStorage, Metadata, VfsError, VfsResult, VfsStorage};

/// Configuration for a secret placeholder.
#[derive(Clone, Debug)]
pub struct SecretConfig {
    /// The placeholder value that should be scrubbed.
    pub placeholder: String,
}

/// Policy for which files should be scrubbed.
///
/// `Except` and `Only` match the normalized path being written against glob
/// patterns: `*` matches any run of characters other than `/`, `**` any run
/// including `/` (so `/cache/**` matches everything under `/cache`, and
/// `/a/**/b` also matches `/a/b`), and `?` one character other than `/`.
/// A path that can't be normalized is always scrubbed.
///
/// Scrubbing happens at write time, so under these policies
/// [`ScrubbingStorage`] re-scrubs a file renamed from an unscrubbed path to a
/// scrubbed one, and refuses to rename directories.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum FileScrubPolicy {
    /// Scrub all files.
    All,
    /// Don't scrub any files.
    None,
    /// Scrub all files except those matching any of these glob patterns.
    #[doc(hidden)]
    Except(Vec<String>),
    /// Only scrub files matching any of these glob patterns.
    #[doc(hidden)]
    Only(Vec<String>),
}

impl FileScrubPolicy {
    /// Check if a given path should be scrubbed according to this policy.
    pub fn should_scrub(&self, path: &str) -> bool {
        let patterns = match self {
            Self::All => return true,
            Self::None => return false,
            Self::Except(patterns) | Self::Only(patterns) => patterns,
        };
        let Ok(path) = InMemoryStorage::normalize_path(path) else {
            return true;
        };
        let matched = patterns.iter().any(|pattern| glob_match(pattern, &path));
        matches!(self, Self::Only(_)) == matched
    }

    /// Whether which files get scrubbed depends on their path.
    fn is_path_based(&self) -> bool {
        matches!(self, Self::Except(_) | Self::Only(_))
    }
}

/// Match `path` against a glob `pattern`; see [`FileScrubPolicy`] for the
/// syntax.
fn glob_match(pattern: &str, path: &str) -> bool {
    let (p, s) = (pattern.as_bytes(), path.as_bytes());
    // m[i][j]: whether p[i..] matches s[j..]. A table rather than
    // backtracking, since the path is guest-controlled.
    let mut m = vec![vec![false; s.len() + 1]; p.len() + 1];
    m[p.len()][s.len()] = true;
    for i in (0..p.len()).rev() {
        // For `**`: whether the rest of the pattern matches some suffix of s[j..].
        let mut rest_matches_suffix = false;
        for j in (0..=s.len()).rev() {
            let next = s.get(j);
            m[i][j] = match p[i] {
                b'*' if p.get(i + 1) == Some(&b'*') => {
                    rest_matches_suffix |= m[i + 2][j];
                    // `**/` may also match no directories at all.
                    rest_matches_suffix || (p.get(i + 2) == Some(&b'/') && m[i + 3][j])
                }
                b'*' => m[i + 1][j] || (next.is_some_and(|&c| c != b'/') && m[i][j + 1]),
                b'?' => next.is_some_and(|&c| c != b'/') && m[i + 1][j + 1],
                c => next == Some(&c) && m[i + 1][j + 1],
            };
        }
    }
    m[0][0]
}

/// A VfsStorage wrapper that scrubs secret placeholders from file writes.
///
/// This wrapper intercepts `write()` and `write_at()` calls and replaces
/// secret placeholders with `[REDACTED]` before passing to the underlying
/// storage.
///
/// # Example
///
/// ```rust,ignore
/// use eryx_vfs::{InMemoryStorage, scrubbing::{ScrubbingStorage, SecretConfig, FileScrubPolicy}};
/// use std::{sync::Arc, collections::HashMap};
///
/// let base_storage = Arc::new(InMemoryStorage::new());
///
/// let mut secrets = HashMap::new();
/// secrets.insert("API_KEY".to_string(), SecretConfig {
///     placeholder: "ERYX_SECRET_PLACEHOLDER_abc123".to_string(),
/// });
///
/// let scrubbing_storage = ScrubbingStorage::new(
///     base_storage,
///     secrets,
///     FileScrubPolicy::All,
/// );
/// ```
#[derive(Debug)]
pub struct ScrubbingStorage<S> {
    inner: S,
    secrets: HashMap<String, SecretConfig>,
    policy: FileScrubPolicy,
}

impl<S> ScrubbingStorage<S> {
    /// Create a new scrubbing storage wrapper.
    pub fn new(inner: S, secrets: HashMap<String, SecretConfig>, policy: FileScrubPolicy) -> Self {
        Self {
            inner,
            secrets,
            policy,
        }
    }

    /// Scrub secret placeholders from data if policy allows.
    ///
    /// With `keep_len`, each placeholder is replaced by a marker of exactly its
    /// own length (`[REDACTED]` padded with `*` or truncated), so the data
    /// keeps its size and byte positions. Positional writes need this: the
    /// caller has already accounted for `data.len()` bytes at the offset.
    fn scrub_if_needed(&self, path: &str, data: &[u8], keep_len: bool) -> Vec<u8> {
        if !self.policy.should_scrub(path) || self.secrets.is_empty() {
            return data.to_vec();
        }

        let mut result = data.to_vec();
        for secret_config in self.secrets.values() {
            let placeholder = secret_config.placeholder.as_bytes();
            let mut replacement = b"[REDACTED]".to_vec();
            if keep_len {
                replacement.resize(placeholder.len(), b'*');
            }
            result = replace_bytes(&result, placeholder, &replacement);
        }
        result
    }
}

#[async_trait]
impl<S: VfsStorage> VfsStorage for ScrubbingStorage<S> {
    async fn read(&self, path: &str) -> VfsResult<Vec<u8>> {
        self.inner.read(path).await
    }

    async fn read_at(&self, path: &str, offset: u64, len: u64) -> VfsResult<Vec<u8>> {
        self.inner.read_at(path, offset, len).await
    }

    async fn write(&self, path: &str, data: &[u8]) -> VfsResult<()> {
        let scrubbed = self.scrub_if_needed(path, data, false);
        self.inner.write(path, &scrubbed).await
    }

    async fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> VfsResult<()> {
        let scrubbed = self.scrub_if_needed(path, data, true);
        self.inner.write_at(path, offset, &scrubbed).await
    }

    async fn set_size(&self, path: &str, size: u64) -> VfsResult<()> {
        self.inner.set_size(path, size).await
    }

    async fn delete(&self, path: &str) -> VfsResult<()> {
        self.inner.delete(path).await
    }

    async fn exists(&self, path: &str) -> VfsResult<bool> {
        self.inner.exists(path).await
    }

    async fn list(&self, path: &str) -> VfsResult<Vec<DirEntry>> {
        self.inner.list(path).await
    }

    async fn stat(&self, path: &str) -> VfsResult<Metadata> {
        self.inner.stat(path).await
    }

    async fn mkdir(&self, path: &str) -> VfsResult<()> {
        self.inner.mkdir(path).await
    }

    async fn rmdir(&self, path: &str) -> VfsResult<()> {
        self.inner.rmdir(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> VfsResult<()> {
        if self.policy.is_path_based() && !self.secrets.is_empty() {
            // A rename could carry unscrubbed contents to a scrubbed path; for
            // a directory that means checking everything beneath it.
            if self.inner.stat(from).await?.is_dir {
                return Err(VfsError::PermissionDenied(format!(
                    "renaming directories is not supported with path-based file scrubbing: {from}"
                )));
            }
            if !self.policy.should_scrub(from) && self.policy.should_scrub(to) {
                let data = self.inner.read(from).await?;
                self.write(to, &data).await?;
                return self.inner.delete(from).await;
            }
        }
        self.inner.rename(from, to).await
    }

    async fn set_times(
        &self,
        path: &str,
        accessed: Option<std::time::SystemTime>,
        modified: Option<std::time::SystemTime>,
    ) -> VfsResult<()> {
        self.inner.set_times(path, accessed, modified).await
    }

    fn mkdir_sync(&self, path: &str) -> VfsResult<()> {
        self.inner.mkdir_sync(path)
    }
}

/// Replace all occurrences of a byte sequence with another.
fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(haystack.len());
    let mut i = 0;

    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            result.extend_from_slice(replacement);
            i += needle.len();
        } else {
            result.push(haystack[i]);
            i += 1;
        }
    }

    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::InMemoryStorage;

    #[tokio::test]
    async fn test_scrubbing_text_file() {
        let storage = InMemoryStorage::new();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            SecretConfig {
                placeholder: "PLACEHOLDER_123".to_string(),
            },
        );

        let scrubbing = ScrubbingStorage::new(storage, secrets, FileScrubPolicy::All);

        // Write file with placeholder
        scrubbing
            .write("/test.txt", b"Secret: PLACEHOLDER_123")
            .await
            .unwrap();

        // Read back - should be scrubbed
        let content = scrubbing.read("/test.txt").await.unwrap();
        let text = String::from_utf8(content).unwrap();
        assert_eq!(text, "Secret: [REDACTED]");
    }

    #[tokio::test]
    async fn test_scrubbing_disabled() {
        let storage = InMemoryStorage::new();
        let secrets = HashMap::new();

        let scrubbing = ScrubbingStorage::new(storage, secrets, FileScrubPolicy::None);

        scrubbing
            .write("/test.txt", b"PLACEHOLDER_123")
            .await
            .unwrap();

        let content = scrubbing.read("/test.txt").await.unwrap();
        assert_eq!(content, b"PLACEHOLDER_123");
    }

    #[tokio::test]
    async fn test_multiple_placeholders() {
        let storage = InMemoryStorage::new();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY1".to_string(),
            SecretConfig {
                placeholder: "PLACEHOLDER_1".to_string(),
            },
        );
        secrets.insert(
            "KEY2".to_string(),
            SecretConfig {
                placeholder: "PLACEHOLDER_2".to_string(),
            },
        );

        let scrubbing = ScrubbingStorage::new(storage, secrets, FileScrubPolicy::All);

        scrubbing
            .write("/test.txt", b"Keys: PLACEHOLDER_1 and PLACEHOLDER_2")
            .await
            .unwrap();

        let content = scrubbing.read("/test.txt").await.unwrap();
        let text = String::from_utf8(content).unwrap();
        assert_eq!(text, "Keys: [REDACTED] and [REDACTED]");
    }

    /// Positional writes must not change length, or the bytes after the
    /// placeholder land at the wrong offsets.
    #[tokio::test]
    async fn test_write_at_keeps_layout() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            SecretConfig {
                placeholder: "PLACEHOLDER_123".to_string(),
            },
        );
        let scrubbing =
            ScrubbingStorage::new(InMemoryStorage::new(), secrets, FileScrubPolicy::All);

        scrubbing
            .write_at("/t", 0, b"a=PLACEHOLDER_123;")
            .await
            .unwrap();
        scrubbing.write_at("/t", 18, b"b=2").await.unwrap();

        let text = String::from_utf8(scrubbing.read("/t").await.unwrap()).unwrap();
        assert_eq!(text, "a=[REDACTED]*****;b=2");
    }

    #[test]
    fn test_replace_bytes() {
        let data = b"Hello NEEDLE World NEEDLE!";
        let result = replace_bytes(data, b"NEEDLE", b"REPLACEMENT");
        assert_eq!(result, b"Hello REPLACEMENT World REPLACEMENT!");
    }

    fn secrets() -> HashMap<String, SecretConfig> {
        HashMap::from([(
            "KEY".to_string(),
            SecretConfig {
                placeholder: "PLACEHOLDER_SECRET".to_string(),
            },
        )])
    }

    async fn written(scrubbing: &ScrubbingStorage<InMemoryStorage>, path: &str) -> String {
        scrubbing.write(path, b"PLACEHOLDER_SECRET").await.unwrap();
        String::from_utf8(scrubbing.read(path).await.unwrap()).unwrap()
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("/a/*", "/a/b"));
        assert!(!glob_match("/a/*", "/a/b/c"));
        assert!(glob_match("/a/**", "/a/b/c"));
        assert!(glob_match("/a/**/c", "/a/c"));
        assert!(glob_match("/a/**/c", "/a/x/y/c"));
        assert!(!glob_match("/a/**/c", "/a/x/yc"));
        assert!(glob_match("/a/*.log", "/a/run.log"));
        assert!(!glob_match("/a/*.log", "/a/run.txt"));
        assert!(glob_match("/a/?.txt", "/a/b.txt"));
        assert!(!glob_match("/a/?.txt", "/a//.txt"));
        assert!(!glob_match("/a/b", "/a/bc"));
        assert!(glob_match("**", "/anything/at/all"));
    }

    #[test]
    fn test_should_scrub_policy_behavior() {
        assert!(FileScrubPolicy::All.should_scrub("/any/path"));
        assert!(!FileScrubPolicy::None.should_scrub("/any/path"));

        let only = FileScrubPolicy::Only(vec!["/out/**".to_string()]);
        assert!(only.should_scrub("/out/f"));
        assert!(only.should_scrub("/out/sub/f"));
        assert!(!only.should_scrub("/tmp/f"));

        let except = FileScrubPolicy::Except(vec!["/cache/*".to_string()]);
        assert!(!except.should_scrub("/cache/f"));
        assert!(except.should_scrub("/cache/sub/f"));
        assert!(except.should_scrub("/data/f"));
    }

    /// Patterns match the normalized path, so `.`/`..` can't dodge them, and
    /// a path that can't be normalized is scrubbed.
    #[test]
    fn test_policy_matches_normalized_path() {
        let only = FileScrubPolicy::Only(vec!["/out/*".to_string()]);
        assert!(only.should_scrub("/out/./f"));
        assert!(only.should_scrub("/tmp/../out/f"));
        assert!(only.should_scrub("relative"));

        let except = FileScrubPolicy::Except(vec!["/cache/*".to_string()]);
        assert!(except.should_scrub("/cache/../f"));
        assert!(!except.should_scrub("/cache/./f"));
        assert!(except.should_scrub("/../cache/f"));
    }

    #[tokio::test]
    async fn test_policy_only() {
        let storage = InMemoryStorage::new();
        storage.mkdir("/out").await.unwrap();
        let scrubbing = ScrubbingStorage::new(
            storage,
            secrets(),
            FileScrubPolicy::Only(vec!["/out/*".to_string()]),
        );

        assert_eq!(written(&scrubbing, "/out/f").await, "[REDACTED]");
        assert_eq!(written(&scrubbing, "/f").await, "PLACEHOLDER_SECRET");
    }

    #[tokio::test]
    async fn test_policy_except() {
        let storage = InMemoryStorage::new();
        storage.mkdir("/cache").await.unwrap();
        let scrubbing = ScrubbingStorage::new(
            storage,
            secrets(),
            FileScrubPolicy::Except(vec!["/cache/*".to_string()]),
        );

        assert_eq!(written(&scrubbing, "/cache/f").await, "PLACEHOLDER_SECRET");
        assert_eq!(written(&scrubbing, "/f").await, "[REDACTED]");
    }

    /// Writing under an unscrubbed path and renaming into a scrubbed one must
    /// still end up scrubbed.
    #[tokio::test]
    async fn test_path_policy_rename() {
        let storage = InMemoryStorage::new();
        storage.mkdir("/cache").await.unwrap();
        storage.mkdir("/cache/dir").await.unwrap();
        let scrubbing = ScrubbingStorage::new(
            storage,
            secrets(),
            FileScrubPolicy::Except(vec!["/cache/**".to_string()]),
        );

        written(&scrubbing, "/cache/f").await;
        scrubbing.rename("/cache/f", "/f").await.unwrap();
        assert_eq!(scrubbing.read("/f").await.unwrap(), b"[REDACTED]");
        assert!(!scrubbing.exists("/cache/f").await.unwrap());

        written(&scrubbing, "/cache/g").await;
        scrubbing.rename("/cache/g", "/cache/h").await.unwrap();
        assert_eq!(
            scrubbing.read("/cache/h").await.unwrap(),
            b"PLACEHOLDER_SECRET"
        );

        let err = scrubbing.rename("/cache/dir", "/dir").await.unwrap_err();
        assert!(matches!(err, VfsError::PermissionDenied(_)), "{err:?}");
    }
}
