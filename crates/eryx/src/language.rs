//! The guest language a sandbox runs.

/// The guest language a sandbox runs.
///
/// Each language is a separate WebAssembly guest runtime behind the same host
/// API. Select one with [`SandboxBuilder::with_language`](crate::SandboxBuilder::with_language).
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
