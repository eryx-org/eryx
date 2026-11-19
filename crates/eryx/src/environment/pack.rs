//! Packing environments from an installed `node_modules` tree (the `env-pack`
//! feature), without resolving or fetching anything.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use super::bundler::{MODULES_DIR, install_environment, io, run_bundler, staging_dir};
use super::{Builder, Environment, EnvironmentError, Inputs, Language, Manifest};

/// How to [`pack`] an environment.
#[derive(Debug, Clone)]
#[must_use]
pub struct PackOptions {
    name: String,
    language: Language,
    root: Option<PathBuf>,
    entries: Vec<String>,
    inputs: Option<Inputs>,
    lockfile: Option<PathBuf>,
    warnings: Vec<String>,
    allow_unresolved: Vec<String>,
}

impl PackOptions {
    /// An environment called `name`, for `language` (only JavaScript packs
    /// today).
    pub fn new(name: impl Into<String>, language: Language) -> Self {
        Self {
            name: name.into(),
            language,
            root: None,
            entries: Vec::new(),
            inputs: None,
            lockfile: None,
            warnings: Vec::new(),
            allow_unresolved: Vec::new(),
        }
    }

    /// Look for packages from the tree up to `dir` instead of in the tree
    /// alone: a directory that holds the tree, its hoisted `node_modules`
    /// and everything its package symlinks lead to, such as a monorepo's
    /// root for workspace members and `file:` dependencies. The tree must be
    /// inside it.
    ///
    /// The bundler still sees only those `node_modules` dirs and the
    /// packages they link to (whole, so a workspace member's own files
    /// too), never the rest of `dir`.
    pub fn root(mut self, dir: impl Into<PathBuf>) -> Self {
        self.root = Some(dir.into());
        self
    }

    /// Make `specifier` importable: `zod`, `date-fns/format`, `@scope/pkg`.
    /// Without any, the tree's `package.json` `dependencies` are.
    pub fn entry(mut self, specifier: impl Into<String>) -> Self {
        self.entries.push(specifier.into());
        self
    }

    /// What the environment was built from, recorded in its manifest but not
    /// part of its hash. Without it, the entries' packages at the versions
    /// installed in the tree.
    pub fn inputs(mut self, inputs: Inputs) -> Self {
        self.inputs = Some(inputs);
        self
    }

    /// Copy `path` (a `package-lock.json`, say) into the environment, and
    /// record it and its hash in the inputs.
    pub fn lockfile(mut self, path: impl Into<PathBuf>) -> Self {
        self.lockfile = Some(path.into());
        self
    }

    /// Let imports of `specifier` (`bufferutil`, or a subpath) stay
    /// unresolved: left external, with a warning in the manifest, so they
    /// fail only if the code imports them at run time. Imports a package
    /// declares optional (`optionalDependencies`, or
    /// `peerDependenciesMeta.<name>.optional`) are allowed already; any
    /// other unresolved import fails the pack.
    pub fn allow_unresolved(mut self, specifier: impl Into<String>) -> Self {
        self.allow_unresolved.push(specifier.into());
        self
    }

    /// Warnings to record in the manifest, such as a resolver's.
    pub fn warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }
}

/// Pack the installed packages in `tree` (a directory with a `node_modules`,
/// as `npm ci --ignore-scripts` or pnpm leave it) into an environment at
/// `out`. `out` is replaced if it holds an environment, filled if it's an
/// empty directory, and refused otherwise.
///
/// Nothing is fetched and nothing from a package runs: the bundler parses
/// the packages inside a sandbox that sees only, read-only, the
/// `node_modules` dirs from `tree` up to [`PackOptions::root`] (default
/// `tree`) and the packages they link to, plus the output directory. So a
/// package can't import the app's own files.
///
/// With entries defaulted from `package.json`, dependencies with nothing to
/// import (types-only or CLI-only packages) are skipped, with a warning.
///
/// ```no_run
/// # async fn run() -> Result<(), eryx::environment::EnvironmentError> {
/// use eryx::environment::{Language, PackOptions, pack};
///
/// let options = PackOptions::new("data-tools", Language::JavaScript).entry("date-fns/format");
/// let environment = pack("app".as_ref(), "envs/data-tools".as_ref(), &options).await?;
/// println!("{}", environment.manifest().content.hash);
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// [`EnvironmentError::Unsupported`] for languages other than JavaScript;
/// [`EnvironmentError::Build`] if there's no `node_modules` from the tree up
/// to the root, nothing to export, or a package symlink that's absolute,
/// leads outside the root or to a directory holding the tree, or if
/// bundling fails: a package importing a file outside what the bundler
/// sees, or anything the installed packages don't provide.
pub async fn pack(
    tree: &Path,
    out: &Path,
    options: &PackOptions,
) -> Result<Environment, EnvironmentError> {
    if options.language != Language::JavaScript {
        return Err(EnvironmentError::Unsupported(options.language));
    }
    let build = |message: String| EnvironmentError::Build(message);
    let tree = tree
        .canonicalize()
        .map_err(io(format!("reading {}", tree.display())))?;
    let root = match &options.root {
        Some(root) => root
            .canonicalize()
            .map_err(io(format!("reading {}", root.display())))?,
        None => tree.clone(),
    };
    let cwd = guest_path(&root, &tree).ok_or_else(|| {
        build(format!(
            "{} isn't inside the root {}",
            tree.display(),
            root.display()
        ))
    })?;
    let layout = layout(&tree, &root)?;

    let mut warnings = options.warnings.clone();
    let mut entries = if options.entries.is_empty() {
        let mut entries = Vec::new();
        for dependency in package_json_dependencies(&tree)? {
            match layout.package_dir(&dependency) {
                Some(dir) if !importable(&dir) => warnings.push(format!(
                    "{dependency} isn't exported: it has nothing to import (no exports, main, \
                     module, browser or index file), like a types-only or CLI-only package"
                )),
                _ => entries.push(dependency),
            }
        }
        entries
    } else {
        options.entries.clone()
    };
    entries.sort();
    entries.dedup();
    if entries.is_empty() {
        return Err(build(format!(
            "nothing to export: {}/package.json has no importable dependencies, and no entries \
             were given",
            tree.display()
        )));
    }

    let staging = staging_dir(out)?;
    let modules = staging.path().join(MODULES_DIR);
    std::fs::create_dir(&modules).map_err(io("creating the modules directory"))?;
    // `modules` is new and empty: the bundler doesn't clear its output dir,
    // so a fresh staging dir is what keeps stale chunks out of the hash.
    // The bundler mounts a mirror of just the node_modules dirs and the
    // packages they link to, at their places relative to the root: never
    // the rest of the tree or the root, so a package can't import the
    // app's files (a `../../service-account.json`) into the environment,
    // and every link stays inside the one mount (the sandbox won't follow a
    // link out of a mount), with Node's real-path resolution for pnpm's
    // store and workspace members. Hard links, so it costs next to nothing
    // on the same filesystem; it's beside the staging dir for that reason.
    // A TempDir: removed when dropped, so on success, on every error return
    // below, and on a panic.
    let source = tempfile::Builder::new()
        .prefix(".eryx-pack-")
        .tempdir_in(staging.path().parent().unwrap_or(Path::new(".")))
        .map_err(io("creating the bundler's source dir"))?;
    mirror(&layout, &root, source.path())?;
    let cwd_dir = source
        .path()
        .join(tree.strip_prefix(&root).unwrap_or(Path::new("")));
    std::fs::create_dir_all(&cwd_dir).map_err(io("creating the bundler's source dir"))?;
    let sources = [(source.path().to_path_buf(), "/src".to_string())];
    let mut report = run_bundler(&sources, &cwd, &modules, &entries).await?;
    drop(source);
    // An import the bundler couldn't resolve would be left external, and
    // fail when the environment is imported, so it fails the pack.
    // ... unless it's one its package declares optional (`ws` tries
    // `bufferutil` in a try/catch) or the caller allowed; those stay external
    // with their warning.
    let unresolved: Vec<&String> = report
        .warnings
        .iter()
        .filter(|w| {
            unresolved_import(w).is_some_and(|(specifier, importer)| {
                !options
                    .allow_unresolved
                    .iter()
                    .any(|allowed| allowed == specifier || allowed == package_of(specifier))
                    && !declared_optional(&root, importer, package_of(specifier))
            })
        })
        .collect();
    if !unresolved.is_empty() {
        return Err(build(format!(
            "some imports can't be resolved from the installed packages, so they'd be \
             missing at run time (allow one with `allow_unresolved`, `--allow-unresolved`, \
             if the code copes without it):\n  {}",
            unresolved
                .iter()
                .map(|w| w.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        )));
    }
    std::fs::remove_file(modules.join("bundle.json")).map_err(io("removing bundle.json"))?;
    let exports = std::mem::take(&mut report.entries)
        .into_iter()
        .map(|(specifier, chunk)| (specifier, format!("{MODULES_DIR}/{chunk}")))
        .collect();

    let mut inputs = match &options.inputs {
        Some(inputs) => inputs.clone(),
        None => Inputs::new(installed_packages(&layout, &entries)),
    };
    if let Some(lockfile) = &options.lockfile {
        let name = lockfile
            .file_name()
            .ok_or_else(|| build(format!("{} isn't a file", lockfile.display())))?;
        let bytes =
            std::fs::read(lockfile).map_err(io(format!("reading {}", lockfile.display())))?;
        std::fs::write(staging.path().join(name), &bytes).map_err(io("copying the lockfile"))?;
        inputs.lockfile = Some(name.to_string_lossy().into_owned());
        inputs.lockfile_hash = Some(format!("sha256-{}", hex::encode(Sha256::digest(&bytes))));
    }
    let mut builder = Builder::new(env!("CARGO_PKG_VERSION"));
    builder.tools.insert("bundler".to_string(), report.bundler);
    let mut manifest = Manifest::new(
        &options.name,
        options.language,
        inputs,
        builder,
        staging.path(),
        MODULES_DIR,
        exports,
    )?;
    manifest.warnings = warnings;
    manifest
        .warnings
        .extend(report.warnings.iter().map(|w| format!("bundler: {w}")));
    manifest.write(staging.path())?;

    install_environment(staging, out)?;
    Environment::load_expecting(out, &manifest.content.hash)
}

/// `path` as the bundler sees it with `root` mounted at `/src`, if it's
/// inside `root`. Both are canonical.
fn guest_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut guest = String::from("/src");
    for component in relative.components() {
        guest.push('/');
        guest.push_str(component.as_os_str().to_str()?);
    }
    Some(guest)
}

/// The specifier and importing file (a guest path without its leading `/`)
/// of a bundler warning about an import it couldn't resolve: `Could not
/// resolve 'x' in src/node_modules/a/index.js`.
fn unresolved_import(warning: &str) -> Option<(&str, &str)> {
    let rest = warning.strip_prefix("Could not resolve '")?;
    let (specifier, importer) = rest.split_once("' in ")?;
    Some((specifier, importer))
}

/// Whether the package holding `importer` (a guest path, `src/…`, under
/// `root`) declares `name` optional: in `optionalDependencies`, or as an
/// optional peer (`peerDependenciesMeta.<name>.optional`).
fn declared_optional(root: &Path, importer: &str, name: &str) -> bool {
    let Some(relative) = importer.strip_prefix("src/") else {
        return false;
    };
    let file = root.join(relative);
    for dir in file
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
    {
        let Ok(bytes) = std::fs::read(dir.join("package.json")) else {
            continue;
        };
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return false;
        };
        // The nearest package.json is the importer's package.
        return json
            .get("optionalDependencies")
            .and_then(|deps| deps.get(name))
            .is_some()
            || json
                .pointer(&format!(
                    "/peerDependenciesMeta/{}/optional",
                    name.replace('~', "~0").replace('/', "~1")
                ))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
    }
    false
}

/// The keys of `tree/package.json`'s `dependencies`, if it has any.
fn package_json_dependencies(tree: &Path) -> Result<Vec<String>, EnvironmentError> {
    let path = tree.join("package.json");
    let Ok(text) = std::fs::read(&path) else {
        return Ok(Vec::new());
    };
    let json: serde_json::Value = serde_json::from_slice(&text)
        .map_err(|e| EnvironmentError::Build(format!("{}: {e}", path.display())))?;
    Ok(json
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
        .map(|deps| deps.keys().cloned().collect())
        .unwrap_or_default())
}

/// The package an entry imports from: `@s/p/x` → `@s/p`, `date-fns/format`
/// → `date-fns`.
fn package_of(entry: &str) -> &str {
    let mut parts = entry.splitn(3, '/');
    match (parts.next(), parts.next()) {
        (Some(scope), Some(name)) if scope.starts_with('@') => {
            &entry[..scope.len() + 1 + name.len()]
        }
        (Some(name), _) => name,
        _ => entry,
    }
}

/// `name@version` for each entry's package, as installed in the nearest
/// `node_modules` that has it (just `name` if its version can't be read).
fn installed_packages(layout: &Layout, entries: &[String]) -> Vec<String> {
    let packages: BTreeSet<&str> = entries.iter().map(|e| package_of(e)).collect();
    packages
        .into_iter()
        .map(|name| {
            let version = layout
                .package_dir(name)
                .and_then(|dir| std::fs::read(dir.join("package.json")).ok())
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|json| json.get("version")?.as_str().map(str::to_owned));
            match version {
                Some(version) => format!("{name}@{version}"),
                None => name.to_string(),
            }
        })
        .collect()
}

/// Whether the package in `dir` has anything to import: an entry point in
/// its `package.json` or an index file. Types-only (`@types/*`) and
/// CLI-only packages don't.
fn importable(dir: &Path) -> bool {
    let fields = std::fs::read(dir.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|json| {
            ["exports", "main", "module", "browser"]
                .iter()
                .any(|field| json.get(field).is_some())
        });
    fields
        || ["index.js", "index.mjs", "index.cjs", "index.json"]
            .iter()
            .any(|index| dir.join(index).is_file())
}

/// What the bundler's sandbox sees: a mirror of these paths, at their
/// places relative to the root.
struct Layout {
    /// The `node_modules` dirs from the tree up to the root, nearest first
    /// (where Node resolution looks for the tree's packages), as real paths.
    node_modules: Vec<PathBuf>,
    /// What's mirrored: those `node_modules` (or the links to them), and
    /// every package directory a link leads to outside them (a workspace
    /// member, a `file:` dependency), whole.
    stage: Vec<PathBuf>,
}

impl Layout {
    /// The directory the package `name` is installed in, nearest first.
    fn package_dir(&self, name: &str) -> Option<PathBuf> {
        self.node_modules
            .iter()
            .map(|dir| dir.join(name))
            .find(|dir| dir.is_dir())
    }

    /// Whether `path` is inside something already staged.
    fn covers(&self, path: &Path) -> bool {
        self.stage.iter().any(|staged| path.starts_with(staged))
    }
}

/// Lay out what the bundler sees for `tree`, inside `root`, checking the
/// package symlinks it will follow on the way: each must be relative and
/// resolve inside `root`, to a package rather than a directory holding the
/// tree. Only the package graph is walked: each `node_modules`' packages
/// (`name` and `@scope/name`, and pnpm's `.pnpm/<id>/node_modules`), and the
/// `node_modules` inside every package reached, through links too. `.bin`
/// and other dot entries, and links that lead nowhere (which nothing can
/// import), are skipped.
fn layout(tree: &Path, root: &Path) -> Result<Layout, EnvironmentError> {
    let mut layout = Layout {
        node_modules: Vec::new(),
        stage: Vec::new(),
    };
    for dir in tree.ancestors().take_while(|dir| dir.starts_with(root)) {
        let path = dir.join("node_modules");
        // A node_modules that's itself a link is checked like a package's.
        let real = match check_link(&path, tree, root)? {
            Some(target) => {
                layout.stage.push(path.clone());
                target
            }
            None if path.is_dir() => path,
            None => continue,
        };
        if real.is_dir() {
            layout.node_modules.push(real.clone());
            if !layout.covers(&real) {
                layout.stage.push(real);
            }
        }
    }
    if layout.node_modules.is_empty() {
        return Err(EnvironmentError::Build(format!(
            "no node_modules in {} or above it in the root {}: install the packages first \
             (e.g. `npm ci --ignore-scripts --omit=dev`)",
            tree.display(),
            root.display()
        )));
    }

    let mut pending = layout.node_modules.clone();
    let mut seen = BTreeSet::new();
    while let Some(dir) = pending.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        let mut nested = |package: &Path| {
            let dir = package.join("node_modules");
            if dir.is_dir() {
                pending.push(dir);
            }
        };
        for package in packages_in(&dir)? {
            match check_link(&package, tree, root)? {
                Some(target) => {
                    if !layout.covers(&target) {
                        layout.stage.push(target.clone());
                    }
                    nested(&target);
                }
                None if package.is_dir() => nested(&package),
                None => {}
            }
        }
        let pnpm = dir.join(".pnpm");
        if pnpm.is_dir() {
            for id in read_dir(&pnpm)? {
                nested(&id);
            }
        }
    }
    // A package staged before a directory that holds it is mirrored with it.
    let stage = layout.stage.clone();
    layout.stage.retain(|path| {
        !stage
            .iter()
            .any(|other| other != path && path.starts_with(other) && !other.is_symlink())
    });
    Ok(layout)
}

/// Mirror `layout`'s paths from under `root` into `dest`: directories made,
/// files hard-linked (copied where that fails, e.g. across filesystems),
/// symlinks recreated as they are. So every link the bundler follows stays
/// inside the one directory it mounts, and nothing else of `root` is there.
fn mirror(layout: &Layout, root: &Path, dest: &Path) -> Result<(), EnvironmentError> {
    for path in &layout.stage {
        let relative = path.strip_prefix(root).map_err(|_| {
            EnvironmentError::Build(format!("{} is outside the root", path.display()))
        })?;
        let target = dest.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(io(format!("creating {}", parent.display())))?;
        }
        mirror_entry(path, &target)?;
    }
    Ok(())
}

fn mirror_entry(from: &Path, to: &Path) -> Result<(), EnvironmentError> {
    let kind = std::fs::symlink_metadata(from)
        .map_err(io(format!("reading {}", from.display())))?
        .file_type();
    if kind.is_symlink() {
        if to.symlink_metadata().is_ok() {
            return Ok(());
        }
        // The exact target, unresolved: a link out of what's mirrored stays
        // dangling in the mirror, or is refused by the mount if it leaves it.
        let link = std::fs::read_link(from).map_err(io(format!("reading {}", from.display())))?;
        return symlink(&link, to).map_err(io(format!("linking {}", to.display())));
    }
    if kind.is_dir() {
        std::fs::create_dir_all(to).map_err(io(format!("creating {}", to.display())))?;
        for entry in read_dir(from)? {
            let name = entry.file_name().unwrap_or_default();
            mirror_entry(&entry, &to.join(name))?;
        }
        return Ok(());
    }
    if kind.is_file() && to.symlink_metadata().is_err() {
        // A hard link shares the user's inode, so the bundler reads the real
        // file, but only through the read-only mount (`run_bundler` mounts
        // sources with `writable: false`), so it can't change it.
        std::fs::hard_link(from, to)
            .or_else(|_| std::fs::copy(from, to).map(|_| ()))
            .map_err(io(format!("copying {}", from.display())))?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(link: &Path, to: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(link, to)
}

/// Windows needs to know whether a link is to a directory.
#[cfg(windows)]
fn symlink(link: &Path, to: &Path) -> std::io::Result<()> {
    let target = to.parent().unwrap_or(Path::new(".")).join(link);
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(link, to)
    } else {
        std::os::windows::fs::symlink_file(link, to)
    }
}

/// The package positions in a `node_modules` directory: `name` and
/// `@scope/name`, skipping dot entries.
fn packages_in(node_modules: &Path) -> Result<Vec<PathBuf>, EnvironmentError> {
    let mut packages = Vec::new();
    for entry in read_dir(node_modules)? {
        let name = entry.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if name.starts_with('@') && entry.is_dir() {
            packages.extend(read_dir(&entry)?);
        } else {
            packages.push(entry);
        }
    }
    Ok(packages)
}

fn read_dir(dir: &Path) -> Result<Vec<PathBuf>, EnvironmentError> {
    let mut entries = std::fs::read_dir(dir)
        .map_err(io(format!("reading {}", dir.display())))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(io(format!("reading {}", dir.display())))?;
    entries.sort();
    Ok(entries)
}

/// If `path` is a symlink, check it and return where it leads (canonical);
/// `None` if it isn't one or leads nowhere.
fn check_link(path: &Path, tree: &Path, root: &Path) -> Result<Option<PathBuf>, EnvironmentError> {
    let Ok(target) = std::fs::read_link(path) else {
        return Ok(None);
    };
    let fail = |why: String| {
        Err(EnvironmentError::Build(format!(
            "{} -> {}: {why}",
            path.display(),
            target.display()
        )))
    };
    if target.has_root()
        || target
            .components()
            .any(|c| matches!(c, Component::Prefix(_)))
    {
        return fail(
            "an absolute symlink, which the bundler can't follow; make it relative".to_string(),
        );
    }
    let Ok(resolved) = path.canonicalize() else {
        return Ok(None);
    };
    if !resolved.starts_with(root) {
        return fail(format!(
            "leads outside the root {}; pack with a root that contains it (`--root`)",
            root.display()
        ));
    }
    // A package, not a directory holding the tree (or the root): mounting
    // that would show the bundler everything the layout keeps out.
    if tree.starts_with(&resolved) {
        return fail("leads to a directory holding the tree, not a package".to_string());
    }
    Ok(Some(resolved))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_warnings_parse() {
        assert_eq!(
            unresolved_import(
                "Could not resolve 'bufferutil' in src/node_modules/ws/lib/buffer-util.js"
            ),
            Some(("bufferutil", "src/node_modules/ws/lib/buffer-util.js"))
        );
        assert_eq!(unresolved_import("something else"), None);
    }

    #[test]
    fn entries_name_their_packages() {
        assert_eq!(package_of("zod"), "zod");
        assert_eq!(package_of("date-fns/format"), "date-fns");
        assert_eq!(package_of("@s/p"), "@s/p");
        assert_eq!(package_of("@s/p/x/y"), "@s/p");
    }

    #[test]
    fn guest_paths_are_under_src() {
        let root = Path::new("/repo");
        assert_eq!(guest_path(root, Path::new("/repo")).unwrap(), "/src");
        assert_eq!(
            guest_path(root, Path::new("/repo/apps/web")).unwrap(),
            "/src/apps/web"
        );
        assert!(guest_path(root, Path::new("/elsewhere")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn lays_out_and_checks_only_the_package_graph() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let app = root.join("apps/web");
        let nm = app.join("node_modules");
        let package = |path: &Path| {
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("package.json"), "{}").unwrap();
        };
        // pnpm: a link into .pnpm, whose own dependency is a sibling link.
        package(&nm.join(".pnpm/a@1/node_modules/a"));
        package(&nm.join(".pnpm/b@1/node_modules/b"));
        symlink(
            "../../b@1/node_modules/b",
            nm.join(".pnpm/a@1/node_modules/b"),
        )
        .unwrap();
        symlink(".pnpm/a@1/node_modules/a", nm.join("a")).unwrap();
        // A workspace package with its own link back into the hoisted root
        // node_modules, a scoped one, and links that don't count.
        package(&root.join("packages/w"));
        package(&root.join("node_modules/hoisted"));
        std::fs::create_dir_all(root.join("packages/w/node_modules")).unwrap();
        symlink(
            "../../../node_modules/hoisted",
            root.join("packages/w/node_modules/h"),
        )
        .unwrap();
        symlink("../../../packages/w", nm.join("w")).unwrap();
        package(&nm.join("@s/real"));
        std::fs::create_dir_all(nm.join(".bin")).unwrap();
        symlink("/usr/bin/true", nm.join(".bin/tool")).unwrap();
        symlink("missing", nm.join("dangling")).unwrap();
        std::fs::write(root.join("secret.json"), "{}").unwrap();

        let layout = layout(&app, &root).unwrap();
        assert_eq!(layout.node_modules, [nm.clone(), root.join("node_modules")]);
        let staged: Vec<&Path> = layout
            .stage
            .iter()
            .map(|path| path.strip_prefix(&root).unwrap())
            .collect();
        // The node_modules dirs and the workspace package its link leads
        // to; the hoisted package the member links to is already in the
        // root's node_modules, and pnpm's links stay in theirs. Nothing
        // else of the root.
        assert_eq!(
            staged,
            [
                Path::new("apps/web/node_modules"),
                Path::new("node_modules"),
                Path::new("packages/w"),
            ]
        );
        let mirrored = tempfile::tempdir().unwrap();
        mirror(&layout, &root, mirrored.path()).unwrap();
        let m = mirrored.path();
        assert!(
            m.join("apps/web/node_modules/.pnpm/b@1/node_modules/b/package.json")
                .is_file()
        );
        assert!(
            m.join("apps/web/node_modules/w/node_modules/h/package.json")
                .is_file(),
            "through the mirror's own links"
        );
        assert!(!m.join("secret.json").exists());
        assert_eq!(
            layout.package_dir("hoisted"),
            Some(root.join("node_modules/hoisted"))
        );

        // The tree alone doesn't hold the workspace package.
        let err = layout_err(&app, &app);
        assert!(
            err.contains("leads outside the root") && err.contains("--root"),
            "{err}"
        );

        symlink(root.join("packages/w"), nm.join("@s/abs")).unwrap();
        let err = layout_err(&app, &root);
        assert!(
            err.contains("@s/abs") && err.contains("absolute symlink"),
            "{err}"
        );
        std::fs::remove_file(nm.join("@s/abs")).unwrap();

        // A link to a directory holding the tree would expose all of it.
        symlink("../../..", nm.join("everything")).unwrap();
        let err = layout_err(&app, &root);
        assert!(err.contains("holding the tree"), "{err}");
        std::fs::remove_file(nm.join("everything")).unwrap();

        // An escaping link deep in pnpm's store is found too.
        symlink(
            "../../../../../../../..",
            nm.join(".pnpm/b@1/node_modules/up"),
        )
        .unwrap();
        let err = layout_err(&app, &root);
        assert!(err.contains("node_modules/up"), "{err}");
    }

    fn layout_err(tree: &Path, root: &Path) -> String {
        match layout(tree, root) {
            Ok(_) => panic!("laid out"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn index_files_and_entry_points_are_importable() {
        let dir = tempfile::tempdir().unwrap();
        let package = |name: &str, json: &str| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("package.json"), json).unwrap();
            path
        };
        assert!(importable(&package("main", r#"{"main": "lib.js"}"#)));
        assert!(importable(&package("exports", r#"{"exports": "./x.js"}"#)));
        let index = package("index", "{}");
        std::fs::write(index.join("index.js"), "").unwrap();
        assert!(importable(&index));
        let types = package("types", r#"{"types": "index.d.ts"}"#);
        std::fs::write(types.join("index.d.ts"), "").unwrap();
        assert!(!importable(&types));
        assert!(!importable(&package("cli", r#"{"bin": {"x": "cli.js"}}"#)));
    }
}
