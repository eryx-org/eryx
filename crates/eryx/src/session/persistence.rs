//! Session persistence for saving and loading session state to disk.
//!
//! This module provides types for serializing session state to files,
//! enabling persistent named sessions that survive process restarts.
//!
//! ## File Format
//!
//! Sessions are stored as JSON files with the `.session` extension:
//!
//! ```json
//! {
//!   "state": [/* base64-encoded pickle bytes */],
//!   "metadata": {
//!     "execution_count": 5,
//!     "eryx_version": "0.2.0"
//!   },
//!   "created_at": { "secs_since_epoch": 1234567890, "nanos_since_epoch": 0 },
//!   "last_active": { "secs_since_epoch": 1234567890, "nanos_since_epoch": 0 }
//! }
//! ```
//!
//! ## Example
//!
//! ```rust,ignore
//! use eryx::session::{InProcessSession, Session};
//! use std::path::Path;
//!
//! // Save a session to disk
//! session.execute("x = 42").await?;
//! session.save("my_session.session").await?;
//!
//! // Later, load the session
//! let mut session = InProcessSession::load(&sandbox, "my_session.session").await?;
//! let result = session.execute("print(x)").await?;  // prints "42"
//! ```

use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// The current eryx version, used for compatibility checking.
pub const ERYX_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Persisted session state for saving to disk.
///
/// This struct captures all the information needed to restore a session:
/// - The pickled Python state (variables, functions, classes)
/// - Metadata about the session (execution count, eryx version)
/// - Timestamps for tracking when the session was created and last used
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    /// Base64-encoded pickle bytes of Python state (from `snapshot_state`).
    ///
    /// We use base64 encoding to ensure the binary pickle data is safely
    /// stored in JSON format.
    #[serde(with = "base64_bytes")]
    pub state: Vec<u8>,

    /// Session metadata.
    pub metadata: SessionMetadata,

    /// When the session was first created.
    pub created_at: SerializableSystemTime,

    /// When the session was last active (last execution or save).
    pub last_active: SerializableSystemTime,
}

/// Session metadata stored alongside the state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Number of executions performed in the session.
    pub execution_count: u32,

    /// Version of eryx that created this session.
    ///
    /// Used for compatibility checking during load.
    pub eryx_version: String,
}

/// A wrapper around `SystemTime` that implements `Serialize` and `Deserialize`.
///
/// `SystemTime` doesn't implement serde traits directly, so we need this wrapper.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SerializableSystemTime {
    /// Seconds since the Unix epoch.
    pub secs_since_epoch: u64,
    /// Nanoseconds within the second.
    pub nanos_since_epoch: u32,
}

impl From<SystemTime> for SerializableSystemTime {
    fn from(time: SystemTime) -> Self {
        let duration = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            secs_since_epoch: duration.as_secs(),
            nanos_since_epoch: duration.subsec_nanos(),
        }
    }
}

impl From<SerializableSystemTime> for SystemTime {
    fn from(time: SerializableSystemTime) -> Self {
        SystemTime::UNIX_EPOCH
            + std::time::Duration::new(time.secs_since_epoch, time.nanos_since_epoch)
    }
}

impl PersistedSession {
    /// Create a new persisted session from state bytes and execution count.
    ///
    /// The `created_at` timestamp should be provided for existing sessions
    /// being re-saved, or `None` for newly created sessions.
    #[must_use]
    pub fn new(state: Vec<u8>, execution_count: u32, created_at: Option<SystemTime>) -> Self {
        let now = SystemTime::now();
        Self {
            state,
            metadata: SessionMetadata {
                execution_count,
                eryx_version: ERYX_VERSION.to_string(),
            },
            created_at: created_at.unwrap_or(now).into(),
            last_active: now.into(),
        }
    }

    /// Save the persisted session to a file.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to save the session to (should end in `.session`)
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub async fn save(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref();
        let json = serde_json::to_string_pretty(self)?;

        // Use tokio's async file I/O
        tokio::fs::write(path, json).await?;

        tracing::debug!(
            path = %path.display(),
            state_size = self.state.len(),
            "Session saved to disk"
        );

        Ok(())
    }

    /// Load a persisted session from a file.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to load the session from
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file cannot be read
    /// - The file is not valid JSON
    /// - The session format is incompatible
    pub async fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let json = tokio::fs::read_to_string(path).await?;

        let session: Self = serde_json::from_str(&json)?;

        // Check version compatibility
        // For now, we require exact major.minor version match
        let current_parts: Vec<&str> = ERYX_VERSION.split('.').collect();
        let stored_parts: Vec<&str> = session.metadata.eryx_version.split('.').collect();

        if current_parts.len() >= 2
            && stored_parts.len() >= 2
            && (current_parts[0] != stored_parts[0] || current_parts[1] != stored_parts[1])
        {
            return Err(Error::Snapshot(format!(
                "Incompatible session version: session was created with eryx {}, but current version is {}",
                session.metadata.eryx_version, ERYX_VERSION
            )));
        }

        tracing::debug!(
            path = %path.display(),
            state_size = session.state.len(),
            version = %session.metadata.eryx_version,
            "Session loaded from disk"
        );

        Ok(session)
    }

    /// Check if this session is compatible with the current eryx version.
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        let current_parts: Vec<&str> = ERYX_VERSION.split('.').collect();
        let stored_parts: Vec<&str> = self.metadata.eryx_version.split('.').collect();

        current_parts.len() >= 2
            && stored_parts.len() >= 2
            && current_parts[0] == stored_parts[0]
            && current_parts[1] == stored_parts[1]
    }
}

/// Serde module for base64 encoding/decoding of byte vectors.
mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    const ENGINE: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use base64::Engine;
        ENGINE.encode(bytes).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        use base64::Engine;
        let s = String::deserialize(deserializer)?;
        ENGINE.decode(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_persisted_session_roundtrip() {
        let state = vec![1, 2, 3, 4, 5];
        let session = PersistedSession::new(state.clone(), 10, None);

        // Serialize to JSON
        let json = serde_json::to_string(&session).expect("Failed to serialize");

        // Deserialize back
        let loaded: PersistedSession = serde_json::from_str(&json).expect("Failed to deserialize");

        assert_eq!(loaded.state, state);
        assert_eq!(loaded.metadata.execution_count, 10);
        assert_eq!(loaded.metadata.eryx_version, ERYX_VERSION);
    }

    #[test]
    fn test_serializable_system_time() {
        let now = SystemTime::now();
        let serializable: SerializableSystemTime = now.into();
        let back: SystemTime = serializable.into();

        // Should be within a second due to potential rounding
        let diff = now.duration_since(back).unwrap_or_default();
        assert!(diff.as_secs() == 0);
    }

    #[test]
    fn test_is_compatible() {
        let session = PersistedSession::new(vec![], 0, None);
        assert!(session.is_compatible());
    }
}
