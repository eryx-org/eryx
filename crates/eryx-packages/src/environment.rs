//! Environments: named package sets built ahead of time.
//!
//! An environment is built once (in CI or a deploy step) from a hand-written
//! [`EnvironmentDefinition`] and mounted read-only by sandboxes, which never
//! see package names or a registry. It's a directory:
//!
//! ```text
//! data-tools/
//!   eryx-environment.json   the Manifest
//!   <lockfile>              what was installed, e.g. eryx-npm.lock
//!   <content dir>/          what sandboxes use, e.g. modules/ for JavaScript
//! ```
//!
//! These are the data types; building one is `eryx::environment::build`.
//! The shapes are language-neutral: each language has its own section in a
//! definition (`[npm]` for JavaScript) and fills [`Manifest::exports`] with
//! what it makes importable.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// The manifest's file name in an environment directory.
pub const MANIFEST_FILE: &str = "eryx-environment.json";

/// [`Manifest::format`].
pub const MANIFEST_FORMAT: &str = "eryx-environment";

/// The manifest format version this crate writes and reads.
pub const MANIFEST_VERSION: u32 = 1;

/// The guest language an environment is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Language {
    /// JavaScript: npm packages, bundled into ES modules.
    JavaScript,
    /// Python: site-packages (not supported yet).
    Python,
    /// The shell: a set of commands (not supported yet).
    Shell,
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Shell => "shell",
        })
    }
}

/// A hand-written environment definition (TOML):
///
/// ```toml
/// name = "data-tools"
/// language = "javascript"
///
/// [npm]
/// packages = ["zod@^3", "date-fns"]
/// entries = ["date-fns/format"]   # importable beyond the packages themselves
/// registry = "https://registry.npmjs.org/"
/// min-release-age = "3d"
/// lockfile = "data-tools.lock"
///
/// [limits]
/// max-packages = 500
/// max-tarball-bytes = "32MiB"
/// ```
///
/// Unknown fields, and sections for another language, are refused rather
/// than ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct EnvironmentDefinition {
    /// The environment's name: letters, digits, `-`, `_` and `.`.
    pub name: String,
    /// The language it's for.
    pub language: Language,
    /// npm packages (JavaScript).
    #[serde(default)]
    pub npm: Option<NpmSection>,
    /// Overrides of the default install limits.
    #[serde(default)]
    pub limits: LimitsSection,
}

/// A definition's `[npm]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct NpmSection {
    /// Package specs: `zod`, `zod@^3`, `@scope/pkg@next`.
    pub packages: Vec<String>,
    /// Specifiers to make importable beyond each package's root, e.g.
    /// `date-fns/format`.
    #[serde(default)]
    pub entries: Vec<String>,
    /// The registry (default: the public npm registry).
    #[serde(default)]
    pub registry: Option<String>,
    /// More hosts the registry may serve tarballs from.
    #[serde(default)]
    pub tarball_hosts: Vec<String>,
    /// Don't install versions published more recently than this (default
    /// `3d`; `0` turns it off). A number and a unit: `s`, `m`, `h`, `d`.
    #[serde(default)]
    pub min_release_age: Option<String>,
    /// The lockfile, relative to the definition (default `<name>.lock`).
    #[serde(default)]
    pub lockfile: Option<PathBuf>,
}

/// A definition's `[limits]` section. Sizes are bytes, as a number or with
/// a unit (`32MiB`, `1GiB`, `500KB`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct LimitsSection {
    /// Packages in the environment.
    #[serde(default)]
    pub max_packages: Option<usize>,
    /// Bytes of one downloaded package.
    #[serde(default)]
    pub max_package_bytes: Option<Size>,
    /// Bytes unpacked across all packages.
    #[serde(default)]
    pub max_total_bytes: Option<Size>,
}

/// A byte size in a definition: a number, or a string with a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "SizeValue")]
pub struct Size(pub u64);

#[derive(Deserialize)]
#[serde(untagged)]
enum SizeValue {
    Number(u64),
    Text(String),
}

impl TryFrom<SizeValue> for Size {
    type Error = String;

    fn try_from(value: SizeValue) -> Result<Self, String> {
        match value {
            SizeValue::Number(n) => Ok(Self(n)),
            SizeValue::Text(text) => parse_size(&text).map(Self),
        }
    }
}

fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid size {text:?}"))?;
    let multiplier: u64 = match unit.trim() {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        other => return Err(format!("unknown size unit {other:?} in {text:?}")),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size {text:?} is too large"))
}

/// Parse a duration like `3d`, `12h`, `30m`, `90s` or `0`.
///
/// # Errors
///
/// Fails for anything else.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration {text:?}"))?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(format!("invalid duration {text:?}: use s, m, h or d")),
    };
    number
        .checked_mul(seconds)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("duration {text:?} is too large"))
}

impl EnvironmentDefinition {
    /// Parse and check a definition.
    ///
    /// # Errors
    ///
    /// Fails for invalid TOML, unknown fields, a bad name, size or duration,
    /// or a language section that doesn't match `language`.
    pub fn from_toml(text: &str) -> Result<Self> {
        let definition: Self =
            toml::from_str(text).map_err(|e| Error::Definition(e.to_string()))?;
        definition.check()?;
        Ok(definition)
    }

    /// Read and check a definition file.
    ///
    /// # Errors
    ///
    /// As for [`from_toml`](Self::from_toml), or if it can't be read.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(Error::io(format!("reading {}", path.display())))?;
        Self::from_toml(&text)
            .map_err(|e| Error::Definition(format!("{}: {}", path.display(), strip(e))))
    }

    fn check(&self) -> Result<()> {
        let bad = |message: String| Err(Error::Definition(message));
        if self.name.is_empty()
            || self.name.starts_with('.')
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return bad(format!(
                "invalid environment name {:?}: use letters, digits, -, _ and .",
                self.name
            ));
        }
        match (self.language, &self.npm) {
            (Language::JavaScript, None) => {
                bad("a javascript environment needs an [npm] section".into())
            }
            (Language::JavaScript, Some(npm)) => {
                if npm.packages.is_empty() {
                    return bad("[npm] packages is empty".into());
                }
                if let Some(age) = &npm.min_release_age {
                    parse_duration(age).map_err(Error::Definition)?;
                }
                Ok(())
            }
            (language, Some(_)) => bad(format!("[npm] doesn't apply to a {language} environment")),
            (_, None) => Ok(()),
        }
    }
}

/// The message of a definition error, without its variant's prefix.
fn strip(error: Error) -> String {
    match error {
        Error::Definition(message) => message,
        other => other.to_string(),
    }
}

/// `eryx-environment.json`: what an environment holds and how it was built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Manifest {
    /// Always [`MANIFEST_FORMAT`].
    pub format: String,
    /// [`MANIFEST_VERSION`].
    pub format_version: u32,
    /// The environment's name.
    pub name: String,
    /// The language it's for; a sandbox for another refuses it.
    pub language: Language,
    /// What it was built from.
    pub inputs: Inputs,
    /// What built it.
    pub builder: Builder,
    /// What sandboxes use.
    pub content: Content,
    /// What it makes importable, by name, as paths relative to the
    /// environment's root: bare module specifiers for JavaScript, command
    /// names for the shell. Python's site-packages needs none.
    #[serde(default)]
    pub exports: BTreeMap<String, String>,
    /// What the build warned about: skipped optional dependencies,
    /// conflicting peers, imports the bundler couldn't resolve.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// [`Manifest::inputs`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Inputs {
    /// The package specs, sorted.
    pub packages: Vec<String>,
    /// The registry, if packages came from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// The lockfile's file name in the environment, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile: Option<String>,
    /// The lockfile's content hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile_hash: Option<String>,
}

impl Inputs {
    /// Inputs of `packages`, with no registry or lockfile.
    #[must_use]
    pub fn new(packages: Vec<String>) -> Self {
        Self {
            packages,
            registry: None,
            lockfile: None,
            lockfile_hash: None,
        }
    }
}

/// [`Manifest::builder`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Builder {
    /// The eryx version that built it.
    pub eryx: String,
    /// Tools it used and their versions, e.g. `bundler: rolldown 1.3.2`.
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
}

impl Builder {
    /// Built by eryx `version`, with no tools recorded.
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            eryx: version.into(),
            tools: BTreeMap::new(),
        }
    }
}

/// [`Manifest::content`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Content {
    /// The directory, relative to the environment's root.
    pub dir: String,
    /// Files in it.
    pub files: u64,
    /// Bytes in them.
    pub bytes: u64,
    /// [`content_hash`] of the directory: the environment's identity.
    pub hash: String,
}

impl Manifest {
    /// A manifest for content in `root/<dir>`, hashing it.
    ///
    /// # Errors
    ///
    /// Fails if the directory can't be read.
    pub fn new(
        name: &str,
        language: Language,
        inputs: Inputs,
        builder: Builder,
        root: &Path,
        dir: &str,
        exports: BTreeMap<String, String>,
    ) -> Result<Self> {
        let content = content_hash(&root.join(dir))?;
        Ok(Self {
            format: MANIFEST_FORMAT.to_string(),
            format_version: MANIFEST_VERSION,
            name: name.to_string(),
            language,
            inputs,
            builder,
            content: Content {
                dir: dir.to_string(),
                files: content.files,
                bytes: content.bytes,
                hash: content.hash,
            },
            exports,
            warnings: Vec::new(),
        })
    }

    /// Read and check `root/eryx-environment.json`.
    ///
    /// # Errors
    ///
    /// Fails if it can't be read or parsed, is another format or version,
    /// or names paths outside the environment.
    pub fn read(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST_FILE);
        let bytes =
            std::fs::read(&path).map_err(Error::io(format!("reading {}", path.display())))?;
        let manifest: Self = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Definition(format!("{}: {e}", path.display())))?;
        manifest.check()?;
        Ok(manifest)
    }

    /// Write it to `root/eryx-environment.json`.
    ///
    /// # Errors
    ///
    /// Fails if it can't be written.
    pub fn write(&self, root: &Path) -> Result<()> {
        let mut json = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Definition(format!("serializing the manifest: {e}")))?;
        json.push(b'\n');
        let path = root.join(MANIFEST_FILE);
        std::fs::write(&path, json).map_err(Error::io(format!("writing {}", path.display())))
    }

    /// Check the content still hashes to what the manifest says.
    ///
    /// # Errors
    ///
    /// [`Error::Integrity`] if it doesn't, or an I/O error.
    pub fn verify(&self, root: &Path) -> Result<()> {
        let actual = content_hash(&root.join(&self.content.dir))?;
        if actual.hash == self.content.hash {
            Ok(())
        } else {
            Err(Error::Integrity {
                what: format!("{}'s content", root.display()),
                expected: self.content.hash.clone(),
            })
        }
    }

    fn check(&self) -> Result<()> {
        let bad = |message: String| Err(Error::Definition(message));
        if self.format != MANIFEST_FORMAT {
            return bad(format!(
                "not an eryx environment manifest ({:?})",
                self.format
            ));
        }
        if self.format_version != MANIFEST_VERSION {
            return bad(format!(
                "environment manifest version {}, this eryx reads {MANIFEST_VERSION}",
                self.format_version
            ));
        }
        let inside = |path: &str| {
            let path = Path::new(path);
            !path.as_os_str().is_empty()
                && path.components().all(|c| matches!(c, Component::Normal(_)))
        };
        if !inside(&self.content.dir) {
            return bad(format!(
                "content dir {:?} is outside the environment",
                self.content.dir
            ));
        }
        for (name, path) in &self.exports {
            if !inside(path) || !Path::new(path).starts_with(&self.content.dir) {
                return bad(format!(
                    "export {name:?} points outside the content dir: {path:?}"
                ));
            }
        }
        Ok(())
    }
}

/// What [`content_hash`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHash {
    /// Regular files.
    pub files: u64,
    /// Their total size.
    pub bytes: u64,
    /// `sha256-<hex>` over each file's relative path and contents, in path
    /// order.
    pub hash: String,
}

/// Hash a directory's files: their `/`-separated relative paths and
/// contents, in sorted order. Symlinks and other non-regular files are
/// refused, so what's hashed is what a mount would show.
///
/// # Errors
///
/// Fails if the directory can't be read or holds a non-regular file.
pub fn content_hash(dir: &Path) -> Result<ContentHash> {
    let mut paths = Vec::new();
    collect(dir, dir, &mut paths)?;
    paths.sort();
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    for (relative, path) in &paths {
        let contents =
            std::fs::read(path).map_err(Error::io(format!("reading {}", path.display())))?;
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update((contents.len() as u64).to_le_bytes());
        hasher.update(&contents);
        bytes += contents.len() as u64;
    }
    Ok(ContentHash {
        files: paths.len() as u64,
        bytes,
        hash: format!("sha256-{}", hex::encode(hasher.finalize())),
    })
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).map_err(Error::io(format!("reading {}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(Error::io(format!("reading {}", dir.display())))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(Error::io(format!("reading {}", path.display())))?;
        if kind.is_dir() {
            collect(root, &path, out)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push((relative, path));
        } else {
            return Err(Error::Extract {
                what: root.display().to_string(),
                message: format!("{} is not a regular file", path.display()),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_javascript_definition() {
        let definition = EnvironmentDefinition::from_toml(
            r#"
            name = "data-tools"
            language = "javascript"

            [npm]
            packages = ["zod@^3", "date-fns"]
            entries = ["date-fns/format"]
            min-release-age = "12h"

            [limits]
            max-packages = 50
            max-package-bytes = "8MiB"
            max-total-bytes = 1000
            "#,
        )
        .unwrap();
        assert_eq!(definition.language, Language::JavaScript);
        let npm = definition.npm.unwrap();
        assert_eq!(npm.packages, ["zod@^3", "date-fns"]);
        assert_eq!(npm.entries, ["date-fns/format"]);
        assert_eq!(definition.limits.max_package_bytes, Some(Size(8 << 20)));
        assert_eq!(definition.limits.max_total_bytes, Some(Size(1000)));
    }

    #[test]
    fn refuses_bad_definitions() {
        for (toml, why) in [
            ("name = \"x\"\nlanguage = \"javascript\"", "needs an [npm]"),
            (
                "name = \"x\"\nlanguage = \"python\"\n[npm]\npackages = [\"a\"]",
                "doesn't apply",
            ),
            (
                "name = \"x\"\nlanguage = \"javascript\"\n[npm]\npackages = []",
                "empty",
            ),
            (
                "name = \"../x\"\nlanguage = \"shell\"",
                "invalid environment name",
            ),
            ("name = \"x\"\nlanguage = \"cobol\"", "unknown variant"),
            (
                "name = \"x\"\nlanguage = \"shell\"\nextra = 1",
                "unknown field",
            ),
            (
                "name = \"x\"\nlanguage = \"javascript\"\n[npm]\npackages = [\"a\"]\nmin-release-age = \"3 days\"",
                "invalid duration",
            ),
            (
                "name = \"x\"\nlanguage = \"shell\"\n[limits]\nmax-total-bytes = \"1 TB\"",
                "unknown size unit",
            ),
        ] {
            let err = EnvironmentDefinition::from_toml(toml)
                .unwrap_err()
                .to_string();
            assert!(err.contains(why), "{toml}: {err}");
        }
        // Languages without a backend yet still parse; building them is
        // what's unsupported.
        EnvironmentDefinition::from_toml("name = \"x\"\nlanguage = \"python\"").unwrap();
    }

    #[test]
    fn durations_and_sizes() {
        assert_eq!(parse_duration("0"), Ok(Duration::ZERO));
        assert_eq!(parse_duration("3d"), Ok(Duration::from_secs(3 * 86_400)));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert!(parse_duration("3").is_err());
        assert_eq!(parse_size("1GiB"), Ok(1 << 30));
        assert_eq!(parse_size("500 KB"), Ok(500_000));
    }

    #[test]
    fn manifest_round_trips_and_detects_changes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("modules/sub")).unwrap();
        std::fs::write(root.path().join("modules/a.js"), "a").unwrap();
        std::fs::write(root.path().join("modules/sub/b.js"), "b").unwrap();
        let manifest = Manifest::new(
            "env",
            Language::JavaScript,
            Inputs::new(vec!["a@1".to_string()]),
            Builder::new("0.0.0"),
            root.path(),
            "modules",
            BTreeMap::from([("a".to_string(), "modules/a.js".to_string())]),
        )
        .unwrap();
        assert_eq!((manifest.content.files, manifest.content.bytes), (2, 2));
        manifest.write(root.path()).unwrap();
        let read = Manifest::read(root.path()).unwrap();
        assert_eq!(read, manifest);
        read.verify(root.path()).unwrap();

        std::fs::write(root.path().join("modules/sub/b.js"), "B").unwrap();
        assert!(matches!(
            read.verify(root.path()),
            Err(Error::Integrity { .. })
        ));

        let mut outside = manifest.clone();
        outside
            .exports
            .insert("evil".to_string(), "../x.js".to_string());
        outside.write(root.path()).unwrap();
        assert!(Manifest::read(root.path()).is_err());
    }

    #[test]
    fn content_hash_depends_on_paths_and_contents() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("x"), "12").unwrap();
        std::fs::write(b.path().join("x1"), "2").unwrap();
        assert_ne!(
            content_hash(a.path()).unwrap().hash,
            content_hash(b.path()).unwrap().hash
        );
    }
}
