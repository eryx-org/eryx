//! The guest language a sandbox runs.
//!
//! [`Language`] names a language at runtime. The marker types [`Python`],
//! [`JavaScript`] and [`Shell`] name it at compile time, as the `Lang`
//! parameter of [`SandboxBuilder`](crate::SandboxBuilder), so that
//! language-specific options are only available for their language.

/// The guest language a sandbox runs.
///
/// Each language is a separate WebAssembly guest runtime behind the same host
/// API. Select one with [`SandboxBuilder::javascript`](crate::SandboxBuilder::javascript)
/// or [`SandboxBuilder::shell`](crate::SandboxBuilder::shell); the default is Python.
///
/// Only [`Language::Python`] ships an embedded runtime today. The other
/// languages need their runtime supplied explicitly (for example with
/// [`SandboxBuilder::with_wasm_file`](crate::SandboxBuilder::with_wasm_file)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Language {
    /// CPython. The default.
    #[default]
    Python,
    /// JavaScript, via QuickJS.
    JavaScript,
    /// A bash-compatible shell.
    Shell,
}

impl Language {
    /// The lowercase name of the language, e.g. `"python"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::Shell => "shell",
        }
    }

    /// The byte that stands for this language in a serialized state snapshot.
    pub(crate) const fn snapshot_tag(self) -> u8 {
        match self {
            Self::Python => 0,
            Self::JavaScript => 1,
            Self::Shell => 2,
        }
    }

    /// The language a snapshot tag stands for, if this eryx knows it.
    pub(crate) const fn from_snapshot_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Python),
            1 => Some(Self::JavaScript),
            2 => Some(Self::Shell),
            _ => None,
        }
    }
}

/// A guest language marker type, used as the `Lang` parameter of
/// [`SandboxBuilder`](crate::SandboxBuilder).
///
/// Sealed: eryx defines the languages.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a guest language",
    label = "the first parameter of `SandboxBuilder` is the language",
    note = "use `eryx::language::Python`, `JavaScript` or `Shell`: \
            0.15's `SandboxBuilder<state::Has, state::Has>` is now `SandboxBuilder<Python, state::Has>`"
)]
pub trait GuestLanguage: sealed::Sealed + Send + Sync + 'static {
    /// The language this marker stands for.
    const LANGUAGE: Language;
}

mod sealed {
    pub trait Sealed {}
}

macro_rules! guest_language {
    ($(#[$doc:meta])* $marker:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, Default)]
        pub struct $marker;

        impl sealed::Sealed for $marker {}

        impl GuestLanguage for $marker {
            const LANGUAGE: Language = Language::$marker;
        }
    };
}

guest_language!(
    /// CPython: [`Language::Python`].
    Python
);
guest_language!(
    /// JavaScript, via QuickJS: [`Language::JavaScript`].
    JavaScript
);
guest_language!(
    /// A bash-compatible shell: [`Language::Shell`].
    Shell
);

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Language {
    type Err = String;

    /// Parse a language name, as written by [`Language::as_str`]. `js` and
    /// `bash` are accepted too.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "python" => Ok(Self::Python),
            "javascript" | "js" => Ok(Self::JavaScript),
            "shell" | "bash" => Ok(Self::Shell),
            _ => Err(format!(
                "unknown language {s:?}: expected python, javascript or shell"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Language; 3] = [Language::Python, Language::JavaScript, Language::Shell];

    #[test]
    fn names_round_trip() {
        for language in ALL {
            assert_eq!(language.as_str().parse::<Language>(), Ok(language));
            assert_eq!(language.to_string(), language.as_str());
        }
        assert_eq!("JS".parse::<Language>(), Ok(Language::JavaScript));
        assert!("ruby".parse::<Language>().is_err());
    }

    #[test]
    fn snapshot_tags_round_trip() {
        for language in ALL {
            assert_eq!(
                Language::from_snapshot_tag(language.snapshot_tag()),
                Some(language)
            );
        }
        assert_eq!(Language::from_snapshot_tag(3), None);
    }
}
