//! Session registry for managing named persistent sessions.
//!
//! The [`SessionRegistry`] provides a high-level interface for working with
//! named sessions that persist to disk. Sessions are stored in a directory
//! and can be listed, retrieved, and deleted by name.
//!
//! ## Example
//!
//! ```rust,ignore
//! use eryx::session::{SessionRegistry, Session};
//!
//! // Create a registry in a directory
//! let registry = SessionRegistry::new("/tmp/sessions");
//!
//! // Get or create a named session
//! let mut session = registry.get_or_create("my-session", &sandbox).await?;
//! session.execute("x = 42").await?;
//!
//! // Save the session back to the registry
//! registry.save("my-session", &mut session).await?;
//!
//! // List all sessions
//! for info in registry.list()? {
//!     println!("{}: {} executions", info.name, info.execution_count);
//! }
//!
//! // Delete a session
//! registry.delete("my-session")?;
//! ```

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::Error;
use crate::sandbox::Sandbox;

use super::InProcessSession;
use super::persistence::PersistedSession;

/// File extension for session files.
const SESSION_EXTENSION: &str = "session";

/// A registry for managing named persistent sessions.
///
/// The registry stores sessions as files in a base directory, with each
/// session named `{name}.session`. This provides a simple way to manage
/// multiple persistent sessions by name.
#[derive(Debug, Clone)]
pub struct SessionRegistry {
    /// The base directory where sessions are stored.
    base_path: PathBuf,
}

impl SessionRegistry {
    /// Create a new session registry.
    ///
    /// The registry will store sessions in the specified directory.
    /// The directory will be created if it doesn't exist when sessions
    /// are saved.
    ///
    /// # Arguments
    ///
    /// * `base_path` - The directory to store session files in
    #[must_use]
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
        }
    }

    /// Get the base path where sessions are stored.
    #[must_use]
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// Get the file path for a named session.
    fn session_path(&self, name: &str) -> PathBuf {
        self.base_path.join(format!("{name}.{SESSION_EXTENSION}"))
    }

    /// Check if a named session exists.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the session to check
    #[must_use]
    pub fn exists(&self, name: &str) -> bool {
        self.session_path(name).exists()
    }

    /// Get or create a named session.
    ///
    /// If a session with the given name exists, it is loaded from disk.
    /// Otherwise, a new session is created.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the session
    /// * `sandbox` - The sandbox to create the session in
    ///
    /// # Errors
    ///
    /// Returns an error if the session cannot be loaded or created.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut session = registry.get_or_create("my-session", &sandbox).await?;
    /// session.execute("print('hello')").await?;
    /// ```
    pub async fn get_or_create<'a>(
        &self,
        name: &str,
        sandbox: &'a Sandbox,
    ) -> Result<InProcessSession<'a>, Error> {
        let path = self.session_path(name);

        if path.exists() {
            tracing::debug!(name = %name, "Loading existing session from registry");
            InProcessSession::load(sandbox, &path).await
        } else {
            tracing::debug!(name = %name, "Creating new session in registry");
            InProcessSession::new(sandbox).await
        }
    }

    /// Load a named session from the registry.
    ///
    /// Unlike [`get_or_create`](Self::get_or_create), this method returns
    /// an error if the session doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the session to load
    /// * `sandbox` - The sandbox to create the session in
    ///
    /// # Errors
    ///
    /// Returns an error if the session doesn't exist or cannot be loaded.
    pub async fn load<'a>(
        &self,
        name: &str,
        sandbox: &'a Sandbox,
    ) -> Result<InProcessSession<'a>, Error> {
        let path = self.session_path(name);

        if !path.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Session '{name}' not found in registry"),
            )));
        }

        InProcessSession::load(sandbox, &path).await
    }

    /// Save a session to the registry.
    ///
    /// This saves the current session state to a file with the given name.
    /// If a session with that name already exists, it is overwritten.
    ///
    /// # Arguments
    ///
    /// * `name` - The name to save the session as
    /// * `session` - The session to save
    ///
    /// # Errors
    ///
    /// Returns an error if the session cannot be saved.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// session.execute("x = 42").await?;
    /// registry.save("my-session", &mut session).await?;
    /// ```
    pub async fn save(&self, name: &str, session: &mut InProcessSession<'_>) -> Result<(), Error> {
        // Ensure the base directory exists
        tokio::fs::create_dir_all(&self.base_path).await?;

        let path = self.session_path(name);
        session.save(&path).await
    }

    /// Save a session to the registry, preserving the original creation time.
    ///
    /// This is useful when re-saving a session that was previously loaded,
    /// to preserve the original creation timestamp.
    ///
    /// # Arguments
    ///
    /// * `name` - The name to save the session as
    /// * `session` - The session to save
    /// * `created_at` - The original creation time to preserve
    ///
    /// # Errors
    ///
    /// Returns an error if the session cannot be saved.
    pub async fn save_with_created_at(
        &self,
        name: &str,
        session: &mut InProcessSession<'_>,
        created_at: SystemTime,
    ) -> Result<(), Error> {
        // Ensure the base directory exists
        tokio::fs::create_dir_all(&self.base_path).await?;

        let path = self.session_path(name);
        session.save_with_created_at(&path, created_at).await
    }

    /// List all sessions in the registry.
    ///
    /// Returns information about each session, including name, creation time,
    /// last active time, and execution count.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be read or session files
    /// cannot be parsed.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// for info in registry.list()? {
    ///     println!("{}: {} executions", info.name, info.execution_count);
    /// }
    /// ```
    pub fn list(&self) -> Result<Vec<SessionInfo>, Error> {
        let mut sessions = Vec::new();

        // If the directory doesn't exist, return empty list
        if !self.base_path.exists() {
            return Ok(sessions);
        }

        let entries = std::fs::read_dir(&self.base_path)?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            // Skip non-session files
            if path.extension().and_then(|e| e.to_str()) != Some(SESSION_EXTENSION) {
                continue;
            }

            // Extract session name from filename
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };

            // Try to read session metadata
            // Use blocking I/O since we're in a sync function
            match std::fs::read_to_string(&path) {
                Ok(json) => {
                    if let Ok(persisted) = serde_json::from_str::<PersistedSession>(&json) {
                        sessions.push(SessionInfo {
                            name,
                            created_at: persisted.created_at.into(),
                            last_active: persisted.last_active.into(),
                            execution_count: persisted.metadata.execution_count,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to read session file"
                    );
                }
            }
        }

        // Sort by last_active (most recent first)
        sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));

        Ok(sessions)
    }

    /// Delete a session from the registry.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the session to delete
    ///
    /// # Errors
    ///
    /// Returns an error if the session doesn't exist or cannot be deleted.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// registry.delete("my-session")?;
    /// ```
    pub fn delete(&self, name: &str) -> Result<(), Error> {
        let path = self.session_path(name);

        if !path.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Session '{name}' not found in registry"),
            )));
        }

        std::fs::remove_file(&path)?;

        tracing::debug!(name = %name, "Session deleted from registry");

        Ok(())
    }

    /// Delete all sessions from the registry.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be read or files cannot be deleted.
    pub fn clear(&self) -> Result<usize, Error> {
        if !self.base_path.exists() {
            return Ok(0);
        }

        let mut count = 0;
        let entries = std::fs::read_dir(&self.base_path)?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) == Some(SESSION_EXTENSION) {
                std::fs::remove_file(&path)?;
                count += 1;
            }
        }

        tracing::debug!(count = count, "Cleared all sessions from registry");

        Ok(count)
    }
}

/// Information about a saved session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The name of the session (without file extension).
    pub name: String,

    /// When the session was first created.
    pub created_at: SystemTime,

    /// When the session was last active (last execution or save).
    pub last_active: SystemTime,

    /// Number of executions performed in the session.
    pub execution_count: u32,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_session_path() {
        let registry = SessionRegistry::new("/tmp/sessions");
        assert_eq!(
            registry.session_path("test"),
            PathBuf::from("/tmp/sessions/test.session")
        );
    }

    #[test]
    fn test_exists_nonexistent() {
        let registry = SessionRegistry::new("/tmp/nonexistent-eryx-sessions");
        assert!(!registry.exists("test"));
    }

    #[test]
    fn test_list_empty_directory() {
        let registry = SessionRegistry::new("/tmp/nonexistent-eryx-sessions");
        let sessions = registry.list().expect("list should work");
        assert!(sessions.is_empty());
    }
}
