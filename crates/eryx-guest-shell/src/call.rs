//! Host callbacks as shell commands: turning `NAME [ARGS]...` into the JSON
//! arguments object a callback receives, and its JSON result into stdout.
//!
//! ```text
//! NAME [--key value | --key=value | POSITIONAL]... [--json '{...}']
//! invoke NAME [same arguments]
//! invoke NAME '{...}'
//! ```
//!
//! - Flag values are typed by the callback's parameter schema: a `string`
//!   property keeps the exact text (`--id 007` stays `"007"`), `integer`,
//!   `number` and `boolean` are parsed, `object` and `array` are JSON. A
//!   property without a usable type, or not in the schema, is JSON if it
//!   parses and a string otherwise.
//! - A `boolean` property may be given bare (`--verbose`): it is `true` unless
//!   the next argument is `true` or `false`.
//! - Repeating a key collects into a list for an `array` property and is an
//!   error otherwise.
//! - `--json` gives a base object; flags and positionals override its keys.
//! - Positional arguments are sent as a list under `__eryx_args__`, and the
//!   host binds them to the schema's properties in order, with the same rules
//!   and messages as the Python guest's `name(a, b)`. Each one is typed by the
//!   property at its position, so `weather 007` sends `"007"` to a `string`
//!   first property. A name the guest has no schema for (under a prefix
//!   callback) types them loosely.
//! - `--` ends the flags; everything after it is positional.
//! - `invoke NAME '{...}'`, with a single JSON object argument, passes that
//!   object as is.
//!
//! On success the result goes to stdout: a string as raw text, `null` as
//! nothing, anything else as compact JSON, each followed by a newline. Exit
//! codes: 0 on success, 1 when the callback fails (`NAME: message` on
//! stderr), 2 for a usage error, 127 for `invoke` of an unknown name. Too many
//! positionals, or one for a key also given as a flag, is the host's error,
//! so it exits 1 like any other failed call.

use serde_json::{Map, Value};

/// The key the Python guest and the host use for positional arguments that
/// only the host can bind (names under a prefix callback).
pub const POSITIONAL_ARGS_KEY: &str = "__eryx_args__";

/// What the guest knows about one callback.
#[derive(Debug, Clone)]
pub struct CallbackDef {
    pub description: String,
    /// The parameters schema, if the host sent one.
    pub schema: Option<Value>,
}

/// A parsed command line: the call to make, or help to print.
#[derive(Debug, PartialEq)]
pub enum Call {
    Invoke(Value),
    Help,
}

/// Parse a callback command's arguments. `def` is `None` for a name the
/// guest only knows through a prefix callback.
pub fn parse(def: Option<&CallbackDef>, args: &[String]) -> Result<Call, String> {
    let schema = def.and_then(|d| d.schema.as_ref());
    let props = properties(schema);
    if args.len() == 1 && args[0] == "--help" && !props.iter().any(|(k, _)| k == "help") {
        return Ok(Call::Help);
    }

    let mut base: Option<Map<String, Value>> = None;
    let mut flags = Map::new();
    let mut positionals: Vec<&str> = Vec::new();
    let mut it = args.iter().peekable();
    let mut only_positionals = false;
    while let Some(arg) = it.next() {
        let key_value = arg
            .strip_prefix("--")
            .filter(|k| !k.is_empty() && !only_positionals);
        let Some(key_value) = key_value else {
            if arg == "--" && !only_positionals {
                only_positionals = true;
            } else {
                positionals.push(arg);
            }
            continue;
        };
        let (key, inline) = match key_value.split_once('=') {
            Some((k, v)) => (k, Some(v.to_owned())),
            None => (key_value, None),
        };
        if key == POSITIONAL_ARGS_KEY {
            return Err(format!("--{key} is reserved"));
        }
        let ty = prop_type(&props, key);
        let text = match inline {
            Some(v) => v,
            None if ty == Some("boolean") => match it.peek().map(|s| s.as_str()) {
                Some(v @ ("true" | "false")) => {
                    let v = v.to_owned();
                    it.next();
                    v
                }
                _ => "true".to_owned(),
            },
            None => it
                .next()
                .cloned()
                .ok_or_else(|| format!("--{key} needs a value"))?,
        };
        if key == "json" && ty.is_none() {
            if base.is_some() {
                return Err("--json given more than once".into());
            }
            match serde_json::from_str(&text) {
                Ok(Value::Object(o)) => base = Some(o),
                _ => return Err("--json needs a JSON object".into()),
            }
            continue;
        }
        let value = coerce(&props, key, &text)?;
        match flags.get_mut(key) {
            None => {
                flags.insert(key.to_owned(), value);
            }
            Some(Value::Array(items)) if ty == Some("array") => match value {
                Value::Array(more) => items.extend(more),
                one => items.push(one),
            },
            Some(_) => return Err(format!("--{key} given more than once")),
        }
    }

    // The host binds these (and reports too many, or a clash with a flag).
    // Type each by the property it will bind to; past the last, loosely.
    if !positionals.is_empty() {
        let list = positionals
            .iter()
            .enumerate()
            .map(|(i, text)| match props.get(i) {
                Some((key, _)) => coerce(&props, key, text),
                None => Ok(loose(text)),
            })
            .collect::<Result<_, _>>()?;
        flags.insert(POSITIONAL_ARGS_KEY.to_owned(), Value::Array(list));
    }

    let mut out = base.unwrap_or_default();
    out.extend(flags);
    Ok(Call::Invoke(Value::Object(out)))
}

/// The help text for `NAME --help`.
pub fn help(name: &str, def: Option<&CallbackDef>) -> String {
    let mut out = format!("{name} - ");
    out.push_str(def.map_or("host callback", |d| d.description.as_str()));
    out.push_str(&format!(
        "\nusage: {name} [ARG]... [--key value]... [--json '{{...}}']\n"
    ));
    let schema = def.and_then(|d| d.schema.as_ref());
    let props = properties(schema);
    if !props.is_empty() {
        let required: Vec<&str> = schema
            .and_then(|s| s.get("required"))
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        out.push_str("arguments, in positional order:\n");
        for (key, prop) in &props {
            out.push_str(&format!("  --{key}"));
            if let Some(ty) = schema_type(prop) {
                out.push_str(&format!(" <{ty}>"));
            }
            if required.contains(&key.as_str()) {
                out.push_str(" (required)");
            }
            if let Some(desc) = prop.get("description").and_then(Value::as_str) {
                out.push_str(&format!("  {desc}"));
            }
            out.push('\n');
        }
    }
    out
}

/// The text a callback's JSON result prints as, or `None` to print nothing.
pub fn render(result_json: &str) -> Option<String> {
    match serde_json::from_str::<Value>(result_json) {
        Ok(Value::Null) => None,
        Ok(Value::String(s)) => Some(s),
        Ok(v) => Some(v.to_string()),
        // Not JSON: pass the host's text through.
        Err(_) if result_json.is_empty() => None,
        Err(_) => Some(result_json.to_owned()),
    }
}

/// The schema's properties, in the order the host serialized them.
fn properties(schema: Option<&Value>) -> Vec<(String, Value)> {
    schema
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)
        .map(|p| p.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// A property schema's JSON type: its `type`, or the first non-null one of a
/// list (`["string", "null"]` for an optional string).
fn schema_type(prop: &Value) -> Option<&str> {
    match prop.get("type")? {
        Value::String(t) => Some(t),
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    }
}

fn prop_type<'a>(props: &'a [(String, Value)], key: &str) -> Option<&'a str> {
    props
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, p)| schema_type(p))
}

/// JSON if it parses, otherwise the text as a string.
fn loose(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

/// Type `text` for property `key` by its schema.
fn coerce(props: &[(String, Value)], key: &str, text: &str) -> Result<Value, String> {
    let bad = |what: &str| format!("--{key} expects {what}, got '{text}'");
    let item_type = || {
        props
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, p)| p.get("items"))
            .and_then(schema_type)
    };
    Ok(match prop_type(props, key) {
        Some("string") => Value::String(text.to_owned()),
        Some("integer") => text
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| text.parse::<u64>().map(Value::from))
            .map_err(|_| bad("an integer"))?,
        Some("number") => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| bad("a number"))?,
        Some("boolean") => match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(bad("true or false")),
        },
        Some("object") => match serde_json::from_str(text) {
            Ok(v @ Value::Object(_)) => v,
            _ => return Err(bad("a JSON object")),
        },
        // A JSON array, or one element of it (typed by `items`).
        Some("array") => match serde_json::from_str(text) {
            Ok(v @ Value::Array(_)) => v,
            _ => Value::Array(vec![match item_type() {
                Some("string") => Value::String(text.to_owned()),
                _ => loose(text),
            }]),
        },
        _ => loose(text),
    })
}

/// `value` with every object's keys sorted, recursively: the arguments'
/// canonical form, which callback replay journals key on.
pub(crate) fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> =
                map.into_iter().map(|(k, v)| (k, canonical(v))).collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_sorts_keys_recursively() {
        let v = canonical(json!({"b": 1, "a": {"d": [{"z": 0, "y": 1}], "c": 2}}));
        assert_eq!(v.to_string(), r#"{"a":{"c":2,"d":[{"y":1,"z":0}]},"b":1}"#);
    }
    use serde_json::json;

    fn def(schema: Value) -> CallbackDef {
        CallbackDef {
            description: "test".into(),
            schema: Some(schema),
        }
    }

    fn call(def: Option<&CallbackDef>, args: &[&str]) -> Result<Value, String> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        match parse(def, &args)? {
            Call::Invoke(v) => Ok(v),
            Call::Help => Err("help".into()),
        }
    }

    fn weather() -> CallbackDef {
        // Deliberately not alphabetical: positionals follow this order.
        def(json!({
            "type": "object",
            "properties": {
                "city": {"type": "string"},
                "days": {"type": "integer"},
                "metric": {"type": "boolean"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "extra": {}
            },
            "required": ["city"]
        }))
    }

    #[test]
    fn flags_are_typed_by_the_schema() {
        let d = weather();
        assert_eq!(
            call(Some(&d), &["--city", "007", "--days=3", "--metric"]).unwrap(),
            json!({"city": "007", "days": 3, "metric": true})
        );
        assert_eq!(
            call(Some(&d), &["--metric", "false", "--extra", "[1]"]).unwrap(),
            json!({"metric": false, "extra": [1]})
        );
        // Not in the schema: JSON if it parses.
        assert_eq!(
            call(Some(&d), &["--n", "1", "--s", "x"]).unwrap(),
            json!({"n": 1, "s": "x"})
        );
        assert!(call(Some(&d), &["--days", "three"]).is_err());
        assert!(call(Some(&d), &["--metric", "--days"]).is_err());
    }

    #[test]
    fn positionals_go_to_the_host_typed_by_position() {
        let d = weather();
        assert_eq!(
            call(Some(&d), &["007", "5", "--metric"]).unwrap(),
            json!({"__eryx_args__": ["007", 5], "metric": true})
        );
        // Past the last property: typed loosely, for the host to reject.
        let empty = def(json!({"type": "object", "properties": {}}));
        assert_eq!(
            call(Some(&empty), &["x", "1"]).unwrap(),
            json!({"__eryx_args__": ["x", 1]})
        );
        assert!(call(Some(&d), &["London", "soon"]).is_err());
    }

    #[test]
    fn repeats_collect_only_into_arrays() {
        let d = weather();
        assert_eq!(
            call(
                Some(&d),
                &["--tags", "a", "--tags", "1", "--tags", "[\"b\"]"]
            )
            .unwrap(),
            json!({"tags": ["a", "1", "b"]})
        );
        assert_eq!(
            call(Some(&d), &["--days", "1", "--days", "2"]).unwrap_err(),
            "--days given more than once"
        );
    }

    #[test]
    fn json_is_the_base_object() {
        let d = weather();
        assert_eq!(
            call(
                Some(&d),
                &["--json", r#"{"city": "x", "days": 1}"#, "--days", "2"]
            )
            .unwrap(),
            json!({"city": "x", "days": 2})
        );
        assert!(call(Some(&d), &["--json", "[1]"]).is_err());
    }

    #[test]
    fn double_dash_ends_flags() {
        let d = weather();
        assert_eq!(
            call(Some(&d), &["--", "--city"]).unwrap(),
            json!({"__eryx_args__": ["--city"]})
        );
    }

    #[test]
    fn prefix_names_send_positionals_to_the_host() {
        assert_eq!(
            call(None, &["a", "2", "--k", "v"]).unwrap(),
            json!({"__eryx_args__": ["a", 2], "k": "v"})
        );
        assert!(call(None, &["--__eryx_args__", "[]"]).is_err());
    }

    #[test]
    fn help_unless_help_is_a_parameter() {
        let d = weather();
        assert_eq!(parse(Some(&d), &["--help".to_owned()]).unwrap(), Call::Help);
        let h = def(json!({"properties": {"help": {"type": "string"}}}));
        assert_eq!(
            call(Some(&h), &["--help", "x"]).unwrap(),
            json!({"help": "x"})
        );
        assert!(help("cb", Some(&d)).contains("--city <string> (required)"));
    }

    #[test]
    fn results_render_as_text() {
        assert_eq!(render(r#""hi""#).as_deref(), Some("hi"));
        assert_eq!(render("null"), None);
        assert_eq!(render(r#"{"a": 1}"#).as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(render("3").as_deref(), Some("3"));
    }
}
