//! Storage wrapper that scrubs secret placeholders from file writes.
//!
//! This module provides a [`ScrubbingStorage`] wrapper that can be used with
//! any [`VfsStorage`] implementation to automatically scrub secret placeholders
//! before writing files.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::{DirEntry, Metadata, VfsResult, VfsStorage};

/// Configuration for a secret placeholder.
#[derive(Clone, Debug)]
pub struct SecretConfig {
    /// The placeholder value that should be scrubbed.
    pub placeholder: String,
}

/// Policy for which files should be scrubbed.
#[derive(Debug, Clone)]
pub enum FileScrubPolicy {
    /// Scrub all files.
    All,
    /// Don't scrub any files.
    None,
    /// Scrub all except specified paths (glob patterns supported).
    ///
    /// **Not yet implemented** - currently behaves the same as `All` (fail closed).
    /// Do not use until glob matching is implemented.
    #[doc(hidden)]
    Except(Vec<String>),
    /// Only scrub specified paths (glob patterns supported).
    ///
    /// **Not yet implemented** - currently behaves the same as `All` (fail closed).
    /// Do not use until glob matching is implemented.
    #[doc(hidden)]
    Only(Vec<String>),
}

impl FileScrubPolicy {
    /// Check if a given path should be scrubbed according to this policy.
    ///
    /// Note: Glob matching is not yet implemented. Both `Except` and `Only`
    /// policies currently fail closed (scrub everything) for security.
    pub fn should_scrub(&self, _path: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Except(_patterns) => {
                // TODO: Implement glob matching
                // Fail closed: scrub everything until glob matching is implemented
                true
            }
            Self::Only(_patterns) => {
                // TODO: Implement glob matching
                // Fail closed: scrub everything until glob matching is implemented
                // (Previously returned false which was an insecure fail-open default)
                true
            }
        }
    }
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
    fn scrub_if_needed(&self, path: &str, data: &[u8]) -> Vec<u8> {
        if !self.policy.should_scrub(path) || self.secrets.is_empty() {
            return data.to_vec();
        }

        // Try to decode as UTF-8
        if let Ok(text) = std::str::from_utf8(data) {
            // Text file - string replacement
            let mut scrubbed = text.to_string();
            for secret_config in self.secrets.values() {
                scrubbed = scrubbed.replace(&secret_config.placeholder, "[REDACTED]");
            }
            scrubbed.into_bytes()
        } else {
            // Binary file - byte sequence search
            let mut result = data.to_vec();
            for secret_config in self.secrets.values() {
                result =
                    replace_bytes(&result, secret_config.placeholder.as_bytes(), b"[REDACTED]");
            }
            result
        }
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
        let scrubbed = self.scrub_if_needed(path, data);
        self.inner.write(path, &scrubbed).await
    }

    async fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> VfsResult<()> {
        let scrubbed = self.scrub_if_needed(path, data);
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
        self.inner.rename(from, to).await
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

    #[test]
    fn test_replace_bytes() {
        let data = b"Hello NEEDLE World NEEDLE!";
        let result = replace_bytes(data, b"NEEDLE", b"REPLACEMENT");
        assert_eq!(result, b"Hello REPLACEMENT World REPLACEMENT!");
    }

    /// Test that the hidden FileScrubPolicy::Only fails closed (scrubs everything).
    /// This is a security-critical test ensuring defense in depth - if someone uses
    /// the hidden variant, it must still scrub rather than leak secrets.
    #[tokio::test]
    async fn test_policy_only_fails_closed() {
        let storage = InMemoryStorage::new();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            SecretConfig {
                placeholder: "PLACEHOLDER_SECRET".to_string(),
            },
        );

        // FileScrubPolicy::Only should scrub ALL files until glob matching is implemented
        // (fail closed for security)
        let scrubbing = ScrubbingStorage::new(
            storage,
            secrets,
            FileScrubPolicy::Only(vec!["/allowed/*".to_string()]),
        );

        // Write to a path that would NOT match the "only" pattern
        // (using root path to avoid needing to create directories)
        scrubbing
            .write("/unmatched.txt", b"Secret: PLACEHOLDER_SECRET")
            .await
            .unwrap();

        // Should still be scrubbed because we fail closed
        let content = scrubbing.read("/unmatched.txt").await.unwrap();
        let text = String::from_utf8(content).unwrap();
        assert_eq!(
            text, "Secret: [REDACTED]",
            "FileScrubPolicy::Only must fail closed and scrub all files until glob matching is implemented"
        );
    }

    /// Test that the hidden FileScrubPolicy::Except also fails closed.
    /// Defense in depth - if someone uses the hidden variant, it must still scrub.
    #[tokio::test]
    async fn test_policy_except_fails_closed() {
        let storage = InMemoryStorage::new();
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            SecretConfig {
                placeholder: "PLACEHOLDER_SECRET".to_string(),
            },
        );

        // FileScrubPolicy::Except should scrub ALL files until glob matching is implemented
        let scrubbing = ScrubbingStorage::new(
            storage,
            secrets,
            FileScrubPolicy::Except(vec!["/excluded/*".to_string()]),
        );

        // Write to a path that would match the "except" pattern (should be excluded)
        // (using root path to avoid needing to create directories)
        scrubbing
            .write("/excluded.txt", b"Secret: PLACEHOLDER_SECRET")
            .await
            .unwrap();

        // Should still be scrubbed because we fail closed
        let content = scrubbing.read("/excluded.txt").await.unwrap();
        let text = String::from_utf8(content).unwrap();
        assert_eq!(
            text, "Secret: [REDACTED]",
            "FileScrubPolicy::Except must fail closed and scrub all files until glob matching is implemented"
        );
    }

    /// Test that should_scrub correctly returns true for all policies except None.
    /// Hidden variants (Only, Except) must fail closed for security.
    #[test]
    fn test_should_scrub_policy_behavior() {
        // All: always scrub
        assert!(FileScrubPolicy::All.should_scrub("/any/path"));

        // None: never scrub
        assert!(!FileScrubPolicy::None.should_scrub("/any/path"));

        // Only: fail closed (scrub everything until glob implemented)
        assert!(
            FileScrubPolicy::Only(vec!["/specific/*".to_string()]).should_scrub("/other/path"),
            "Only policy must fail closed"
        );
        assert!(
            FileScrubPolicy::Only(vec!["/specific/*".to_string()]).should_scrub("/specific/file"),
            "Only policy must fail closed"
        );

        // Except: fail closed (scrub everything until glob implemented)
        assert!(
            FileScrubPolicy::Except(vec!["/excluded/*".to_string()]).should_scrub("/other/path"),
            "Except policy must fail closed"
        );
        assert!(
            FileScrubPolicy::Except(vec!["/excluded/*".to_string()]).should_scrub("/excluded/file"),
            "Except policy must fail closed"
        );
    }
}
