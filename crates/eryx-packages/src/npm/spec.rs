//! What a caller asks for: `zod`, `zod@^3`, `@scope/pkg@next`.

use crate::{Error, Result};

/// A requested package: its name and a version range or dist-tag.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Spec {
    /// The package name, e.g. `zod` or `@scope/pkg`.
    pub name: String,
    /// A semver range (`^3.2`, `1.x`, `*`) or a dist-tag (`latest`, `next`).
    pub version: String,
}

impl Spec {
    /// Parse `name`, `name@range` or `name@tag`. A bare name means `latest`.
    ///
    /// # Errors
    ///
    /// Fails for an invalid name, or a spec eryx doesn't install from the
    /// registry: aliases (`npm:`), git, URLs and local paths.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        let (name, version) = match spec.strip_prefix('@') {
            Some(scoped) => match scoped.split_once('@') {
                Some((name, version)) => (format!("@{name}"), version),
                None => (spec.to_string(), ""),
            },
            None => match spec.split_once('@') {
                Some((name, version)) => (name.to_string(), version),
                None => (spec.to_string(), ""),
            },
        };
        let version = if version.trim().is_empty() {
            "latest".to_string()
        } else {
            version.trim().to_string()
        };
        let unsupported = [
            "npm:",
            "git",
            "http:",
            "https:",
            "file:",
            "link:",
            "workspace:",
        ];
        if unsupported.iter().any(|p| version.starts_with(p)) || version.contains('/') {
            return Err(Error::Resolve {
                spec: spec.to_string(),
                message: "only registry versions, ranges and dist-tags are supported".to_string(),
            });
        }
        check_name(&name).map_err(|message| Error::Resolve {
            spec: spec.to_string(),
            message,
        })?;
        Ok(Self { name, version })
    }
}

impl std::fmt::Display for Spec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// npm's package-name rules: at most 214 characters, URL-safe, lowercase,
/// optionally `@scope/name`, not starting with `.` or `_`.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    let invalid = |why: &str| Err(format!("invalid package name {name:?}: {why}"));
    if name.is_empty() || name.len() > 214 {
        return invalid("must be 1 to 214 characters");
    }
    let parts: Vec<&str> = match name.strip_prefix('@') {
        Some(scoped) => match scoped.split_once('/') {
            Some((scope, pkg)) => vec![scope, pkg],
            None => return invalid("a scoped name is @scope/name"),
        },
        None => vec![name],
    };
    for part in parts {
        if part.is_empty() || part.starts_with(['.', '_']) {
            return invalid("parts can't be empty or start with . or _");
        }
        if !part
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._~".contains(&b))
        {
            return invalid("only lowercase letters, digits and -._~ are allowed");
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_ranges_and_tags() {
        let parse = |s: &str| {
            let spec = Spec::parse(s).unwrap();
            (spec.name, spec.version)
        };
        assert_eq!(parse("zod"), ("zod".into(), "latest".into()));
        assert_eq!(parse("zod@^3.22"), ("zod".into(), "^3.22".into()));
        assert_eq!(parse("@scope/pkg"), ("@scope/pkg".into(), "latest".into()));
        assert_eq!(
            parse("@scope/pkg@next"),
            ("@scope/pkg".into(), "next".into())
        );
        assert_eq!(
            parse(" date-fns@ >=2 <4 "),
            ("date-fns".into(), ">=2 <4".into())
        );
    }

    #[test]
    fn refuses_unsupported_and_invalid_specs() {
        for bad in [
            "",
            "Zod",
            "_private",
            "@scope",
            "a b",
            "zod@npm:other@1",
            "zod@github:colinhacks/zod",
            "zod@https://example.com/zod.tgz",
            "zod@file:../zod",
            "x@user/repo",
        ] {
            assert!(Spec::parse(bad).is_err(), "{bad}");
        }
    }
}
