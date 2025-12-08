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
