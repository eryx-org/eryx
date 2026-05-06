//! Generate Python type stubs (`.pyi`) from callback declarations.
//!
//! Each callback becomes an `async def` at module level so the ty type
//! checker can validate calls like `await query_loki(expr="...")`.

use crate::{CallbackDeclaration, ParameterDeclaration};

/// Map a JSON Schema type string to a Python type annotation.
fn json_type_to_python(json_type: &str) -> &str {
    match json_type {
        "string" => "str",
        "number" => "float",
        "integer" => "int",
        "boolean" => "bool",
        "object" => "dict[str, Any]",
        "array" => "list[Any]",
        _ => "Any",
    }
}

/// Generate the content of a `.pyi` stub file for callback declarations.
///
/// Returns an empty string if `callbacks` is empty. Otherwise produces a
/// complete `.pyi` file with one `async def` per callback.
///
/// Required parameters come before optional ones. Optional parameters
/// use `T | None = None` syntax.
pub(crate) fn generate_callback_stubs(callbacks: &[CallbackDeclaration]) -> String {
    if callbacks.is_empty() {
        return String::new();
    }

    let mut out = String::from("from typing import Any\n");

    for cb in callbacks {
        out.push('\n');
        out.push_str("async def ");
        out.push_str(&cb.name);
        out.push('(');

        // Required params first, then optional.
        let required: Vec<_> = cb.parameters.iter().filter(|p| p.required).collect();
        let optional: Vec<_> = cb.parameters.iter().filter(|p| !p.required).collect();

        let mut first = true;
        for param in &required {
            if !first {
                out.push_str(", ");
            }
            first = false;
            write_param(&mut out, param, true);
        }
        for param in &optional {
            if !first {
                out.push_str(", ");
            }
            first = false;
            write_param(&mut out, param, false);
        }

        out.push_str(") -> Any: ...\n");
    }

    out
}

/// Write a single parameter annotation to the output string.
fn write_param(out: &mut String, param: &ParameterDeclaration, required: bool) {
    let py_type = json_type_to_python(&param.json_type);
    out.push_str(&param.name);
    out.push_str(": ");
    if required {
        out.push_str(py_type);
    } else {
        out.push_str(py_type);
        out.push_str(" | None = None");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{CallbackDeclaration, ParameterDeclaration};

    fn make_param(name: &str, json_type: &str, required: bool) -> ParameterDeclaration {
        ParameterDeclaration {
            name: name.to_string(),
            json_type: json_type.to_string(),
            required,
        }
    }

    fn make_callback(name: &str, params: Vec<ParameterDeclaration>) -> CallbackDeclaration {
        CallbackDeclaration {
            name: name.to_string(),
            description: String::new(),
            parameters: params,
        }
    }

    #[test]
    fn empty_callbacks_returns_empty_string() {
        assert_eq!(generate_callback_stubs(&[]), "");
    }

    #[test]
    fn single_callback_no_params() {
        let stubs = generate_callback_stubs(&[make_callback("ping", vec![])]);
        assert_eq!(
            stubs,
            "from typing import Any\n\nasync def ping() -> Any: ...\n"
        );
    }

    #[test]
    fn all_json_types_mapped() {
        let params = vec![
            make_param("a", "string", true),
            make_param("b", "number", true),
            make_param("c", "integer", true),
            make_param("d", "boolean", true),
            make_param("e", "object", true),
            make_param("f", "array", true),
        ];
        let stubs = generate_callback_stubs(&[make_callback("test", params)]);
        assert!(
            stubs.contains("a: str, b: float, c: int, d: bool, e: dict[str, Any], f: list[Any]")
        );
    }

    #[test]
    fn unknown_type_maps_to_any() {
        let params = vec![make_param("x", "unknown_type", true)];
        let stubs = generate_callback_stubs(&[make_callback("test", params)]);
        assert!(stubs.contains("x: Any"));
    }

    #[test]
    fn optional_params_have_none_default() {
        let params = vec![make_param("limit", "integer", false)];
        let stubs = generate_callback_stubs(&[make_callback("query", params)]);
        assert!(stubs.contains("limit: int | None = None"));
    }

    #[test]
    fn required_before_optional() {
        let params = vec![
            make_param("opt", "string", false),
            make_param("req", "string", true),
        ];
        let stubs = generate_callback_stubs(&[make_callback("test", params)]);
        // Required param should come first even though optional was declared first.
        let def_line = stubs.lines().find(|l| l.starts_with("async def")).unwrap();
        let req_pos = def_line.find("req: str").unwrap();
        let opt_pos = def_line.find("opt: str").unwrap();
        assert!(
            req_pos < opt_pos,
            "required param should come before optional"
        );
    }

    #[test]
    fn multiple_callbacks() {
        let stubs = generate_callback_stubs(&[
            make_callback("foo", vec![]),
            make_callback("bar", vec![make_param("x", "string", true)]),
        ]);
        assert!(stubs.contains("async def foo() -> Any: ..."));
        assert!(stubs.contains("async def bar(x: str) -> Any: ..."));
    }
}
