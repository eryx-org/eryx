//! Subresource Integrity hashes (`sha512-<base64>`), as npm records them.

use base64::Engine as _;
use sha2::{Digest, Sha256, Sha512};

use crate::{Error, Result};

/// A parsed SRI hash. Only SHA-512 and SHA-256 are accepted: npm's legacy
/// SHA-1 `shasum` is too weak to pin a download with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    algorithm: Algorithm,
    digest: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Algorithm {
    Sha256,
    Sha512,
}

impl Integrity {
    /// Parse an SRI string. When it lists several hashes (space-separated),
    /// the strongest supported one is used.
    ///
    /// # Errors
    ///
    /// Fails if it has no SHA-512 or SHA-256 hash, or one isn't valid base64
    /// of the right length.
    pub fn parse(sri: &str) -> Result<Self> {
        let bad = || Error::Integrity {
            what: "integrity string".to_string(),
            expected: sri.to_string(),
        };
        let mut best: Option<Self> = None;
        for token in sri.split_ascii_whitespace() {
            // Options after `?` are allowed by the spec and ignored.
            let token = token.split('?').next().unwrap_or_default();
            let Some((algorithm, digest)) = token.split_once('-') else {
                continue;
            };
            let (algorithm, len) = match algorithm {
                "sha512" => (Algorithm::Sha512, 64),
                "sha256" => (Algorithm::Sha256, 32),
                _ => continue,
            };
            let digest = base64::engine::general_purpose::STANDARD
                .decode(digest)
                .map_err(|_| bad())?;
            if digest.len() != len {
                return Err(bad());
            }
            if best
                .as_ref()
                .is_none_or(|b| b.algorithm == Algorithm::Sha256)
            {
                best = Some(Self { algorithm, digest });
            }
        }
        best.ok_or_else(bad)
    }

    /// The SHA-512 SRI string of `bytes`.
    #[must_use]
    pub fn sha512_of(bytes: &[u8]) -> Self {
        Self {
            algorithm: Algorithm::Sha512,
            digest: Sha512::digest(bytes).to_vec(),
        }
    }

    /// Whether `bytes` hash to this.
    #[must_use]
    pub fn matches(&self, bytes: &[u8]) -> bool {
        let actual = match self.algorithm {
            Algorithm::Sha512 => Sha512::digest(bytes).to_vec(),
            Algorithm::Sha256 => Sha256::digest(bytes).to_vec(),
        };
        actual == self.digest
    }

    /// Check `bytes` against this, naming them `what` in the error.
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] if they don't match.
    pub fn check(&self, what: &str, bytes: &[u8]) -> Result<()> {
        if self.matches(bytes) {
            Ok(())
        } else {
            Err(Error::Integrity {
                what: what.to_string(),
                expected: self.to_string(),
            })
        }
    }

    /// A file name for content with this hash: `<algorithm>-<hex>`.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!("{}-{}", self.algorithm_name(), hex::encode(&self.digest))
    }

    fn algorithm_name(&self) -> &'static str {
        match self.algorithm {
            Algorithm::Sha512 => "sha512",
            Algorithm::Sha256 => "sha256",
        }
    }
}

impl std::fmt::Display for Integrity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let digest = base64::engine::general_purpose::STANDARD.encode(&self.digest);
        write!(f, "{}-{digest}", self.algorithm_name())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_checks() {
        let sri = Integrity::sha512_of(b"hello");
        let parsed = Integrity::parse(&sri.to_string()).unwrap();
        assert_eq!(parsed, sri);
        assert!(parsed.matches(b"hello"));
        assert!(parsed.check("x", b"hellO").is_err());
        assert!(sri.file_name().starts_with("sha512-"));
    }

    #[test]
    fn prefers_sha512_and_refuses_weak_or_malformed() {
        let sha256 = format!(
            "sha256-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"x"))
        );
        let sha512 = Integrity::sha512_of(b"x").to_string();
        let both = Integrity::parse(&format!("{sha256} {sha512}")).unwrap();
        assert_eq!(both.to_string(), sha512);
        assert!(Integrity::parse(&sha256).unwrap().matches(b"x"));

        for bad in [
            "",
            "sha1-AAAA",
            "sha512-not base64!",
            "sha512-AAAA",
            "md5-abc",
        ] {
            assert!(Integrity::parse(bad).is_err(), "{bad}");
        }
    }
}
