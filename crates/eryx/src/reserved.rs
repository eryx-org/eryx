//! Callback names each guest language already uses (#962).
//!
//! A guest installs each callback under its name (or, for a dotted name, its
//! first part), and skips one that would replace something of its own, so a
//! callback named like a JavaScript global, a Python builtin or a shell
//! builtin could only ever be reached through `invoke()`. `build()` refuses
//! those names instead. The lists are kept here rather than asked of the
//! guest, and tests check them against the real guests.

use crate::{Error, ErrorKind, Language};

/// What happens to a callback whose name a guest already uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// `build()` fails.
    Refuse,
    /// `build()` logs a warning; the guest skips the callback.
    Warn,
}

// ponytail: Python only warns, since released pyeryx apps may register names
// like `list` or `open` and call them through invoke(); switch it to Refuse
// once that's decided.
const PYTHON_POLICY: Policy = Policy::Warn;

const JAVASCRIPT: &str = include_str!("reserved/javascript.txt");
const PYTHON: &str = include_str!("reserved/python.txt");
const SHELL: &str = include_str!("reserved/shell.txt");

/// The names in one of the lists, skipping comments.
pub(crate) fn names(list: &'static str) -> impl Iterator<Item = &'static str> {
    list.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// The names `language`'s guest uses itself, what they are, and the policy.
fn reserved(language: Language) -> (&'static str, &'static str, Policy) {
    match language {
        Language::JavaScript => (JAVASCRIPT, "the JavaScript global", Policy::Refuse),
        Language::Shell => (SHELL, "the shell builtin", Policy::Refuse),
        _ => (PYTHON, "the Python builtin", PYTHON_POLICY),
    }
}

/// The name a guest installs a callback under: the first part of a dotted
/// name (`mcp.github.*` is installed as `mcp`), except in the shell, where
/// the whole name is the command.
fn installed_as(language: Language, name: &str) -> &str {
    if language == Language::Shell {
        return name;
    }
    name.split('.').next().unwrap_or(name)
}

/// Fail (or, for Python, warn) if a callback would collide with something
/// the guest already has, or, in the shell, with a host command of the same
/// name, which the callback would hide. Reports the first collision by name,
/// so the error is stable.
pub(crate) fn check<'a>(
    language: Language,
    callbacks: impl IntoIterator<Item = &'a str>,
    commands: impl IntoIterator<Item = &'a str>,
) -> Result<(), Error> {
    let (list, what, policy) = reserved(language);
    let mut callbacks: Vec<&str> = callbacks.into_iter().collect();
    callbacks.sort_unstable();
    let commands: std::collections::HashSet<&str> = commands.into_iter().collect();
    for name in callbacks {
        let root = installed_as(language, name);
        let message = if names(list).any(|reserved| reserved == root) {
            format!(
                "callback '{name}' collides with {what} '{root}': the guest keeps its own, \
                 so the callback could only be reached through invoke(); give it another name"
            )
        } else if language == Language::Shell && commands.contains(root) {
            format!(
                "callback '{name}' collides with the command '{root}', which it would hide; \
                 give one of them another name"
            )
        } else {
            continue;
        };
        if policy == Policy::Warn {
            tracing::warn!(callback = name, "{message}");
            continue;
        }
        return Err(ErrorKind::Initialization(message).into());
    }
    Ok(())
}

/// The JS SDK's copy of the lists (js/eryx/shims/reserved-names.js), as
/// `mise run update-reserved-names` writes it.
#[cfg(test)]
fn js_sdk_module() -> String {
    let list = |text: &'static str| {
        let quoted: Vec<String> = names(text).map(|n| format!("    {n:?},")).collect();
        format!("[\n{}\n  ]", quoted.join("\n"))
    };
    format!(
        "// Generated from crates/eryx/src/reserved/*.txt by `mise run update-reserved-names`\n\
         // (#962). Don't edit: a test in crates/eryx/src/reserved.rs checks it's current.\n\
         \n\
         /** Callback names each guest uses itself, by guest language. */\n\
         export const RESERVED_CALLBACK_NAMES = Object.freeze({{\n\
         \x20 javascript: Object.freeze({}),\n\
         \x20 python: Object.freeze({}),\n\
         \x20 shell: Object.freeze({}),\n\
         }});\n",
        list(JAVASCRIPT),
        list(PYTHON),
        list(SHELL),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The JS SDK checks callback names against its own copy of the lists;
    /// `ERYX_UPDATE_RESERVED_NAMES=1` (`mise run update-reserved-names`)
    /// rewrites it.
    #[test]
    fn js_sdk_copy_is_current() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../js/eryx/shims/reserved-names.js"
        );
        let expected = js_sdk_module();
        if std::env::var_os("ERYX_UPDATE_RESERVED_NAMES").is_some() {
            std::fs::write(path, &expected).unwrap();
        }
        let actual = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            actual == expected,
            "js/eryx/shims/reserved-names.js is stale: run `mise run update-reserved-names`"
        );
    }

    #[test]
    fn lists_are_sorted_and_unique() {
        for list in [JAVASCRIPT, PYTHON, SHELL] {
            let names: Vec<_> = names(list).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(names, sorted);
        }
    }

    /// The preamble's names in python.rs's `_eryx_reserved` literal are all
    /// in the Python list (its builtins part is checked against a live
    /// guest, in tests/reserved_names.rs).
    #[test]
    fn python_list_has_the_guest_preamble_names() {
        let source = include_str!("../../eryx-guest-python/src/python.rs");
        let start = source
            .find("_eryx_reserved = set(dir(__builtins__)) | {")
            .expect("python.rs defines _eryx_reserved");
        let literal = &source[start..];
        let literal = &literal[literal.find('{').unwrap() + 1..literal.find('}').unwrap()];
        for name in literal.split(',').map(|n| n.trim().trim_matches('\'')) {
            if !name.is_empty() {
                assert!(names(PYTHON).any(|n| n == name), "{name} is missing");
            }
        }
    }

    #[test]
    fn refuses_names_and_roots_the_guest_uses() {
        let refused = |language, name: &str| check(language, [name], []).is_err();
        assert!(refused(Language::JavaScript, "fetch"));
        assert!(refused(Language::JavaScript, "crypto.subtle.*"));
        assert!(refused(Language::JavaScript, "toString"));
        assert!(!refused(Language::JavaScript, "fetch_user"));
        assert!(!refused(Language::JavaScript, "mcp.fetch.*"));
        assert!(refused(Language::Shell, "echo"));
        assert!(!refused(Language::Shell, "echo.*"));
        assert!(check(Language::Shell, ["cat"], ["cat"]).is_err());
        assert!(check(Language::Shell, ["cat"], ["ls"]).is_ok());
        // Python only warns (PYTHON_POLICY).
        assert!(!refused(Language::Python, "print"));
        assert!(!refused(Language::Python, "json.dumps"));
    }

    #[test]
    fn the_error_names_the_collision() {
        let err = check(Language::JavaScript, ["zeta", "fetch", "crypto"], []).unwrap_err();
        assert!(
            err.to_string()
                .contains("callback 'crypto' collides with the JavaScript global 'crypto'"),
            "{err}"
        );
    }
}
