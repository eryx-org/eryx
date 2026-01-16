//! Integration tests for session persistence.
//!
//! Tests save/load functionality and SessionRegistry.
//!
//! ## Running Tests
//!
//! Use `mise run test` which automatically handles precompilation:
//! ```sh
//! mise run setup  # One-time: build WASM + precompile
//! mise run test   # Run tests with precompiled WASM
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::OnceLock;

use eryx::{InProcessSession, PersistedSession, Sandbox, Session, SessionInfo, SessionRegistry};
use tempfile::TempDir;

/// Shared sandbox to avoid repeated WASM loading across tests.
static SHARED_SANDBOX: OnceLock<Sandbox> = OnceLock::new();

fn get_shared_sandbox() -> &'static Sandbox {
    SHARED_SANDBOX.get_or_init(|| {
        #[cfg(feature = "embedded")]
        {
            Sandbox::embedded()
                .build()
                .expect("Failed to create embedded sandbox")
        }
        #[cfg(not(feature = "embedded"))]
        {
            panic!("Tests require the 'embedded' feature. Run with: cargo test --features embedded")
        }
    })
}

// =============================================================================
// PersistedSession Tests
// =============================================================================

#[test]
fn test_persisted_session_serialization() {
    let state = vec![1, 2, 3, 4, 5, 255, 0, 128];
    let session = PersistedSession::new(state.clone(), 42, None);

    // Verify fields
    assert_eq!(session.state, state);
    assert_eq!(session.metadata.execution_count, 42);
    assert!(!session.metadata.eryx_version.is_empty());

    // Serialize to JSON
    let json = serde_json::to_string(&session).expect("Failed to serialize");
    assert!(json.contains("\"execution_count\":42"));

    // Deserialize back
    let loaded: PersistedSession = serde_json::from_str(&json).expect("Failed to deserialize");
    assert_eq!(loaded.state, state);
    assert_eq!(loaded.metadata.execution_count, 42);
}

#[tokio::test]
async fn test_persisted_session_file_roundtrip() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path = temp_dir.path().join("test.session");

    let state = vec![10, 20, 30, 40, 50];
    let session = PersistedSession::new(state.clone(), 100, None);

    // Save to file
    session.save(&path).await.expect("Failed to save");
    assert!(path.exists());

    // Load from file
    let loaded = PersistedSession::load(&path).await.expect("Failed to load");
    assert_eq!(loaded.state, state);
    assert_eq!(loaded.metadata.execution_count, 100);
}

// =============================================================================
// InProcessSession Save/Load Tests
// =============================================================================

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_session_save_and_load() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path = temp_dir.path().join("session.session");
    let sandbox = get_shared_sandbox();

    // Create a session and set some state
    let mut session = InProcessSession::new(sandbox)
        .await
        .expect("Failed to create session");
    session
        .execute("x = 42")
        .await
        .expect("Failed to execute x = 42");
    session
        .execute("y = 'hello'")
        .await
        .expect("Failed to execute y = 'hello'");
    session
        .execute("data = [1, 2, 3]")
        .await
        .expect("Failed to execute data = [1, 2, 3]");

    // Save the session
    session.save(&path).await.expect("Failed to save session");
    assert!(path.exists());

    // Drop the original session
    drop(session);

    // Load the session in a new instance
    let mut loaded_session = InProcessSession::load(sandbox, &path)
        .await
        .expect("Failed to load session");

    // Verify state was restored
    let result = loaded_session
        .execute("print(x)")
        .await
        .expect("Failed to print x");
    assert_eq!(result.stdout, "42");

    let result = loaded_session
        .execute("print(y)")
        .await
        .expect("Failed to print y");
    assert_eq!(result.stdout, "hello");

    let result = loaded_session
        .execute("print(data)")
        .await
        .expect("Failed to print data");
    assert_eq!(result.stdout, "[1, 2, 3]");
}

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_session_save_load_with_functions() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let path = temp_dir.path().join("functions.session");
    let sandbox = get_shared_sandbox();

    // Create a session and define a function
    let mut session = InProcessSession::new(sandbox)
        .await
        .expect("Failed to create session");
    session
        .execute(
            r#"
def add(a, b):
    return a + b

result = add(10, 20)
"#,
        )
        .await
        .expect("Failed to define function");

    // Save and reload
    session.save(&path).await.expect("Failed to save");
    drop(session);

    let mut loaded = InProcessSession::load(sandbox, &path)
        .await
        .expect("Failed to load");

    // Result should persist (functions may or may not depending on pickle)
    let output = loaded
        .execute("print(result)")
        .await
        .expect("Failed to print result");
    assert_eq!(output.stdout, "30");
}

// =============================================================================
// SessionRegistry Tests
// =============================================================================

#[test]
fn test_registry_new() {
    let registry = SessionRegistry::new("/tmp/test-sessions");
    assert_eq!(
        registry.base_path(),
        std::path::Path::new("/tmp/test-sessions")
    );
}

#[test]
fn test_registry_exists_nonexistent() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    assert!(!registry.exists("nonexistent"));
}

#[test]
fn test_registry_list_empty() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    let sessions = registry.list().expect("Failed to list");
    assert!(sessions.is_empty());
}

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_registry_save_and_list() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    let sandbox = get_shared_sandbox();

    // Create and save a session
    let mut session = InProcessSession::new(sandbox)
        .await
        .expect("Failed to create session");
    session.execute("x = 100").await.expect("Failed to execute");

    registry
        .save("test-session", &mut session)
        .await
        .expect("Failed to save");

    // Check it exists
    assert!(registry.exists("test-session"));

    // List sessions
    let sessions = registry.list().expect("Failed to list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].name, "test-session");
    assert!(sessions[0].execution_count > 0);
}

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_registry_get_or_create_new() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    let sandbox = get_shared_sandbox();

    // Get or create should create a new session
    let mut session = registry
        .get_or_create("new-session", sandbox)
        .await
        .expect("Failed to get_or_create");

    // Should be a fresh session
    session.execute("x = 1").await.expect("Failed to execute");
    let result = session.execute("print(x)").await.expect("Failed to print");
    assert_eq!(result.stdout, "1");
}

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_registry_get_or_create_existing() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    let sandbox = get_shared_sandbox();

    // Create and save a session with state
    let mut session = InProcessSession::new(sandbox)
        .await
        .expect("Failed to create session");
    session
        .execute("saved_value = 999")
        .await
        .expect("Failed to execute");
    registry
        .save("existing", &mut session)
        .await
        .expect("Failed to save");
    drop(session);

    // Get or create should load the existing session
    let mut loaded = registry
        .get_or_create("existing", sandbox)
        .await
        .expect("Failed to get_or_create");

    // State should be restored
    let result = loaded
        .execute("print(saved_value)")
        .await
        .expect("Failed to print");
    assert_eq!(result.stdout, "999");
}

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_registry_delete() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    let sandbox = get_shared_sandbox();

    // Create and save a session
    let mut session = InProcessSession::new(sandbox)
        .await
        .expect("Failed to create session");
    registry
        .save("to-delete", &mut session)
        .await
        .expect("Failed to save");

    assert!(registry.exists("to-delete"));

    // Delete it
    registry.delete("to-delete").expect("Failed to delete");

    assert!(!registry.exists("to-delete"));
}

#[tokio::test]
async fn test_registry_delete_nonexistent() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());

    // Should fail for nonexistent session
    let result = registry.delete("nonexistent");
    assert!(result.is_err());
}

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_registry_clear() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    let sandbox = get_shared_sandbox();

    // Create and save multiple sessions
    for name in ["session1", "session2", "session3"] {
        let mut session = InProcessSession::new(sandbox)
            .await
            .expect("Failed to create session");
        registry
            .save(name, &mut session)
            .await
            .expect("Failed to save");
    }

    assert_eq!(registry.list().expect("Failed to list").len(), 3);

    // Clear all sessions
    let count = registry.clear().expect("Failed to clear");
    assert_eq!(count, 3);

    assert!(registry.list().expect("Failed to list").is_empty());
}

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_registry_load_nonexistent() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    let sandbox = get_shared_sandbox();

    // Should fail for nonexistent session
    let result = registry.load("nonexistent", sandbox).await;
    assert!(result.is_err());
}

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_registry_multiple_sessions() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    let sandbox = get_shared_sandbox();

    // Create sessions with different values
    for (name, value) in [("alpha", 1), ("beta", 2), ("gamma", 3)] {
        let mut session = InProcessSession::new(sandbox)
            .await
            .expect("Failed to create session");
        session
            .execute(&format!("value = {value}"))
            .await
            .expect("Failed to execute");
        registry
            .save(name, &mut session)
            .await
            .expect("Failed to save");
    }

    // Load each and verify values
    for (name, expected) in [("alpha", "1"), ("beta", "2"), ("gamma", "3")] {
        let mut session = registry.load(name, sandbox).await.expect("Failed to load");
        let result = session
            .execute("print(value)")
            .await
            .expect("Failed to print");
        assert_eq!(result.stdout, expected, "Session {name} has wrong value");
    }
}

// =============================================================================
// SessionInfo Tests
// =============================================================================

#[tokio::test]
#[cfg(feature = "embedded")]
async fn test_session_info_fields() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let registry = SessionRegistry::new(temp_dir.path());
    let sandbox = get_shared_sandbox();

    // Create a session with some executions
    let mut session = InProcessSession::new(sandbox)
        .await
        .expect("Failed to create session");
    session.execute("x = 1").await.expect("Failed to execute");
    session.execute("x = 2").await.expect("Failed to execute");
    session.execute("x = 3").await.expect("Failed to execute");

    registry
        .save("info-test", &mut session)
        .await
        .expect("Failed to save");

    // Get session info
    let sessions = registry.list().expect("Failed to list");
    assert_eq!(sessions.len(), 1);

    let info: &SessionInfo = &sessions[0];
    assert_eq!(info.name, "info-test");
    assert!(info.execution_count >= 3);

    // Timestamps should be reasonable (within the last minute)
    let now = std::time::SystemTime::now();
    let one_minute_ago = now - std::time::Duration::from_secs(60);
    assert!(info.created_at > one_minute_ago);
    assert!(info.last_active > one_minute_ago);
}
