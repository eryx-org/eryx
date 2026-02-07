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
#[derive(Clone, Debug)]
pub struct SecretConfig {
    /// The real secret value (never exposed to sandbox)
    pub(crate) real_value: String,
    /// Generated placeholder (what Python sees)
    pub(crate) placeholder: String,
    /// Host restrictions for this secret.
    ///
    /// **⚠️ Security Note:**
    /// - If empty, falls back to `NetConfig.allowed_hosts`
    /// - If both are empty, the secret can be sent to ANY host (except blocked_hosts)
    /// - Always specify explicit hosts for production use
    pub allowed_hosts: Vec<String>,
}

/// File scrubbing policy for preventing secret leakage via file writes.
#[derive(Debug, Clone, Default)]
pub enum FileScrubPolicy {
    /// Scrub all files (default when secrets configured)
    #[default]
    All,
    /// Don't scrub any files
    None,
    /// Scrub all except specified paths (glob patterns supported).
    ///
    /// **Not yet implemented** - currently behaves the same as `All` (fail closed).
    /// Do not use until glob matching is implemented in Phase 2.
    #[doc(hidden)]
    #[allow(dead_code)]
    Except(Vec<String>),
    /// Scrub only specified paths (glob patterns supported).
    ///
    /// **Not yet implemented** - currently behaves the same as `All` (fail closed).
    /// Do not use until glob matching is implemented in Phase 2.
    #[doc(hidden)]
    #[allow(dead_code)]
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

    /// Scrub all except specific paths (glob patterns supported).
    ///
    /// **Not yet implemented** - do not use until Phase 2.
    #[doc(hidden)]
    #[allow(dead_code)]
    #[must_use]
    pub fn except(paths: Vec<String>) -> Self {
        Self::Except(paths)
    }

    /// Only scrub specific paths (glob patterns supported).
    ///
    /// **Not yet implemented** - do not use until Phase 2.
    #[doc(hidden)]
    #[allow(dead_code)]
    #[must_use]
    pub fn only(paths: Vec<String>) -> Self {
        Self::Only(paths)
    }
}

/// Output stream scrubbing policy for preventing secret leakage via stdout/stderr.
#[derive(Debug, Clone, Default)]
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
pub(crate) fn generate_placeholder(_secret_name: &str) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let random: [u8; 16] = rng.r#gen();
    let hex = hex::encode(random);
    format!("ERYX_SECRET_PLACEHOLDER_{hex}")
}

/// Scrub secret placeholders from text, replacing them with `[REDACTED]`.
pub(crate) fn scrub_placeholders(text: &str, secrets: &HashMap<String, SecretConfig>) -> String {
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
/// Phase 2: Used for VFS file scrubbing.
#[allow(dead_code)]
pub(crate) fn scrub_placeholders_bytes(
    data: &[u8],
    secrets: &HashMap<String, SecretConfig>,
) -> Vec<u8> {
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
/// Phase 2: Used for VFS file scrubbing.
#[allow(dead_code)]
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
mod tests {
    use super::*;

    #[test]
    fn test_placeholder_generation() {
        let p1 = generate_placeholder("API_KEY");
        let p2 = generate_placeholder("API_KEY");

        assert!(p1.starts_with("ERYX_SECRET_PLACEHOLDER_"));
        assert!(p2.starts_with("ERYX_SECRET_PLACEHOLDER_"));
        assert_ne!(p1, p2, "Placeholders should be unique");
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
