//! Environments: package sets built ahead of time and mounted by sandboxes.
//!
//! An environment is built once, from an [`EnvironmentDefinition`] (a TOML
//! file naming the language and its packages, or one built in code with
//! [`EnvironmentDefinition::new`]), into a directory that
//! sandboxes mount read-only. Sandboxes never see package names or a
//! registry, and building needs the network only to fetch packages, so it
//! belongs in CI or a deploy step:
//!
//! ```text
//! eryx-precompile env build data-tools.toml --out envs/data-tools
//! ```
//!
//! ```no_run
//! # async fn run() -> Result<(), eryx::environment::EnvironmentError> {
//! use eryx::environment::{BuildOptions, EnvironmentDefinition, build};
//!
//! let definition = EnvironmentDefinition::read("data-tools.toml".as_ref())?;
//! let options = BuildOptions::new().out_dir("envs/data-tools").base_dir(".");
//! let environment = build(&definition, &options).await?;
//! println!("{}", environment.manifest().content.hash);
//! # Ok(())
//! # }
//! ```
//!
//! What a build does depends on the language:
//!
//! - **JavaScript** (`[npm]`): resolves and installs the packages (see
//!   [`eryx_packages::npm`]), writing or reusing the lockfile, then bundles
//!   them into ES modules with rolldown. The bundler runs as a WASI command
//!   inside wasmtime, with only the installed packages and its output
//!   directory mounted and no network, so the untrusted package source is
//!   parsed in a sandbox. The environment's `exports` map each importable
//!   specifier to its module.
//! - **Python** and the **shell**: not supported yet
//!   ([`EnvironmentError::Unsupported`]).
//!
//! The bundler is embedded by the `packages` feature. A crates.io build
//! gets it from `eryx-precompile setup --bundler`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

pub use eryx_packages::environment::{
    Builder, Content, EnvironmentDefinition, Inputs, Language, LimitsSection, Manifest, NpmSection,
    Size,
};

use eryx_packages::environment::{MANIFEST_FILE, parse_duration};
use eryx_packages::npm::{Install, Limits, Registry};

use crate::Language as GuestLanguage;
use crate::process::{IsolatedLimits, Mount};

/// The directory a JavaScript environment's modules are in.
const MODULES_DIR: &str = "modules";

/// The lockfile's name inside a JavaScript environment.
const NPM_LOCKFILE: &str = "eryx-npm.lock";

/// Why building or loading an environment failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnvironmentError {
    /// Environments for this language can't be built yet.
    #[error("{0} environments aren't supported yet")]
    Unsupported(Language),
    /// Resolving, fetching or installing the packages failed, or the
    /// definition or manifest is invalid.
    #[error(transparent)]
    Packages(#[from] eryx_packages::Error),
    /// The language's build step (e.g. the bundler) failed.
    #[error("{0}")]
    Build(String),
    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> EnvironmentError {
    let context = context.into();
    move |source| EnvironmentError::Io { context, source }
}

/// How to build an environment.
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct BuildOptions {
    out_dir: Option<PathBuf>,
    base_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    offline: bool,
    frozen: bool,
}

impl BuildOptions {
    /// Defaults: built into the eryx cache directory, the lockfile kept there
    /// too (see [`base_dir`](Self::base_dir)), online, not frozen.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build into `dir`, replacing an environment already there (moved
    /// aside, then deleted, so `dir` never holds a half-replaced one) or
    /// filling an empty directory in place. Anything else at `dir` is
    /// refused. `dir` mustn't be a mount point that already holds an
    /// environment, which can't be renamed: mount its parent, or an empty
    /// directory. Without one, environments go to the eryx cache directory,
    /// named by a hash of what they're built from.
    pub fn out_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.out_dir = Some(dir.into());
        self
    }

    /// Resolve the definition's relative paths (its lockfile) against `dir`,
    /// usually the definition file's directory.
    ///
    /// Without one, the lockfile goes in the cache directory
    /// (`<cache>/packages/lockfiles/`, or `lockfiles/` under
    /// [`cache_dir`](Self::cache_dir)), never the process's current
    /// directory, so a server whose working directory is read-only still
    /// builds. A lockfile there isn't shared across machines, though, so two
    /// builds can resolve different versions. For reproducible builds,
    /// commit the lockfile next to the definition, point `base_dir` at it,
    /// and build with [`frozen`](Self::frozen).
    pub fn base_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(dir.into());
        self
    }

    /// Cache downloads and installs in `dir` instead of the eryx cache
    /// directory.
    pub fn cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    /// Use only the lockfile and the cache; never touch the network.
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// Fail instead of resolving when the lockfile is missing or out of date.
    pub fn frozen(mut self, frozen: bool) -> Self {
        self.frozen = frozen;
        self
    }
}

/// A built environment: its directory and manifest.
#[derive(Debug, Clone)]
pub struct Environment {
    root: PathBuf,
    manifest: Manifest,
    language: GuestLanguage,
}

impl Environment {
    /// Load the environment in `dir`, checking its manifest, that its
    /// exports exist, and that it still hashes to what the manifest records
    /// (see [`Manifest::verify`] for what that does and doesn't guarantee).
    ///
    /// # Errors
    ///
    /// Fails if there's no valid manifest, the environment was changed, or
    /// it's for a language this eryx doesn't know.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, EnvironmentError> {
        let root = dir.as_ref().to_path_buf();
        let manifest = Manifest::read(&root)?;
        manifest.verify(&root)?;
        let language = guest_language(manifest.language)?;
        Ok(Self {
            root,
            manifest,
            language,
        })
    }

    /// [`load`](Self::load), and check it's the environment whose
    /// [`Content::hash`] is `hash`: pin one built in CI, so a sandbox can't
    /// be handed another environment at the same path.
    ///
    /// # Errors
    ///
    /// As for [`load`](Self::load), or if it's another environment.
    pub fn load_expecting(dir: impl AsRef<Path>, hash: &str) -> Result<Self, EnvironmentError> {
        let environment = Self::load(&dir)?;
        if environment.manifest.content.hash == hash {
            Ok(environment)
        } else {
            Err(eryx_packages::Error::Integrity {
                what: format!("the environment at {}", dir.as_ref().display()),
                expected: hash.to_string(),
            }
            .into())
        }
    }

    /// The environment's directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Its manifest.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The guest language it's for.
    #[must_use]
    pub fn language(&self) -> GuestLanguage {
        self.language
    }

    /// What it makes importable, by name, as paths inside [`root`](Self::root).
    #[must_use]
    pub fn exports(&self) -> &BTreeMap<String, String> {
        &self.manifest.exports
    }
}

fn guest_language(language: Language) -> Result<GuestLanguage, EnvironmentError> {
    match language {
        Language::JavaScript => Ok(GuestLanguage::JavaScript),
        Language::Python => Ok(GuestLanguage::Python),
        Language::Shell => Ok(GuestLanguage::Shell),
        other => Err(EnvironmentError::Unsupported(other)),
    }
}

/// Build the environment `definition` describes.
///
/// # Errors
///
/// [`EnvironmentError::Unsupported`] for languages without a backend yet,
/// otherwise whatever the build step reports.
pub async fn build(
    definition: &EnvironmentDefinition,
    options: &BuildOptions,
) -> Result<Environment, EnvironmentError> {
    // A definition built in code hasn't been through `from_toml`'s checks.
    definition.check()?;
    match (definition.language, &definition.npm) {
        (Language::JavaScript, Some(npm)) => build_javascript(definition, npm, options).await,
        (other, _) => Err(EnvironmentError::Unsupported(other)),
    }
}

async fn build_javascript(
    definition: &EnvironmentDefinition,
    npm: &NpmSection,
    options: &BuildOptions,
) -> Result<Environment, EnvironmentError> {
    let cache_dir = match &options.cache_dir {
        Some(dir) => dir.clone(),
        None => crate::cache::default_cache_root()
            .map_err(|e| EnvironmentError::Build(e.to_string()))?
            .join("packages"),
    };
    // Not the current directory: a library or server caller may run anywhere.
    let base_dir = match &options.base_dir {
        Some(dir) => dir.clone(),
        None => {
            let dir = cache_dir.join("lockfiles");
            std::fs::create_dir_all(&dir)
                .map_err(|e| EnvironmentError::Build(format!("creating {}: {e}", dir.display())))?;
            dir
        }
    };
    let lockfile = base_dir.join(
        npm.lockfile
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("{}.lock", definition.name))),
    );

    let mut registry = match &npm.registry {
        Some(url) => Registry::new(url)?,
        None => Registry::npmjs(),
    };
    for host in &npm.tarball_hosts {
        registry = registry.allow_tarball_host(host);
    }
    let mut limits = Limits::default();
    if let Some(max) = definition.limits.max_packages {
        limits.max_packages = max;
    }
    if let Some(Size(max)) = definition.limits.max_package_bytes {
        limits.max_tarball_bytes = max;
    }
    if let Some(Size(max)) = definition.limits.max_total_bytes {
        limits.max_total_bytes = max;
    }
    let mut install = Install::new(&npm.packages, &cache_dir)
        .registry(registry.clone())
        .lockfile(&lockfile)
        .offline(options.offline)
        .frozen(options.frozen)
        .limits(limits);
    if let Some(age) = &npm.min_release_age {
        install =
            install.min_release_age(parse_duration(age).map_err(eryx_packages::Error::Definition)?);
    }
    let installed = install.run().await?;

    // What's importable: each package's root, plus the extra entries.
    let mut entries: Vec<String> = npm
        .packages
        .iter()
        .map(|spec| eryx_packages::npm::Spec::parse(spec).map(|s| s.name))
        .collect::<Result<_, _>>()?;
    entries.extend(npm.entries.iter().cloned());
    entries.sort();
    entries.dedup();

    let out_dir = match &options.out_dir {
        Some(dir) => dir.clone(),
        None => {
            let key = blake3::hash(
                format!(
                    "{}\0{}\0{}\0{}",
                    definition.name,
                    installed.lockfile.content_hash(),
                    env!("ERYX_EMBEDDED_BUNDLER_HASH"),
                    entries.join("\0")
                )
                .as_bytes(),
            );
            let dir = cache_dir.join("environments").join(&key.to_hex()[..32]);
            if let Ok(existing) = Environment::load(&dir) {
                return Ok(existing);
            }
            dir
        }
    };

    let cached = options.out_dir.is_none();
    let staging = staging_dir(&out_dir)?;
    let modules = staging.path().join(MODULES_DIR);
    std::fs::create_dir(&modules).map_err(io("creating the modules directory"))?;

    let tree_root = installed
        .node_modules
        .parent()
        .ok_or_else(|| EnvironmentError::Build("installed tree has no root".to_string()))?;
    // `modules` is new and empty: the bundler doesn't clear its output dir,
    // so a fresh staging dir is what keeps stale chunks out of the hash.
    let mut report = run_bundler(tree_root, &modules, &entries).await?;
    std::fs::remove_file(modules.join("bundle.json")).map_err(io("removing bundle.json"))?;

    let exports = std::mem::take(&mut report.entries)
        .into_iter()
        .map(|(specifier, chunk)| (specifier, format!("{MODULES_DIR}/{chunk}")))
        .collect();
    installed
        .lockfile
        .write(&staging.path().join(NPM_LOCKFILE))?;
    let mut inputs = Inputs::new(installed.lockfile.requested.clone());
    inputs.registry = Some(registry.url().to_string());
    inputs.lockfile = Some(NPM_LOCKFILE.to_string());
    inputs.lockfile_hash = Some(installed.lockfile.content_hash());
    let mut builder = Builder::new(env!("CARGO_PKG_VERSION"));
    builder.tools.insert("bundler".to_string(), report.bundler);
    let mut manifest = Manifest::new(
        &definition.name,
        definition.language,
        inputs,
        builder,
        staging.path(),
        MODULES_DIR,
        exports,
    )?;
    manifest.warnings = installed.warnings.clone();
    manifest
        .warnings
        .extend(report.warnings.iter().map(|w| format!("bundler: {w}")));
    manifest.write(staging.path())?;

    install_environment(staging, &out_dir, cached, &manifest.content.hash)?;
    Environment::load_expecting(&out_dir, &manifest.content.hash)
}

/// Whether `dir` is an existing, empty directory.
fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

/// Where to build `out`: inside it if it's an existing empty directory (a
/// volume mounted there, say), so the result can be moved in on the same
/// filesystem; otherwise next to it, to be renamed into place. An `out`
/// that holds anything other than an environment is refused up front.
fn staging_dir(out: &Path) -> Result<tempfile::TempDir, EnvironmentError> {
    let staging = |dir: &Path| {
        tempfile::Builder::new()
            .prefix(".eryx-staging-")
            .tempdir_in(dir)
            .map_err(io(format!("creating a directory in {}", dir.display())))
    };
    if is_empty_dir(out) {
        return staging(out);
    }
    if out.exists() && !out.join(MANIFEST_FILE).is_file() {
        return Err(EnvironmentError::Build(format!(
            "{} exists and isn't an environment; not replacing it",
            out.display()
        )));
    }
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(io(format!("creating {}", parent.display())))?;
    staging(parent)
}

/// Put the environment built in `staging` at `out`.
///
/// - `out` was empty (`staging` is inside it): move the contents up, the
///   manifest last, so it never describes files that aren't there yet.
/// - `out` doesn't exist: rename `staging` to it.
/// - `out` holds an environment: if `cached` and it's the one just built
///   (`hash`), another build got there first, so keep it. Otherwise move it
///   aside, rename `staging` in, and delete it, so there's never a
///   half-deleted environment at `out` (only, briefly, none).
///
/// `out` must not be a mount point holding an environment, since that can't
/// be renamed: mount its parent instead, or an empty directory.
fn install_environment(
    staging: tempfile::TempDir,
    out: &Path,
    cached: bool,
    hash: &str,
) -> Result<(), EnvironmentError> {
    if staging.path().parent() == Some(out) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(staging.path())
            .map_err(io("reading the staging directory"))?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<Result<_, _>>()
            .map_err(io("reading the staging directory"))?;
        entries.sort_by_key(|p| p.ends_with(MANIFEST_FILE));
        for entry in entries {
            let name = entry.file_name().unwrap_or_default();
            std::fs::rename(&entry, out.join(name))
                .map_err(io(format!("moving the build into {}", out.display())))?;
        }
        return Ok(());
    }

    let staged = staging.keep();
    let discard = |staged: &Path| {
        let _ = std::fs::remove_dir_all(staged);
    };
    // A few attempts: a concurrent build can replace `out` in between.
    for _ in 0..8 {
        match std::fs::rename(&staged, out) {
            Ok(()) => return Ok(()),
            Err(e) if !out.exists() => {
                if !staged.exists() {
                    return Err(io(format!("installing {}", out.display()))(e));
                }
                continue;
            }
            Err(_) => {}
        }
        if cached && Environment::load_expecting(out, hash).is_ok() {
            discard(&staged);
            return Ok(());
        }
        if !out.join(MANIFEST_FILE).is_file() {
            discard(&staged);
            return Err(EnvironmentError::Build(format!(
                "{} exists and isn't an environment; not replacing it",
                out.display()
            )));
        }
        let aside = aside_path(out);
        match std::fs::rename(out, &aside) {
            Ok(()) => {}
            // Another build moved it first; try again.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                discard(&staged);
                return Err(EnvironmentError::Build(format!(
                    "can't replace the environment at {}: {e}. If it's a mount point, \
                     mount its parent (or an empty directory) instead",
                    out.display()
                )));
            }
        }
        let swapped = std::fs::rename(&staged, out);
        if swapped.is_err() {
            // Put the old one back unless another build already replaced it.
            let _ = std::fs::rename(&aside, out);
        } else {
            let _ = std::fs::remove_dir_all(&aside);
        }
        match swapped {
            Ok(()) => return Ok(()),
            Err(_) if out.exists() => {}
            Err(e) => {
                discard(&staged);
                return Err(io(format!("installing {}", out.display()))(e));
            }
        }
    }
    discard(&staged);
    Err(EnvironmentError::Build(format!(
        "{} kept changing while installing; is another build replacing it?",
        out.display()
    )))
}

/// A unique name next to `out` to move the environment there aside to.
fn aside_path(out: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = out.file_name().unwrap_or_default().to_string_lossy();
    out.with_file_name(format!(
        ".{name}.old-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// `bundle.json`, as the bundler writes it.
#[derive(serde::Deserialize)]
struct BundleReport {
    bundler: String,
    entries: BTreeMap<String, String>,
    #[serde(default)]
    warnings: Vec<String>,
}

/// Bundle `entries` from the tree at `tree_root` into `out`.
async fn run_bundler(
    tree_root: &Path,
    out: &Path,
    entries: &[String],
) -> Result<BundleReport, EnvironmentError> {
    let mut argv: Vec<String> = ["eryx-bundler", "--cwd", "/src", "--out", "/out"]
        .iter()
        .map(ToString::to_string)
        .collect();
    for entry in entries {
        argv.push("--entry".to_string());
        argv.push(entry.clone());
    }
    let mounts = [
        Mount {
            host: tree_root,
            guest: "/src",
            writable: false,
        },
        Mount {
            host: out,
            guest: "/out",
            writable: true,
        },
    ];
    let limits = IsolatedLimits {
        memory: 1024 * 1024 * 1024,
        fuel: 500_000_000_000,
        timeout: Duration::from_secs(5 * 60),
        output: 1024 * 1024,
    };
    let (code, _stdout, stderr) = bundler()?
        .run_isolated(&argv, &mounts, &limits)
        .await
        .map_err(|e| EnvironmentError::Build(format!("bundling: {e}")))?;
    if code != 0 {
        return Err(EnvironmentError::Build(format!(
            "bundling failed:\n{}",
            String::from_utf8_lossy(&stderr).trim_end()
        )));
    }
    let report = std::fs::read(out.join("bundle.json")).map_err(io("reading bundle.json"))?;
    serde_json::from_slice(&report)
        .map_err(|e| EnvironmentError::Build(format!("the bundler's bundle.json: {e}")))
}

/// The embedded bundler, loaded on first use.
fn bundler() -> Result<crate::Command, EnvironmentError> {
    static BUNDLER: OnceLock<Result<crate::Command, String>> = OnceLock::new();
    BUNDLER
        .get_or_init(|| {
            let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/eryx-bundler.cwasm"));
            if bytes.is_empty() {
                return Err("this eryx was built without the bundler (a placeholder is \
                            embedded): build it with `mise run precompile-bundler`"
                    .to_string());
            }
            // SAFETY: the build script embeds a bundler that eryx's precompile
            // produced, as it does the runtimes.
            #[allow(unsafe_code)]
            unsafe { crate::Command::from_precompiled_isolated(bytes) }.map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| EnvironmentError::Build(format!("the embedded bundler: {e}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn other_languages_are_unsupported() {
        for language in ["python", "shell"] {
            let definition = EnvironmentDefinition::from_toml(&format!(
                "name = \"x\"\nlanguage = \"{language}\""
            ))
            .unwrap();
            let err = build(&definition, &BuildOptions::new()).await.unwrap_err();
            assert!(
                matches!(err, EnvironmentError::Unsupported(_)),
                "{language}: {err}"
            );
        }
    }

    #[test]
    fn refuses_to_replace_a_non_environment() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("precious"), "x").unwrap();
        assert!(staging_dir(&out).is_err());
        assert!(out.join("precious").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn stages_inside_an_empty_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("volume");
        std::fs::create_dir(&out).unwrap();
        let staging = staging_dir(&out).unwrap();
        assert_eq!(staging.path().parent(), Some(out.as_path()));
        std::fs::write(staging.path().join(MANIFEST_FILE), "{}").unwrap();
        std::fs::create_dir(staging.path().join("modules")).unwrap();
        install_environment(staging, &out, false, "unused").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["eryx-environment.json", "modules"]);
    }
}
