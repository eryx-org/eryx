//! Python linting and type checking for eryx.
//!
//! Uses ruff's Python parser for syntax validation and ty for type checking.
//! All crates are pinned to the ruff commit used by ty 0.0.29.

mod stubs;
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

/// A supporting file to include in the type checking environment.
#[derive(Debug, Clone)]
pub struct SupportingFile {
    /// Filename (e.g. "helpers.py"). Must not contain path separators.
    pub name: String,
    /// File content.
    pub content: String,
}

/// Declaration of a callback function available at runtime.
///
/// Used to generate `.pyi` stubs so the type checker can validate
/// calls to callback functions.
#[derive(Debug, Clone)]
pub struct CallbackDeclaration {
    /// Function name (e.g. "query_loki").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Parameter declarations.
    pub parameters: Vec<ParameterDeclaration>,
}

/// A single parameter in a callback declaration.
#[derive(Debug, Clone)]
pub struct ParameterDeclaration {
    /// Parameter name.
    pub name: String,
    /// JSON Schema type: "string", "number", "integer", "boolean", "object", "array".
    pub json_type: String,
    /// Whether this parameter is required.
    pub required: bool,
}

/// Options for type checking.
#[derive(Debug, Default)]
pub struct CheckOptions {
    /// Supporting module files (importable by the main script).
    pub files: Vec<SupportingFile>,
    /// Callback declarations to generate type stubs for.
    pub callbacks: Vec<CallbackDeclaration>,
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
    check_types_with_options(source, CheckOptions::default())
}

/// Check Python source code with supporting files and callback stubs.
///
/// Extends [`check_types`] to support:
/// - Supporting module files importable from the main script
/// - Callback declarations that generate `.pyi` type stubs
///
/// Returns syntax and type diagnostics for the main script only.
pub fn check_types_with_options(
    source: &str,
    options: CheckOptions,
) -> anyhow::Result<Vec<Diagnostic>> {
    tycheck::check_with_options(source, &options)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

    #[test]
    fn check_with_supporting_module() {
        let options = CheckOptions {
            files: vec![SupportingFile {
                name: "helpers.py".to_string(),
                content: "def add(a: int, b: int) -> int:\n    return a + b\n".to_string(),
            }],
            callbacks: vec![],
        };
        let diagnostics =
            check_types_with_options("from helpers import add\nx: int = add(1, 2)\n", options)
                .unwrap();
        let type_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.source == Source::Type)
            .collect();
        assert!(
            type_errors.is_empty(),
            "expected no type errors, got: {type_errors:?}"
        );
    }

    #[test]
    fn check_with_callback_correct_usage() {
        let options = CheckOptions {
            files: vec![],
            callbacks: vec![CallbackDeclaration {
                name: "query_loki".to_string(),
                description: "Query Loki".to_string(),
                parameters: vec![ParameterDeclaration {
                    name: "expr".to_string(),
                    json_type: "string".to_string(),
                    required: true,
                }],
            }],
        };
        let diagnostics = check_types_with_options(
            "async def main():\n    result = await query_loki(expr='up{job=\"foo\"}')\n",
            options,
        )
        .unwrap();
        let type_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.source == Source::Type)
            .collect();
        assert!(
            type_errors.is_empty(),
            "expected no type errors, got: {type_errors:?}"
        );
    }

    #[test]
    fn check_with_callback_wrong_arg_type() {
        let options = CheckOptions {
            files: vec![],
            callbacks: vec![CallbackDeclaration {
                name: "query_loki".to_string(),
                description: "Query Loki".to_string(),
                parameters: vec![ParameterDeclaration {
                    name: "expr".to_string(),
                    json_type: "string".to_string(),
                    required: true,
                }],
            }],
        };
        let diagnostics = check_types_with_options(
            "async def main():\n    result = await query_loki(expr=42)\n",
            options,
        )
        .unwrap();
        assert!(
            diagnostics.iter().any(|d| d.source == Source::Type),
            "expected type error for int passed as str, got: {diagnostics:?}"
        );
    }

    #[test]
    fn check_offset_adjustment_with_callbacks() {
        let source = "x: int = 'hello'\n";
        // Without callbacks, get the raw offsets.
        let raw = check_types(source).unwrap();
        // With callbacks, offsets should still match (adjusted for prepended import).
        let options = CheckOptions {
            files: vec![],
            callbacks: vec![CallbackDeclaration {
                name: "noop".to_string(),
                description: String::new(),
                parameters: vec![],
            }],
        };
        let adjusted = check_types_with_options(source, options).unwrap();
        assert!(!raw.is_empty(), "expected raw diagnostics");
        assert!(!adjusted.is_empty(), "expected adjusted diagnostics");
        assert_eq!(
            raw[0].start_offset, adjusted[0].start_offset,
            "offsets should match after adjustment"
        );
    }
}
