//! The environment format: an environment directory's manifest
//! (`eryx-environment.json`) and the hash that identifies it. See
//! book/src/api/environment-format.md, the contract between the eryx that
//! packs an environment and the eryx that loads it.
//!
//! ```text
//! data-tools/
//!   eryx-environment.json   the Manifest
//!   <lockfile>              optional: what was installed, e.g. package-lock.json
//!   <content dir>/          what sandboxes use, e.g. modules/ for JavaScript
//! ```

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::EnvironmentError;

type Result<T, E = EnvironmentError> = std::result::Result<T, E>;

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> EnvironmentError {
    let context = context.into();
    move |source| EnvironmentError::Io { context, source }
}

/// The manifest's file name in an environment directory.
pub const MANIFEST_FILE: &str = "eryx-environment.json";

/// [`Manifest::format`].
pub const MANIFEST_FORMAT: &str = "eryx-environment";

/// The manifest format version this eryx writes and reads.
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
    /// What it was built from (informational: a writer may leave it out).
    #[serde(default)]
    pub inputs: Inputs,
    /// What built it (informational: a writer may leave it out).
    #[serde(default)]
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Inputs {
    /// The package specs, sorted.
    #[serde(default)]
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Builder {
    /// The eryx version that built it.
    #[serde(default)]
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
    /// Files in it (informational).
    #[serde(default)]
    pub files: u64,
    /// Bytes in them (informational).
    #[serde(default)]
    pub bytes: u64,
    /// The environment's identity: a hash over its name, language, content
    /// dir and exports, and every file in the content dir (see
    /// [`Manifest::identity_hash`]).
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
        let mut manifest = Self {
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
                hash: String::new(),
            },
            exports,
            warnings: Vec::new(),
        };
        manifest.check()?;
        manifest.check_exports(&content)?;
        manifest.content.hash = manifest.identity_hash(&content);
        Ok(manifest)
    }

    /// The hash [`Content::hash`] records: over the name, language, content
    /// dir, exports and `content`'s (the content dir's) [`ContentHash`]. So
    /// it changes if any of what a sandbox relies on changes: the files, or
    /// what the manifest says they are.
    #[must_use]
    pub fn identity_hash(&self, content: &ContentHash) -> String {
        let mut hasher = Sha256::new();
        for part in [
            MANIFEST_FORMAT,
            &MANIFEST_VERSION.to_string(),
            &self.name,
            &self.language.to_string(),
            &self.content.dir,
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        for (name, path) in &self.exports {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            hasher.update(path.as_bytes());
            hasher.update([0]);
        }
        hasher.update(b"files\0");
        hasher.update(content.hash.as_bytes());
        format!("sha256-{}", hex::encode(hasher.finalize()))
    }

    /// Check every export names a file in the content dir.
    fn check_exports(&self, content: &ContentHash) -> Result<()> {
        for (name, path) in &self.exports {
            let inside = path
                .strip_prefix(&self.content.dir)
                .and_then(|p| p.strip_prefix('/'));
            if !inside.is_some_and(|p| content.paths.iter().any(|f| f == p)) {
                return Err(EnvironmentError::Manifest(format!(
                    "export {name:?} names {path:?}, which isn't a file in the environment"
                )));
            }
        }
        Ok(())
    }

    /// Read and check `root/eryx-environment.json`.
    ///
    /// # Errors
    ///
    /// Fails if it can't be read or parsed, is another format or version,
    /// or names paths outside the environment.
    pub fn read(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST_FILE);
        let bytes = std::fs::read(&path).map_err(io(format!("reading {}", path.display())))?;
        let manifest: Self = serde_json::from_slice(&bytes)
            .map_err(|e| EnvironmentError::Manifest(format!("{}: {e}", path.display())))?;
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
            .map_err(|e| EnvironmentError::Manifest(format!("serializing the manifest: {e}")))?;
        json.push(b'\n');
        let path = root.join(MANIFEST_FILE);
        std::fs::write(&path, json).map_err(io(format!("writing {}", path.display())))
    }

    /// Check the environment in `root` is what this manifest describes:
    /// every export names a file in it, and its files and the manifest's
    /// identity fields still hash to [`Content::hash`]. This catches
    /// accidental changes (a partial copy, an edited or corrupted file, a
    /// hand-edited manifest); it can't stop someone who can write the
    /// environment from changing both it and the hash, so pin an expected
    /// hash where that matters.
    ///
    /// # Errors
    ///
    /// [`EnvironmentError::Integrity`] if it doesn't match, or an I/O error.
    pub fn verify(&self, root: &Path) -> Result<()> {
        let actual = content_hash(&root.join(&self.content.dir))?;
        self.check_exports(&actual)?;
        if self.identity_hash(&actual) == self.content.hash {
            Ok(())
        } else {
            Err(EnvironmentError::Integrity {
                what: format!("{}'s content", root.display()),
                expected: self.content.hash.clone(),
            })
        }
    }

    fn check(&self) -> Result<()> {
        let bad = |message: String| Err(EnvironmentError::Manifest(message));
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
    /// The files' `/`-separated paths, relative to the directory, sorted.
    pub paths: Vec<String>,
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
        let contents = std::fs::read(path).map_err(io(format!("reading {}", path.display())))?;
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
        paths: paths.into_iter().map(|(relative, _)| relative).collect(),
    })
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(io(format!("reading {}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(io(format!("reading {}", dir.display())))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(io(format!("reading {}", path.display())))?;
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
            return Err(EnvironmentError::Manifest(format!(
                "{}: {} is not a regular file",
                root.display(),
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
            Err(EnvironmentError::Integrity { .. })
        ));

        let mut outside = manifest.clone();
        outside
            .exports
            .insert("evil".to_string(), "../x.js".to_string());
        outside.write(root.path()).unwrap();
        assert!(Manifest::read(root.path()).is_err());
    }

    #[test]
    fn the_hash_covers_the_manifests_identity_and_exports_exist() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("modules")).unwrap();
        std::fs::write(root.path().join("modules/a.js"), "a").unwrap();
        std::fs::write(root.path().join("modules/b.js"), "b").unwrap();
        let new = |exports: &[(&str, &str)]| {
            Manifest::new(
                "env",
                Language::JavaScript,
                Inputs::new(vec![]),
                Builder::new("0.0.0"),
                root.path(),
                "modules",
                exports
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            )
        };
        let manifest = new(&[("a", "modules/a.js")]).unwrap();
        manifest.verify(root.path()).unwrap();

        // Exports must name files that are there.
        assert!(new(&[("x", "modules/missing.js")]).is_err());

        // Changing what the manifest says, not just the files, is caught.
        let mut edited = vec![manifest.clone(); 4];
        edited[0].name = "other".to_string();
        edited[1].language = Language::Shell;
        edited[2]
            .exports
            .insert("a".to_string(), "modules/b.js".to_string());
        edited[3]
            .exports
            .insert("b".to_string(), "modules/b.js".to_string());
        for changed in &edited {
            assert!(
                matches!(
                    changed.verify(root.path()),
                    Err(EnvironmentError::Integrity { .. })
                ),
                "{changed:?}"
            );
        }
        let mut dangling = manifest.clone();
        dangling
            .exports
            .insert("gone".to_string(), "modules/gone.js".to_string());
        assert!(matches!(
            dangling.verify(root.path()),
            Err(EnvironmentError::Manifest(_))
        ));
    }

    /// A fixed environment: two modules, one export.
    fn golden(root: &Path) -> Manifest {
        std::fs::create_dir_all(root.join("modules/sub")).unwrap();
        std::fs::write(root.join("modules/a.js"), "export const a = 1;\n").unwrap();
        std::fs::write(root.join("modules/sub/b.js"), "export const b = 2;\n").unwrap();
        Manifest::new(
            "golden",
            Language::JavaScript,
            Inputs::new(vec!["a@1.0.0".to_string()]),
            Builder::new("0.0.0"),
            root,
            "modules",
            BTreeMap::from([("a".to_string(), "modules/a.js".to_string())]),
        )
        .unwrap()
    }

    /// The identity hash is part of the environment format
    /// (book/src/api/environment-format.md): environments packed by one eryx
    /// are loaded by another, so it must never change by accident. Changing
    /// it means a new MANIFEST_VERSION.
    #[test]
    fn the_identity_hash_is_stable() {
        let root = tempfile::tempdir().unwrap();
        let manifest = golden(root.path());
        assert_eq!(
            manifest.content.hash,
            "sha256-8ff8b91932effce64fd2558d104dcb20164ecc63aa2772e65792e36aa4ab5c4d"
        );
        // What isn't identity doesn't move it.
        let mut informational = manifest.clone();
        informational.inputs = Inputs::new(vec!["other@2".to_string()]);
        informational.builder = Builder::new("9.9.9");
        informational.warnings = vec!["w".to_string()];
        informational.verify(root.path()).unwrap();
    }

    /// Readers ignore fields they don't know, so a newer eryx can add
    /// informational ones without a new format version.
    #[test]
    fn unknown_manifest_fields_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let manifest = golden(root.path());
        let mut json = serde_json::to_value(&manifest).unwrap();
        json["futureField"] = serde_json::json!({"x": 1});
        json["content"]["futureField"] = serde_json::json!(true);
        std::fs::write(root.path().join(MANIFEST_FILE), json.to_string()).unwrap();
        let read = Manifest::read(root.path()).unwrap();
        assert_eq!(read, manifest);
        read.verify(root.path()).unwrap();
    }

    /// The informational fields are optional, so another writer (a different
    /// eryx, or a service) may leave them out.
    #[test]
    fn informational_fields_are_optional() {
        let root = tempfile::tempdir().unwrap();
        let manifest = golden(root.path());
        let mut json = serde_json::to_value(&manifest).unwrap();
        let object = json.as_object_mut().unwrap();
        object.remove("inputs");
        object.remove("builder");
        let content = object["content"].as_object_mut().unwrap();
        content.remove("files");
        content.remove("bytes");
        std::fs::write(root.path().join(MANIFEST_FILE), json.to_string()).unwrap();
        let read = Manifest::read(root.path()).unwrap();
        read.verify(root.path()).unwrap();
        assert_eq!(read.content.hash, manifest.content.hash);
        assert_eq!(read.inputs, Inputs::default());
        assert_eq!(read.builder, Builder::default());

        // And inside them: `inputs: {}` and `builder: {}` load too.
        json["inputs"] = serde_json::json!({});
        json["builder"] = serde_json::json!({});
        std::fs::write(root.path().join(MANIFEST_FILE), json.to_string()).unwrap();
        Manifest::read(root.path())
            .unwrap()
            .verify(root.path())
            .unwrap();
    }

    /// An environment in a format version this eryx doesn't know is refused,
    /// naming both.
    #[test]
    fn unknown_format_versions_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let mut json = serde_json::to_value(golden(root.path())).unwrap();
        json["formatVersion"] = serde_json::json!(MANIFEST_VERSION + 1);
        std::fs::write(root.path().join(MANIFEST_FILE), json.to_string()).unwrap();
        let err = Manifest::read(root.path()).unwrap_err().to_string();
        assert!(
            err.contains(&format!("version {}", MANIFEST_VERSION + 1))
                && err.contains(&format!("reads {MANIFEST_VERSION}")),
            "{err}"
        );
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
