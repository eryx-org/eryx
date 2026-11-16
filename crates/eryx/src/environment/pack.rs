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

    /// Mount `dir` for the bundler instead of the tree itself: a directory
    /// that holds the tree and everything its symlinks point to, such as a
    /// monorepo's root for workspace packages or `file:` dependencies. The
    /// tree must be inside it.
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
/// the packages inside a sandbox that sees only [`PackOptions::root`]
/// (default `tree`), read-only, and the output directory.
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
/// [`EnvironmentError::Build`] if the tree has no `node_modules`, nothing to
/// export, or a package symlink that's absolute or leads outside the root,
/// or if bundling fails.
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
    let node_modules = tree.join("node_modules");
    if !node_modules.is_dir() {
        return Err(build(format!(
            "{} has no node_modules: install the packages first (e.g. `npm ci --ignore-scripts --omit=dev`)",
            tree.display()
        )));
    }
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
    check_links(&node_modules, &root)?;

    let mut entries = if options.entries.is_empty() {
        package_json_dependencies(&tree)?
    } else {
        options.entries.clone()
    };
    entries.sort();
    entries.dedup();
    if entries.is_empty() {
        return Err(build(format!(
            "nothing to export: {}/package.json has no dependencies, and no entries were given",
            tree.display()
        )));
    }

    let staging = staging_dir(out)?;
    let modules = staging.path().join(MODULES_DIR);
    std::fs::create_dir(&modules).map_err(io("creating the modules directory"))?;
    // `modules` is new and empty: the bundler doesn't clear its output dir,
    // so a fresh staging dir is what keeps stale chunks out of the hash.
    let mut report = run_bundler(&root, &cwd, &modules, &entries).await?;
    std::fs::remove_file(modules.join("bundle.json")).map_err(io("removing bundle.json"))?;
    let exports = std::mem::take(&mut report.entries)
        .into_iter()
        .map(|(specifier, chunk)| (specifier, format!("{MODULES_DIR}/{chunk}")))
        .collect();

    let mut inputs = match &options.inputs {
        Some(inputs) => inputs.clone(),
        None => Inputs::new(installed_packages(&node_modules, &entries)),
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
    manifest.warnings = options.warnings.clone();
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

/// `name@version` for each entry's package, as installed at the top of
/// `node_modules` (just `name` if its version can't be read).
fn installed_packages(node_modules: &Path, entries: &[String]) -> Vec<String> {
    let packages: BTreeSet<&str> = entries.iter().map(|e| package_of(e)).collect();
    packages
        .into_iter()
        .map(|name| {
            let version = std::fs::read(node_modules.join(name).join("package.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|json| json.get("version")?.as_str().map(str::to_owned));
            match version {
                Some(version) => format!("{name}@{version}"),
                None => name.to_string(),
            }
        })
        .collect()
}

/// Check the package symlinks the bundler will follow from `node_modules`:
/// each must be relative and resolve inside `root`, which is all the
/// bundler's sandbox can see (it refuses anything else, so this is for a
/// clear error up front). Only the package graph is checked: each
/// `node_modules`' packages (`name` and `@scope/name`, and pnpm's
/// `.pnpm/<id>/node_modules`), and the `node_modules` inside every package
/// reached, through links too. `.bin` and other dot entries, and links that
/// lead nowhere (which nothing can import), are skipped.
fn check_links(node_modules: &Path, root: &Path) -> Result<(), EnvironmentError> {
    let mut pending = vec![node_modules.to_path_buf()];
    let mut seen = BTreeSet::new();
    while let Some(dir) = pending.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        for package in packages_in(&dir)? {
            let real = match check_link(&package, root)? {
                Some(target) => target,
                None if package.is_dir() => package,
                None => continue,
            };
            let nested = real.join("node_modules");
            if nested.is_dir() {
                pending.push(nested);
            }
        }
        let pnpm = dir.join(".pnpm");
        if pnpm.is_dir() {
            for id in read_dir(&pnpm)? {
                let nested = id.join("node_modules");
                if nested.is_dir() {
                    pending.push(nested);
                }
            }
        }
    }
    Ok(())
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
fn check_link(path: &Path, root: &Path) -> Result<Option<PathBuf>, EnvironmentError> {
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
    fn checks_only_the_package_graph() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let nm = root.join("app/node_modules");
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
        // A workspace package, a scoped one, and links that don't count.
        package(&root.join("packages/w"));
        symlink("../../packages/w", nm.join("w")).unwrap();
        package(&nm.join("@s/real"));
        std::fs::create_dir_all(nm.join(".bin")).unwrap();
        symlink("/usr/bin/true", nm.join(".bin/tool")).unwrap();
        symlink("missing", nm.join("dangling")).unwrap();
        check_links(&nm, &root).unwrap();
        // The tree alone doesn't hold the workspace package.
        let err = check_links(&nm, &root.join("app")).unwrap_err().to_string();
        assert!(
            err.contains("leads outside the root") && err.contains("--root"),
            "{err}"
        );

        symlink(root.join("packages/w"), nm.join("@s/abs")).unwrap();
        let err = check_links(&nm, &root).unwrap_err().to_string();
        assert!(
            err.contains("@s/abs") && err.contains("absolute symlink"),
            "{err}"
        );
        std::fs::remove_file(nm.join("@s/abs")).unwrap();

        // An escaping link deep in pnpm's store is found too.
        symlink("../../../../../..", nm.join(".pnpm/b@1/node_modules/up")).unwrap();
        let err = check_links(&nm, &root).unwrap_err().to_string();
        assert!(err.contains("node_modules/up"), "{err}");
    }
}
