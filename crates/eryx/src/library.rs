//! Runtime library for composable callbacks with Python wrappers and type stubs.

use std::fmt;

use crate::callback::Callback;

/// A composable set of callbacks with Python wrappers and type stubs.
///
/// Runtime libraries bundle together:
/// - Callbacks that Python code can invoke
/// - Python preamble code (wrapper classes, helpers, etc.)
/// - Type stubs (.pyi content) for LLM context windows
#[derive(Default)]
pub struct RuntimeLibrary {
    /// Callbacks provided by this library.
    pub callbacks: Vec<Box<dyn Callback>>,

    /// Python code injected before user code (wrapper classes, etc.).
    pub python_preamble: String,

    /// Type stubs (.pyi content) for LLM context.
    pub type_stubs: String,
}

impl RuntimeLibrary {
    /// Create a new empty runtime library.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a callback to this library.
    #[must_use]
    pub fn with_callback<C: Callback + 'static>(mut self, callback: C) -> Self {
        self.callbacks.push(Box::new(callback));
        self
    }

    /// Add multiple callbacks to this library.
    #[must_use]
    pub fn with_callbacks(mut self, callbacks: Vec<Box<dyn Callback>>) -> Self {
        self.callbacks.extend(callbacks);
        self
    }

    /// Set the Python preamble code.
    #[must_use]
    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        self.python_preamble = preamble.into();
        self
    }

    /// Set the type stubs content.
    #[must_use]
    pub fn with_stubs(mut self, stubs: impl Into<String>) -> Self {
        self.type_stubs = stubs.into();
        self
    }

    /// Merge another library into this one.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        self.callbacks.extend(other.callbacks);

        if !other.python_preamble.is_empty() {
            if !self.python_preamble.is_empty() {
                self.python_preamble.push_str("\n\n");
            }
            self.python_preamble.push_str(&other.python_preamble);
        }

        if !other.type_stubs.is_empty() {
            if !self.type_stubs.is_empty() {
                self.type_stubs.push_str("\n\n");
            }
            self.type_stubs.push_str(&other.type_stubs);
        }

        self
    }
}

impl fmt::Debug for RuntimeLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeLibrary")
            .field(
                "callbacks",
                &format!("[{} callbacks]", self.callbacks.len()),
            )
            .field("python_preamble", &self.python_preamble)
            .field("type_stubs", &self.type_stubs)
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::callback::{CallbackError, TypedCallback};
    use crate::schema::JsonSchema;
    use serde::Deserialize;
    use serde_json::{Value, json};
    use std::future::Future;
    use std::pin::Pin;

    // Test callbacks for use in tests
    #[derive(Deserialize, JsonSchema)]
    struct EchoArgs {
        message: String,
    }

    struct EchoCallback;

    impl TypedCallback for EchoCallback {
        type Args = EchoArgs;

        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes the message"
        }

        fn invoke_typed(
            &self,
            args: EchoArgs,
        ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
            Box::pin(async move { Ok(json!({ "echoed": args.message })) })
        }
    }

    struct GetTimeCallback;

    impl TypedCallback for GetTimeCallback {
        type Args = ();

        fn name(&self) -> &str {
            "get_time"
        }

        fn description(&self) -> &str {
            "Returns the current time"
        }

        fn invoke_typed(
            &self,
            _args: (),
        ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
            Box::pin(async move { Ok(json!(12345)) })
        }
    }

    struct AddCallback;

    impl TypedCallback for AddCallback {
        type Args = ();

        fn name(&self) -> &str {
            "add"
        }

        fn description(&self) -> &str {
            "Adds numbers"
        }

        fn invoke_typed(
            &self,
            _args: (),
        ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
            Box::pin(async move { Ok(json!(42)) })
        }
    }

    #[test]
    fn new_creates_empty_library() {
        let lib = RuntimeLibrary::new();

        assert!(lib.callbacks.is_empty());
        assert!(lib.python_preamble.is_empty());
        assert!(lib.type_stubs.is_empty());
    }

    #[test]
    fn default_creates_empty_library() {
        let lib = RuntimeLibrary::default();

        assert!(lib.callbacks.is_empty());
        assert!(lib.python_preamble.is_empty());
        assert!(lib.type_stubs.is_empty());
    }

    #[test]
    fn with_callback_adds_single_callback() {
        let lib = RuntimeLibrary::new().with_callback(EchoCallback);

        assert_eq!(lib.callbacks.len(), 1);
        assert_eq!(lib.callbacks[0].name(), "echo");
    }

    #[test]
    fn with_callback_chains_multiple_callbacks() {
        let lib = RuntimeLibrary::new()
            .with_callback(EchoCallback)
            .with_callback(GetTimeCallback);

        assert_eq!(lib.callbacks.len(), 2);
        assert_eq!(lib.callbacks[0].name(), "echo");
        assert_eq!(lib.callbacks[1].name(), "get_time");
    }

    #[test]
    fn with_callbacks_adds_multiple_at_once() {
        let callbacks: Vec<Box<dyn Callback>> =
            vec![Box::new(EchoCallback), Box::new(GetTimeCallback)];

        let lib = RuntimeLibrary::new().with_callbacks(callbacks);

        assert_eq!(lib.callbacks.len(), 2);
    }

    #[test]
    fn with_preamble_sets_python_code() {
        let preamble = "import json\n\ndef helper(): pass";
        let lib = RuntimeLibrary::new().with_preamble(preamble);

        assert_eq!(lib.python_preamble, preamble);
    }

    #[test]
    fn with_preamble_accepts_string() {
        let lib = RuntimeLibrary::new().with_preamble(String::from("# Python code"));

        assert_eq!(lib.python_preamble, "# Python code");
    }

    #[test]
    fn with_stubs_sets_type_stubs() {
        let stubs = "def echo(message: str) -> dict: ...";
        let lib = RuntimeLibrary::new().with_stubs(stubs);

        assert_eq!(lib.type_stubs, stubs);
    }

    #[test]
    fn with_stubs_accepts_string() {
        let lib = RuntimeLibrary::new().with_stubs(String::from("# Type stubs"));

        assert_eq!(lib.type_stubs, "# Type stubs");
    }

    #[test]
    fn builder_pattern_chains_all_methods() {
        let lib = RuntimeLibrary::new()
            .with_callback(EchoCallback)
            .with_callback(GetTimeCallback)
            .with_preamble("# Preamble")
            .with_stubs("# Stubs");

        assert_eq!(lib.callbacks.len(), 2);
        assert_eq!(lib.python_preamble, "# Preamble");
        assert_eq!(lib.type_stubs, "# Stubs");
    }

    #[test]
    fn merge_combines_callbacks() {
        let lib1 = RuntimeLibrary::new().with_callback(EchoCallback);
        let lib2 = RuntimeLibrary::new().with_callback(GetTimeCallback);

        let merged = lib1.merge(lib2);

        assert_eq!(merged.callbacks.len(), 2);
        assert_eq!(merged.callbacks[0].name(), "echo");
        assert_eq!(merged.callbacks[1].name(), "get_time");
    }

    #[test]
    fn merge_combines_preambles_with_separator() {
        let lib1 = RuntimeLibrary::new().with_preamble("# Part 1");
        let lib2 = RuntimeLibrary::new().with_preamble("# Part 2");

        let merged = lib1.merge(lib2);

        assert!(merged.python_preamble.contains("# Part 1"));
        assert!(merged.python_preamble.contains("# Part 2"));
        assert!(merged.python_preamble.contains("\n\n"));
    }

    #[test]
    fn merge_combines_stubs_with_separator() {
        let lib1 = RuntimeLibrary::new().with_stubs("def foo(): ...");
        let lib2 = RuntimeLibrary::new().with_stubs("def bar(): ...");

        let merged = lib1.merge(lib2);

        assert!(merged.type_stubs.contains("def foo(): ..."));
        assert!(merged.type_stubs.contains("def bar(): ..."));
        assert!(merged.type_stubs.contains("\n\n"));
    }

    #[test]
    fn merge_empty_preamble_no_extra_newlines() {
        let lib1 = RuntimeLibrary::new().with_preamble("# Only preamble");
        let lib2 = RuntimeLibrary::new(); // Empty preamble

        let merged = lib1.merge(lib2);

        assert_eq!(merged.python_preamble, "# Only preamble");
        assert!(!merged.python_preamble.ends_with("\n\n"));
    }

    #[test]
    fn merge_into_empty_preamble_no_extra_newlines() {
        let lib1 = RuntimeLibrary::new(); // Empty preamble
        let lib2 = RuntimeLibrary::new().with_preamble("# Only preamble");

        let merged = lib1.merge(lib2);

        assert_eq!(merged.python_preamble, "# Only preamble");
        assert!(!merged.python_preamble.starts_with("\n\n"));
    }

    #[test]
    fn merge_empty_stubs_no_extra_newlines() {
        let lib1 = RuntimeLibrary::new().with_stubs("# Only stubs");
        let lib2 = RuntimeLibrary::new(); // Empty stubs

        let merged = lib1.merge(lib2);

        assert_eq!(merged.type_stubs, "# Only stubs");
    }

    #[test]
    fn merge_both_empty_preambles() {
        let lib1 = RuntimeLibrary::new();
        let lib2 = RuntimeLibrary::new();

        let merged = lib1.merge(lib2);

        assert!(merged.python_preamble.is_empty());
    }

    #[test]
    fn merge_is_chainable() {
        let lib1 = RuntimeLibrary::new().with_callback(EchoCallback);
        let lib2 = RuntimeLibrary::new().with_callback(GetTimeCallback);
        let lib3 = RuntimeLibrary::new().with_callback(AddCallback);

        let merged = lib1.merge(lib2).merge(lib3);

        assert_eq!(merged.callbacks.len(), 3);
    }

    #[test]
    fn debug_format_shows_callback_count() {
        let lib = RuntimeLibrary::new()
            .with_callback(EchoCallback)
            .with_callback(GetTimeCallback);

        let debug = format!("{:?}", lib);

        assert!(debug.contains("RuntimeLibrary"));
        assert!(debug.contains("[2 callbacks]"));
    }

    #[test]
    fn debug_format_shows_preamble() {
        let lib = RuntimeLibrary::new().with_preamble("# Test preamble");

        let debug = format!("{:?}", lib);

        assert!(debug.contains("# Test preamble"));
    }

    #[test]
    fn debug_format_shows_stubs() {
        let lib = RuntimeLibrary::new().with_stubs("# Test stubs");

        let debug = format!("{:?}", lib);

        assert!(debug.contains("# Test stubs"));
    }

    #[test]
    fn empty_library_debug() {
        let lib = RuntimeLibrary::new();

        let debug = format!("{:?}", lib);

        assert!(debug.contains("[0 callbacks]"));
    }

    #[test]
    fn callbacks_are_accessible() {
        let lib = RuntimeLibrary::new()
            .with_callback(EchoCallback)
            .with_callback(GetTimeCallback);

        // Can iterate over callbacks
        let names: Vec<&str> = lib.callbacks.iter().map(|c| c.name()).collect();
        assert_eq!(names, vec!["echo", "get_time"]);
    }

    #[test]
    fn preamble_is_accessible() {
        let lib = RuntimeLibrary::new().with_preamble("test preamble");

        assert_eq!(lib.python_preamble, "test preamble");
    }

    #[test]
    fn stubs_are_accessible() {
        let lib = RuntimeLibrary::new().with_stubs("test stubs");

        assert_eq!(lib.type_stubs, "test stubs");
    }
}
