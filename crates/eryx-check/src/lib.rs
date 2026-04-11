//! Python linting and type checking for eryx.
//!
//! Uses ruff's Python parser for syntax validation and ty for type checking.
//! All crates are pinned to the ruff commit used by ty 0.0.29.

mod tycheck;

use ruff_python_parser::{Mode, parse_unchecked};

/// A diagnostic from checking Python source code.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Error message.
    pub message: String,
    /// Severity level.
    pub severity: Severity,
    /// Source of the diagnostic.
    pub source: Source,
    /// Byte offset of the error start.
    pub start_offset: u32,
    /// Byte offset of the error end.
    pub end_offset: u32,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A syntax or type error.
    Error,
    /// A warning from the type checker.
    Warning,
    /// Informational diagnostic.
    Info,
}

/// Source of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// From the Python parser (syntax errors).
    Syntax,
    /// From the ty type checker.
    Type,
}

/// Check Python source code for syntax errors.
///
/// Returns an empty vec if the code is syntactically valid.
pub fn check_syntax(source: &str) -> Vec<Diagnostic> {
    let parsed = parse_unchecked(source, Mode::Module.into());
    parsed
        .errors()
        .iter()
        .map(|err| Diagnostic {
            message: err.error.to_string(),
            severity: Severity::Error,
            source: Source::Syntax,
            start_offset: err.location.start().into(),
            end_offset: err.location.end().into(),
        })
        .collect()
}

/// Check Python source code for type errors.
///
/// This runs the ty type checker on the given source code. It also
/// includes syntax errors if any are found.
pub fn check_types(source: &str) -> anyhow::Result<Vec<Diagnostic>> {
    tycheck::check(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_syntax() {
        let diagnostics = check_syntax("x = 1\nprint(x)\n");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn invalid_syntax() {
        let diagnostics = check_syntax("def foo(\n");
        assert!(!diagnostics.is_empty());
        assert_eq!(diagnostics[0].source, Source::Syntax);
    }

    #[test]
    fn type_error_detected() {
        let diagnostics = check_types("x: int = 'hello'\n").unwrap();
        assert!(
            diagnostics.iter().any(|d| d.source == Source::Type),
            "expected type error, got: {diagnostics:?}"
        );
    }

    #[test]
    fn clean_code_no_type_errors() {
        let diagnostics = check_types("x: int = 42\nprint(x)\n").unwrap();
        let type_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.source == Source::Type)
            .collect();
        assert!(
            type_errors.is_empty(),
            "expected no type errors, got: {type_errors:?}"
        );
    }
}
