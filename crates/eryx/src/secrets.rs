//! Secrets management with placeholder substitution.
//!
//! This module provides secure secrets handling for sandboxed Python code.
//! Secrets are exposed as placeholders to the sandbox, and real values are
//! transparently substituted only when making HTTP requests to authorized hosts.
//!
//! Placeholders are scrubbed from outputs (stdout, stderr, files) to prevent
//! leakage, providing better security than direct secret access.

use std::collections::HashMap;

/// A secret with its placeholder and allowed hosts.
#[derive(Clone)]
pub struct SecretConfig {
    /// The real secret value (never exposed to sandbox)
    pub(crate) real_value: String,
    /// Generated placeholder (what Python sees)
    pub(crate) placeholder: String,
    /// Host restrictions for this secret.
    ///
    /// If empty, falls back to `NetConfig.allowed_hosts`, but only while
    /// `allow_all_hosts` is off. A secret with no hosts either way is never
    /// substituted, and the sandbox refuses to build with it; see
    /// [`NetConfig::secret_hosts`](crate::NetConfig::secret_hosts).
    pub(crate) allowed_hosts: Vec<String>,
}

impl SecretConfig {
    /// A secret with the real value `real_value`, which may only be sent to
    /// hosts matching `allowed_hosts`, and a newly generated placeholder.
    ///
    /// If `allowed_hosts` is empty, the secret falls back to
    /// `NetConfig`'s allowed hosts; see
    /// [`NetConfig::secret_hosts`](crate::NetConfig::secret_hosts).
    #[must_use]
    pub fn new(real_value: impl Into<String>, allowed_hosts: Vec<String>) -> Self {
        Self {
            real_value: real_value.into(),
            placeholder: random_placeholder(),
            allowed_hosts,
        }
    }

    /// The placeholder the sandbox sees in place of the real value.
    #[must_use]
    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    /// Host patterns this secret may be sent to. Empty means it falls back to
    /// `NetConfig`'s allowed hosts.
    #[must_use]
    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }
}

impl std::fmt::Debug for SecretConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretConfig")
            .field("real_value", &"[REDACTED]")
            .field("placeholder", &self.placeholder)
            .field("allowed_hosts", &self.allowed_hosts)
            .finish()
    }
}

/// File scrubbing policy for preventing secret leakage via file writes.
///
/// The experimental path-based policies (`Except`, `Only`) match the
/// normalized path being written against glob patterns: `*` matches any run
/// of characters other than `/`, `**` any run including `/` (so `/cache/**`
/// matches everything under `/cache`, and `/a/**/b` also matches `/a/b`),
/// and `?` one character other than `/`. A path that can't be normalized is
/// always scrubbed. Scrubbing happens at write time, so under these policies
/// a file renamed from an unscrubbed path to a scrubbed one is scrubbed
/// again, and renaming directories is refused.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum FileScrubPolicy {
    /// Scrub all files (default when secrets configured)
    #[default]
    All,
    /// Don't scrub any files
    None,
    /// Scrub all files except those whose path matches any of these glob patterns.
    ///
    /// **Experimental.** See [`FileScrubPolicy`] for the pattern syntax.
    #[doc(hidden)]
    Except(Vec<String>),
    /// Scrub only files whose path matches any of these glob patterns.
    ///
    /// **Experimental.** See [`FileScrubPolicy`] for the pattern syntax.
    #[doc(hidden)]
    Only(Vec<String>),
}

impl From<bool> for FileScrubPolicy {
    fn from(enabled: bool) -> Self {
        if enabled { Self::All } else { Self::None }
    }
}

impl FileScrubPolicy {
    /// Scrub all files
    #[must_use]
    pub fn all() -> Self {
        Self::All
    }

    /// Don't scrub any files
    #[must_use]
    pub fn none() -> Self {
        Self::None
    }

    /// Scrub all files except those matching these glob patterns.
    ///
    /// **Experimental.** See [`FileScrubPolicy`] for the pattern syntax.
    #[doc(hidden)]
    #[must_use]
    pub fn except(paths: Vec<String>) -> Self {
        Self::Except(paths)
    }

    /// Only scrub files matching these glob patterns.
    ///
    /// **Experimental.** See [`FileScrubPolicy`] for the pattern syntax.
    #[doc(hidden)]
    #[must_use]
    pub fn only(paths: Vec<String>) -> Self {
        Self::Only(paths)
    }
}

/// Output stream scrubbing policy for preventing secret leakage via stdout/stderr.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum OutputScrubPolicy {
    /// Scrub output (default when secrets configured)
    #[default]
    All,
    /// Don't scrub output
    None,
}

impl From<bool> for OutputScrubPolicy {
    fn from(enabled: bool) -> Self {
        if enabled { Self::All } else { Self::None }
    }
}

/// Generate a unique placeholder for a secret.
///
/// Format: `ERYX_SECRET_PLACEHOLDER_{random_hex}`
///
/// Placeholders are ephemeral (regenerated on each use) for better security.
fn random_placeholder() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let random: [u8; 16] = rng.random();
    let hex = hex::encode(random);
    format!("ERYX_SECRET_PLACEHOLDER_{hex}")
}

/// Scrub secret placeholders from text, replacing them with `[REDACTED]`.
pub fn scrub_placeholders(text: &str, secrets: &HashMap<String, SecretConfig>) -> String {
    let mut result = text.to_string();
    for secret in secrets.values() {
        result = result.replace(&secret.placeholder, "[REDACTED]");
    }
    result
}

/// Scrub secret placeholders from byte data.
///
/// For text data (valid UTF-8), performs string replacement.
/// For binary data, performs byte sequence search.
pub fn scrub_placeholders_bytes(data: &[u8], secrets: &HashMap<String, SecretConfig>) -> Vec<u8> {
    if let Ok(text) = std::str::from_utf8(data) {
        // Text file - string replacement
        scrub_placeholders(text, secrets).into_bytes()
    } else {
        // Binary file - byte sequence search
        let mut result = data.to_vec();
        for secret in secrets.values() {
            result = replace_bytes(&result, secret.placeholder.as_bytes(), b"[REDACTED]");
        }
        result
    }
}

/// Replace all occurrences of a byte sequence with another.
pub(crate) fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
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
mod tests {
    use super::*;

    #[test]
    fn new_secret_gets_a_fresh_placeholder_and_hides_its_value() {
        let a = SecretConfig::new("real-secret", vec!["api.example.com".into()]);
        let b = SecretConfig::new("real-secret", vec![]);
        assert!(a.placeholder().starts_with("ERYX_SECRET_PLACEHOLDER_"));
        assert_ne!(a.placeholder(), b.placeholder());
        assert_eq!(a.allowed_hosts(), ["api.example.com"]);
        assert!(!format!("{a:?}").contains("real-secret"));
    }

    #[test]
    fn test_scrub_placeholders() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "API_KEY".to_string(),
            SecretConfig {
                real_value: "real-secret".to_string(),
                placeholder: "ERYX_SECRET_PLACEHOLDER_abc123".to_string(),
                allowed_hosts: vec![],
            },
        );

        let text = "My key is: ERYX_SECRET_PLACEHOLDER_abc123";
        let scrubbed = scrub_placeholders(text, &secrets);
        assert_eq!(scrubbed, "My key is: [REDACTED]");
    }

    #[test]
    fn test_scrub_multiple_placeholders() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY1".to_string(),
            SecretConfig {
                real_value: "secret1".to_string(),
                placeholder: "PLACEHOLDER_1".to_string(),
                allowed_hosts: vec![],
            },
        );
        secrets.insert(
            "KEY2".to_string(),
            SecretConfig {
                real_value: "secret2".to_string(),
                placeholder: "PLACEHOLDER_2".to_string(),
                allowed_hosts: vec![],
            },
        );

        let text = "Keys: PLACEHOLDER_1 and PLACEHOLDER_2";
        let scrubbed = scrub_placeholders(text, &secrets);
        assert_eq!(scrubbed, "Keys: [REDACTED] and [REDACTED]");
    }

    #[test]
    fn test_replace_bytes() {
        let data = b"Hello NEEDLE World NEEDLE!";
        let result = replace_bytes(data, b"NEEDLE", b"REPLACEMENT");
        assert_eq!(result, b"Hello REPLACEMENT World REPLACEMENT!".to_vec());
    }

    #[test]
    fn test_scrub_placeholders_bytes_utf8() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "KEY".to_string(),
            SecretConfig {
                real_value: "secret".to_string(),
                placeholder: "PLACEHOLDER".to_string(),
                allowed_hosts: vec![],
            },
        );

        let data = b"Key: PLACEHOLDER";
        let scrubbed = scrub_placeholders_bytes(data, &secrets);
        assert_eq!(scrubbed, b"Key: [REDACTED]");
    }

    #[test]
    fn test_file_scrub_policy_from_bool() {
        let policy_true: FileScrubPolicy = true.into();
        let policy_false: FileScrubPolicy = false.into();

        assert!(matches!(policy_true, FileScrubPolicy::All));
        assert!(matches!(policy_false, FileScrubPolicy::None));
    }

    #[test]
    fn test_output_scrub_policy_from_bool() {
        let policy_true: OutputScrubPolicy = true.into();
        let policy_false: OutputScrubPolicy = false.into();

        assert!(matches!(policy_true, OutputScrubPolicy::All));
        assert!(matches!(policy_false, OutputScrubPolicy::None));
    }
}
