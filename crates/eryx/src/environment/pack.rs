//! Packing environments from an installed `node_modules` tree (the `packages`
//! feature), without resolving or fetching anything.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use super::build::{MODULES_DIR, install_environment, io, run_bundler, staging_dir};
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
/// bundling fails (including a package importing a file outside what the
/// bundler sees).
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
    // Only the node_modules dirs and the packages they link to are mounted,
    // over an empty skeleton holding the directories down to them: never the
    // rest of the tree or the root, so a package can't import the app's
    // files (a `../../service-account.json`) into the environment.
    let skeleton = tempfile::tempdir().map_err(io("creating the bundler's skeleton"))?;
    for guest in layout.mounts.iter().map(|(_, guest)| guest).chain([&cwd]) {
        let dir = skeleton
            .path()
            .join(guest.trim_start_matches("/src").trim_start_matches('/'));
        std::fs::create_dir_all(&dir).map_err(io("creating the bundler's skeleton"))?;
    }
    let mut sources = vec![(skeleton.path().to_path_buf(), "/src".to_string())];
    sources.extend(layout.mounts.iter().cloned());
    let mut report = run_bundler(&sources, &cwd, &modules, &entries).await?;
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

    install_environment(staging, out, false, &manifest.content.hash)?;
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

/// What the bundler's sandbox sees, read-only, over an empty `/src`.
struct Layout {
    /// The `node_modules` dirs from the tree up to the root, nearest first,
    /// where Node resolution looks for the tree's packages.
    node_modules: Vec<PathBuf>,
    /// Host directories and the guest paths they're mounted at: those
    /// `node_modules`, and every package a link leads to outside the mount
    /// it's in (a workspace member, a `file:` dependency), at the link's
    /// own path, since the sandbox never follows a link out of a mount.
    mounts: Vec<(PathBuf, String)>,
}

impl Layout {
    /// The directory the package `name` is installed in, nearest first.
    fn package_dir(&self, name: &str) -> Option<PathBuf> {
        self.node_modules
            .iter()
            .map(|dir| dir.join(name))
            .find(|dir| dir.is_dir())
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
    let mut node_modules = Vec::new();
    for dir in tree.ancestors().take_while(|dir| dir.starts_with(root)) {
        if let Ok(found) = dir.join("node_modules").canonicalize()
            && found.is_dir()
        {
            node_modules.push(found);
        }
    }
    if node_modules.is_empty() {
        return Err(EnvironmentError::Build(format!(
            "no node_modules in {} or above it in the root {}: install the packages first \
             (e.g. `npm ci --ignore-scripts --omit=dev`)",
            tree.display(),
            root.display()
        )));
    }
    let mut mounts = Vec::new();
    for dir in &node_modules {
        let guest = guest_path(root, dir).ok_or_else(|| {
            EnvironmentError::Build(format!("{} leads outside the root", dir.display()))
        })?;
        mounts.push((dir.clone(), guest));
    }

    // (node_modules dir, its guest path, the mount it's in)
    let mut pending: Vec<(PathBuf, String, usize)> = mounts
        .iter()
        .enumerate()
        .map(|(i, (host, guest))| (host.clone(), guest.clone(), i))
        .collect();
    let mut seen = BTreeSet::new();
    while let Some((dir, guest, mount)) = pending.pop() {
        if !seen.insert((dir.clone(), guest.clone())) {
            continue;
        }
        let (mount_host, mount_guest) = mounts[mount].clone();
        let mut nested = |host: &Path, guest: String, mount: usize| {
            let host = host.join("node_modules");
            if host.is_dir() {
                pending.push((host, format!("{guest}/node_modules"), mount));
            }
        };
        for package in packages_in(&dir)? {
            let package_guest = join_guest(&guest, package.strip_prefix(&dir).unwrap_or(&package));
            match check_link(&package, tree, root)? {
                Some(target) => match target.strip_prefix(&mount_host) {
                    // It stays in its mount (pnpm's store): followed there.
                    Ok(inside) => nested(&target, join_guest(&mount_guest, inside), mount),
                    Err(_) => {
                        mounts.push((target.clone(), package_guest.clone()));
                        nested(&target, package_guest, mounts.len() - 1);
                    }
                },
                None if package.is_dir() => nested(&package, package_guest, mount),
                None => {}
            }
        }
        let pnpm = dir.join(".pnpm");
        if pnpm.is_dir() {
            for id in read_dir(&pnpm)? {
                let name = id.file_name().unwrap_or_default().to_string_lossy();
                nested(&id, format!("{guest}/.pnpm/{name}"), mount);
            }
        }
    }
    Ok(Layout {
        node_modules,
        mounts,
    })
}

/// `guest` with `relative`'s components appended, `/`-separated.
fn join_guest(guest: &str, relative: &Path) -> String {
    let mut joined = guest.to_string();
    for component in relative.components() {
        joined.push('/');
        joined.push_str(&component.as_os_str().to_string_lossy());
    }
    joined
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
        let mounts: Vec<(&Path, &str)> = layout
            .mounts
            .iter()
            .map(|(host, guest)| (host.strip_prefix(&root).unwrap(), guest.as_str()))
            .collect();
        // The node_modules dirs; the workspace package at its link's path;
        // and the hoisted package its own link leads to, at that link's
        // path. pnpm's links stay in their mount. Nothing else of the root.
        assert_eq!(
            mounts,
            [
                (
                    Path::new("apps/web/node_modules"),
                    "/src/apps/web/node_modules"
                ),
                (Path::new("node_modules"), "/src/node_modules"),
                (Path::new("packages/w"), "/src/apps/web/node_modules/w"),
                (
                    Path::new("node_modules/hoisted"),
                    "/src/apps/web/node_modules/w/node_modules/h"
                ),
            ]
        );
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
