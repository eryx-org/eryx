//! Guest language selection for the sandbox.
//!
//! Eryx began as a Python-only sandbox (CPython compiled to a WebAssembly
//! component). The [`Language`] enum is the seam that lets a caller pick which
//! guest language an [`Executor`](crate::Executor) runs. Today only
//! [`Language::Python`] has a working execution path; [`Language::JavaScript`]
//! (QuickJS) is reserved for an upcoming release and currently has no guest.

/// The guest language a sandbox executes.
///
/// Threaded through [`Executor`](crate::Executor) /
/// [`SandboxBuilder`](crate::SandboxBuilder) construction so a single host API
/// can target multiple guest runtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Language {
    /// CPython, compiled to a WebAssembly component. This is the only language
    /// with a working execution path today, and the default.
    #[default]
    Python,

    /// JavaScript via QuickJS.
    ///
    /// SPIKE: the JS guest does not exist yet — selecting this language builds
    /// fine but execution will `todo!()`. Reserved for the 0.6.0 release.
    JavaScript,
}

impl Language {
    /// A short, lowercase identifier for the language (e.g. `"python"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::JavaScript => "javascript",
        }
    }

    /// Whether this language currently has a working guest runtime.
    ///
    /// SPIKE: only Python is implemented today.
    #[must_use]
    pub const fn is_implemented(self) -> bool {
        matches!(self, Self::Python)
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_python() {
        assert_eq!(Language::default(), Language::Python);
    }

    #[test]
    fn only_python_is_implemented() {
        assert!(Language::Python.is_implemented());
        assert!(!Language::JavaScript.is_implemented());
    }

    #[test]
    fn display_and_as_str_agree() {
        assert_eq!(Language::Python.as_str(), "python");
        assert_eq!(Language::JavaScript.to_string(), "javascript");
    }
}
