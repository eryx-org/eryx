//! Sandbox factory for Python.
//!
//! Provides the `SandboxFactory` class for creating sandboxes with custom packages.
//! The factory bundles packages and pre-imports into a reusable snapshot.

use std::io::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use crate::callback::extract_callbacks;
use crate::error::{InitializationError, eryx_error_to_py};
use crate::net_config::NetConfig;
use crate::resource_limits::ResourceLimits;
use crate::sandbox::{PyOutputHandler, Sandbox, apply_secrets, extract_secrets};
use crate::session::Session;

const FACTORY_MAGIC: &[u8; 4] = b"ERYX";
const FACTORY_VERSION: u32 = 4;

/// A factory for creating sandboxes with custom packages.
///
/// `SandboxFactory` bundles packages and pre-imports into a reusable snapshot,
/// allowing fast creation of sandboxes with those packages already loaded.
///
/// Note: For basic usage without packages, `eryx.Sandbox()` is already fast
/// because the base runtime ships pre-initialized. Use `SandboxFactory` when
/// you need to bundle custom packages.
///
/// Example:
///     # Create a factory with jinja2
///     factory = SandboxFactory(
///         packages=["/path/to/jinja2.whl", "/path/to/markupsafe.whl"],
///         imports=["jinja2"],
///     )
///
///     # Create sandboxes with packages already loaded (~1ms each)
///     sandbox = factory.create_sandbox()
///     result = sandbox.execute('from jinja2 import Template; print(Template("{{ x }}").render(x=42))')
///
///     # Save for reuse across processes
///     factory.save("/path/to/jinja2-factory.bin")
///
///     # Load in another process
///     factory = SandboxFactory.load("/path/to/jinja2-factory.bin")
#[pyclass(module = "eryx")]
pub struct SandboxFactory {
    /// Pre-compiled component artifact shared across sandboxes without copying.
    precompiled: eryx::PrecompiledArtifact,
    /// Path to Python stdlib.
    stdlib_path: PathBuf,
    /// Path to site-packages (if any).
    site_packages_path: Option<PathBuf>,
    /// Extracted packages (kept alive to prevent temp dir cleanup).
    extracted_packages: Arc<Vec<eryx::ExtractedPackage>>,
    /// Callbacks whose declarations were baked into the snapshot (or, for a
    /// loaded factory, the ones the caller says were). `create_sandbox()` and
    /// `create_session()` register them when given no callbacks of their own.
    callbacks: Option<Py<PyAny>>,
    /// Holds the temp directory for data files extracted during `load()`.
    /// Arc-wrapped so the pool can share ownership and outlive the factory.
    data_files_dir: Option<Arc<tempfile::TempDir>>,
    /// What `save()` stores so that another pyeryx version can rebuild the
    /// factory.
    build_inputs: BuildInputs,
    /// Tokio runtime shared across all sandboxes and sessions created by this
    /// factory. Children hold an `Arc` clone, so the runtime outlives the
    /// factory when children still exist.
    runtime: Arc<tokio::runtime::Runtime>,
}

/// Construct a pre-compiled artifact with optional content-safe caching.
fn make_precompiled_artifact(bytes: Vec<u8>, cache: bool) -> eryx::PrecompiledArtifact {
    if cache {
        eryx::PrecompiledArtifact::new_cached(bytes)
    } else {
        eryx::PrecompiledArtifact::new(bytes)
    }
}

/// A factory file opened by [`load_factory_file`].
struct FactoryFile {
    /// The component, or why it can't be used, for example because another
    /// eryx version built it.
    precompiled: Result<eryx::PrecompiledArtifact, eryx::Error>,
    data_files_dir: Option<tempfile::TempDir>,
    data_files_path: Option<PathBuf>,
    build_inputs: BuildInputs,
}

/// Load a factory file: map its component (v1, v3, v4) or read it into
/// memory (legacy v2), and extract its data files.
fn load_factory_file(path: &Path, cache: bool) -> PyResult<FactoryFile> {
    use std::io::{Read as _, Seek as _};

    let load_err = |e: std::io::Error| {
        InitializationError::new_err(format!(
            "failed to load factory from {}: {e}",
            path.display()
        ))
    };
    let mut file = open_trusted_factory_file(path).map_err(load_err)?;

    match read_factory_layout(&mut file).map_err(load_err)? {
        FactoryLayout::V2 => {
            let mut bytes = Vec::new();
            file.seek(std::io::SeekFrom::Start(0))
                .and_then(|_| file.read_to_end(&mut bytes))
                .map_err(load_err)?;
            let (precompiled, data_files_dir, data_files_path) =
                parse_factory_v2(&bytes).map_err(load_err)?;
            Ok(FactoryFile {
                precompiled: Ok(make_precompiled_artifact(precompiled, cache)),
                data_files_dir,
                data_files_path,
                build_inputs: BuildInputs::Missing,
            })
        }
        FactoryLayout::Mapped {
            component_len,
            data_files_len,
            build_inputs_len,
            component_hash,
        } => {
            let data_files = if data_files_len == 0 {
                None
            } else {
                let mut compressed = Vec::new();
                file.seek(std::io::SeekFrom::Start(component_len))
                    .and_then(|_| {
                        (&mut file)
                            .take(data_files_len)
                            .read_to_end(&mut compressed)
                    })
                    .map_err(load_err)?;
                Some(extract_data_files(&compressed).map_err(load_err)?)
            };

            // The build inputs are only needed to save or rebuild the
            // factory, so they're read from this same file then rather than
            // kept in memory.
            let build_inputs = if build_inputs_len == 0 {
                BuildInputs::Missing
            } else {
                let data_files_end = component_len + data_files_len;
                BuildInputs::InFile(StoredSections {
                    file: Mutex::new(file.try_clone().map_err(load_err)?),
                    data_files: component_len..data_files_end,
                    build_inputs: data_files_end..data_files_end + build_inputs_len,
                })
            };

            // SAFETY: `open_trusted_factory_file` checked that only this user
            // or root can modify this file (so its stored hash is as trusted
            // as its code), and `save()` only ever replaces factory files by
            // renaming a new file over them.
            #[allow(unsafe_code)]
            let precompiled = unsafe {
                match (cache, component_hash) {
                    (true, Some(hash)) => {
                        eryx::PrecompiledArtifact::from_file_with_hash(file, component_len, hash)
                    }
                    (true, None) => {
                        eryx::PrecompiledArtifact::from_file_cached(file, component_len)
                    }
                    (false, _) => eryx::PrecompiledArtifact::from_file(file, component_len),
                }
            };

            let (data_files_dir, data_files_path) = data_files.unzip();
            Ok(FactoryFile {
                precompiled,
                data_files_dir,
                data_files_path,
                build_inputs,
            })
        }
    }
}

/// Check that a loaded component works with this eryx version.
///
/// On Unix a file-backed component is already deserialized, and this links
/// it once; elsewhere it deserializes the component too. Either way a
/// component from another eryx version fails here rather than in the first
/// `create_sandbox()`.
fn check_compatible(precompiled: &eryx::PrecompiledArtifact) -> Result<(), eryx::Error> {
    // SAFETY: the artifact came from a file `open_trusted_factory_file`
    // accepted, or from a build in this process.
    #[allow(unsafe_code)]
    unsafe { precompiled.to_executor() }.map(drop)
}

/// The error for a factory file that another eryx version built.
fn incompatible_error(path: &Path, err: &eryx::Error, build_inputs: &BuildInputs) -> PyErr {
    let hint = if matches!(build_inputs, BuildInputs::Missing) {
        "It was saved before factories stored their build inputs, so it can't \
         be rebuilt automatically: recreate it with SandboxFactory(...) and \
         save it again"
    } else {
        "Pass rebuild_if_incompatible=True to SandboxFactory.load() to rebuild \
         it from the build inputs it stores"
    };
    InitializationError::new_err(format!(
        "failed to load factory from {}: {err}. {hint}",
        path.display()
    ))
}

/// Where a rebuilt copy of a factory file is cached: the per-user eryx cache
/// directory, keyed by the file's data files and build inputs and by this
/// pyeryx version.
///
/// pyeryx shares eryx's version, which is what a saved factory's
/// compatibility follows. Keying by version keeps processes of different
/// versions sharing a cache directory from replacing each other's copies.
fn rebuilt_factory_path(data_files: &[u8], build_inputs: &[u8]) -> Result<PathBuf, eryx::Error> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(data_files.len() as u64).to_le_bytes());
    hasher.update(data_files);
    hasher.update(build_inputs);
    Ok(eryx::cache::default_cache_root()?
        .join("factories")
        .join(format!(
            "{}-pyeryx-{}.bin",
            hasher.finalize().to_hex(),
            env!("CARGO_PKG_VERSION")
        )))
}

/// Everything [`SandboxFactory::rebuild`] needs from a failed `load()`.
struct RebuildRequest<'a> {
    path: &'a Path,
    error: eryx::Error,
    data_files_dir: Option<tempfile::TempDir>,
    data_files_path: Option<PathBuf>,
    build_inputs: BuildInputs,
    site_packages: Option<PathBuf>,
    cache: bool,
}

/// Pre-initialize and pre-compile a factory's component: the slow part of
/// creating a factory, shared by `SandboxFactory(...)` and rebuilding a saved
/// one.
fn build_component(
    py: Python<'_>,
    runtime: &tokio::runtime::Runtime,
    stdlib_path: &Path,
    site_packages: Option<&Path>,
    extensions: Vec<eryx::preinit::NativeExtension>,
    recipe: &FactoryRecipe,
    cache: bool,
) -> PyResult<eryx::PrecompiledArtifact> {
    let mut options = eryx::preinit::PreInitOptions::new(stdlib_path)
        .imports(recipe.imports.clone())
        .extensions(extensions)
        .callbacks(recipe.callbacks.clone());
    if let Some(path) = site_packages {
        options = options.site_packages(path);
    }
    if let Some(code) = &recipe.setup_code {
        options = options.setup_code(code.clone());
    }

    // Pre-initialize, then pre-compile to native code for faster
    // instantiation. Both take seconds, so let other Python threads run.
    py.detach(|| {
        let preinit_bytes = runtime.block_on(async {
            eryx::preinit::pre_initialize_with_options(options)
                .await
                .map_err(|e| {
                    InitializationError::new_err(format!("pre-initialization failed: {e}"))
                })
        })?;
        let precompiled = eryx::PythonExecutor::precompile(&preinit_bytes)
            .map_err(|e| InitializationError::new_err(format!("pre-compilation failed: {e}")))?;
        Ok(make_precompiled_artifact(precompiled, cache))
    })
}

/// Emit a Python `RuntimeWarning`.
fn warn(py: Python<'_>, message: String) -> PyResult<()> {
    py.import("warnings")?.call_method1(
        "warn",
        (message, py.get_type::<pyo3::exceptions::PyRuntimeWarning>()),
    )?;
    Ok(())
}

#[pymethods]
impl SandboxFactory {
    /// Create a new sandbox factory with custom packages.
    ///
    /// This performs one-time initialization that can take 3-5 seconds,
    /// but subsequent sandbox creation will be very fast (~1ms).
    ///
    /// Args:
    ///     site_packages: Optional path to a directory containing Python packages.
    ///     packages: Optional list of paths to .whl or .tar.gz package files.
    ///         These are extracted and their native extensions are linked.
    ///     imports: Optional list of module names to pre-import during initialization.
    ///         Pre-imported modules are immediately available without import overhead.
    ///     callbacks: Optional callbacks (a CallbackRegistry or a list of callback
    ///         dicts) whose declarations are baked into the snapshot, so sandboxes
    ///         created from this factory skip the per-sandbox callback setup.
    ///         `create_sandbox()` and `create_session()` register these callbacks
    ///         unless given their own. Setup code cannot invoke them.
    ///     cache: Whether to cache the pre-compiled component in the process-global
    ///         cache. When enabled, a BLAKE3 content hash is computed once during
    ///         factory construction and subsequent ``create_sandbox()`` calls skip
    ///         component deserialization entirely (~0.8ms vs ~8ms per call).
    ///         Defaults to True.
    ///
    /// Returns:
    ///     A SandboxFactory ready to create sandboxes with packages.
    ///
    /// Raises:
    ///     InitializationError: If initialization fails.
    ///
    /// Example:
    ///     # Create factory with jinja2 and markupsafe
    ///     factory = SandboxFactory(
    ///         packages=[
    ///             "/path/to/jinja2-3.1.2-py3-none-any.whl",
    ///             "/path/to/markupsafe-2.1.3-wasi.tar.gz",
    ///         ],
    ///         imports=["jinja2"],
    ///     )
    ///
    ///     # With the callbacks every sandbox will register
    ///     factory = SandboxFactory(callbacks=[
    ///         {"name": "get_time", "fn": get_time, "description": "Returns current time"}
    ///     ])
    ///     sandbox = factory.create_sandbox()  # get_time() available, no setup cost
    #[new]
    #[pyo3(signature = (*, site_packages=None, packages=None, imports=None, setup_code=None, callbacks=None, cache=true))]
    fn new(
        py: Python<'_>,
        site_packages: Option<PathBuf>,
        packages: Option<Vec<PathBuf>>,
        imports: Option<Vec<String>>,
        setup_code: Option<String>,
        callbacks: Option<Bound<'_, PyAny>>,
        cache: bool,
    ) -> PyResult<Self> {
        let runtime = crate::error::make_runtime()?;

        // Get embedded resources for stdlib path
        let embedded = eryx::embedded::EmbeddedResources::get().map_err(eryx_error_to_py)?;
        let stdlib_path = embedded.stdlib().to_path_buf();

        // Process packages to extract site-packages and native extensions
        let (final_site_packages, extensions, extracted_packages) =
            process_packages(site_packages.as_ref(), packages.as_ref())?;

        // Bake the callbacks' declarations so sandboxes registering the same
        // set skip the per-sandbox wrapper installation.
        let declarations = match &callbacks {
            Some(cbs) => extract_callbacks(py, cbs)?
                .iter()
                .map(|cb| eryx::preinit::callback_declaration(cb))
                .collect(),
            None => Vec::new(),
        };

        let recipe = FactoryRecipe {
            imports: imports.unwrap_or_default(),
            setup_code,
            callbacks: declarations,
        };
        let precompiled = build_component(
            py,
            &runtime,
            &stdlib_path,
            final_site_packages.as_deref(),
            extensions,
            &recipe,
            cache,
        )?;

        Ok(Self {
            precompiled,
            stdlib_path,
            site_packages_path: final_site_packages,
            extracted_packages: Arc::new(extracted_packages),
            callbacks: callbacks.map(Bound::unbind),
            data_files_dir: None,
            runtime,
            build_inputs: BuildInputs::Recipe(recipe),
        })
    }

    /// Load a sandbox factory from a file.
    ///
    /// This loads a previously saved factory, which is much faster than
    /// creating a new one (~10ms vs ~3-5s).
    ///
    /// The file contains pre-compiled native code that is loaded without
    /// validation, so only load factory files you created yourself (with
    /// ``save()``) and that nobody else can modify. On Unix, files writable by
    /// other users, or owned by a user other than you or root, are refused.
    ///
    /// The pre-compiled component is memory-mapped rather than read into
    /// memory, so processes loading the same file share its pages (except on
    /// Windows, which can't replace a mapped file, so it is read there). Don't
    /// modify a factory file in place while it is loaded; ``save()`` replaces
    /// it atomically, which is safe. For the same reason, never keep a factory
    /// file in a directory mounted into a sandbox as a writable volume.
    ///
    /// Args:
    ///     path: Path to the saved factory file.
    ///     callbacks: The callbacks the factory was created with, if any. The
    ///         file holds their baked declarations but not the Python callables,
    ///         so pass the same callbacks here to get the setup-free fast path;
    ///         a different set still works, it just installs per sandbox.
    ///     cache: Whether to cache the pre-compiled component in the process-global
    ///         cache. When enabled, a BLAKE3 content hash is computed once during
    ///         loading and subsequent ``create_sandbox()`` calls reuse the linked
    ///         component instead of re-linking it per call. Defaults to True.
    ///     rebuild_if_incompatible: What to do with a file saved by another
    ///         pyeryx version, which can't be loaded as is. When False (the
    ///         default), raise ``InitializationError``. When True, rebuild the
    ///         factory from the build inputs the file stores (its packages,
    ///         imports, setup code and callback declarations), which takes as
    ///         long as creating it did. The rebuilt copy is saved to the per-user
    ///         eryx cache directory, so later loads with this flag reuse it; the
    ///         file at ``path`` is never modified. If the copy can't be saved, a
    ///         ``RuntimeWarning`` is emitted and the rebuilt factory is still
    ///         returned. Factories saved before pyeryx 0.13.0 don't store their
    ///         build inputs and can't be rebuilt.
    ///
    /// Returns:
    ///     A SandboxFactory loaded from the file.
    ///
    /// Raises:
    ///     InitializationError: If loading fails, including when another
    ///         pyeryx version saved the file and it isn't rebuilt.
    ///
    /// Example:
    ///     factory = SandboxFactory.load("/path/to/jinja2-factory.bin")
    ///     sandbox = factory.create_sandbox()
    ///
    ///     # Survive pyeryx upgrades without recreating the factory by hand
    ///     factory = SandboxFactory.load(path, rebuild_if_incompatible=True)
    #[staticmethod]
    #[pyo3(signature = (path, *, site_packages=None, callbacks=None, cache=true, rebuild_if_incompatible=false))]
    fn load(
        py: Python<'_>,
        path: PathBuf,
        site_packages: Option<PathBuf>,
        callbacks: Option<Bound<'_, PyAny>>,
        cache: bool,
        rebuild_if_incompatible: bool,
    ) -> PyResult<Self> {
        let FactoryFile {
            precompiled,
            data_files_dir,
            data_files_path,
            build_inputs,
        } = load_factory_file(&path, cache)?;

        // Without the flag a mismatch can surface later, in the first
        // `create_sandbox()`, as it always has. With it, catch it now.
        let precompiled = precompiled.and_then(|precompiled| {
            if rebuild_if_incompatible {
                check_compatible(&precompiled)?;
            }
            Ok(precompiled)
        });
        let precompiled = match precompiled {
            Ok(precompiled) => precompiled,
            Err(error @ eryx::Error::IncompatiblePrecompiled { .. }) if rebuild_if_incompatible => {
                let request = RebuildRequest {
                    path: &path,
                    error,
                    data_files_dir,
                    data_files_path,
                    build_inputs,
                    site_packages,
                    cache,
                };
                return Self::rebuild(py, request, callbacks);
            }
            Err(error @ eryx::Error::IncompatiblePrecompiled { .. }) => {
                return Err(incompatible_error(&path, &error, &build_inputs));
            }
            Err(error) => return Err(eryx_error_to_py(error)),
        };

        let runtime = crate::error::make_runtime()?;
        let embedded = eryx::embedded::EmbeddedResources::get().map_err(eryx_error_to_py)?;
        Ok(Self {
            precompiled,
            stdlib_path: embedded.stdlib().to_path_buf(),
            site_packages_path: site_packages.or(data_files_path),
            extracted_packages: Arc::new(Vec::new()),
            callbacks: callbacks.map(Bound::unbind),
            data_files_dir: data_files_dir.map(Arc::new),
            runtime,
            build_inputs,
        })
    }

    /// Save the sandbox factory to a file.
    ///
    /// The saved file can be loaded later with `SandboxFactory.load()`,
    /// which is much faster than creating a new factory. It only loads in
    /// the pyeryx version that saved it: after upgrading pyeryx, recreate
    /// and re-save the factory.
    ///
    /// Args:
    ///     path: Path where the factory should be saved.
    ///
    /// Raises:
    ///     InitializationError: If saving fails.
    ///
    /// Example:
    ///     factory = SandboxFactory(packages=[...], imports=["jinja2"])
    ///     factory.save("/path/to/jinja2-factory.bin")
    fn save(&self, path: PathBuf) -> PyResult<()> {
        self.save_to(&path).map_err(|e| {
            InitializationError::new_err(format!(
                "failed to save factory to {}: {e}",
                path.display()
            ))
        })
    }

    /// Create a persistent Python session from this factory's preinitialized
    /// runtime. The session preserves interpreter and module state across
    /// executions while remaining isolated from other sessions and sandboxes.
    /// The factory remains disposable; extracted package ownership is retained
    /// by the returned session.
    ///
    /// Args:
    ///     vfs: Optional caller-owned VFS storage. Its policy and quota remain
    ///         owned by the caller.
    ///     vfs_mount_path: Mount path used when `vfs` or `volumes` enable a
    ///         wrapper-visible VFS; alone it does not expose `Session.vfs`.
    ///     resource_limits: Optional session limits. Defaults to
    ///         `ResourceLimits()` (30-second execution timeout, 128 MB memory).
    ///     network: Optional network configuration.
    ///     callbacks: Optional callbacks that sandboxed code can invoke.
    ///     mcp: Optional MCPManager whose tools are exposed as callbacks.
    ///     volumes: Optional host volume mounts; these enable wrapper-visible
    ///         VFS storage when no caller storage is supplied.
    ///     on_stdout: Optional stdout streaming callback.
    ///     on_stderr: Optional stderr streaming callback.
    ///     result_variable: Optional variable whose value is returned by execute.
    ///
    /// Returns:
    ///     A new persistent `Session`.
    ///
    /// Raises:
    ///     InitializationError: If the session cannot be initialized.
    #[pyo3(signature = (*, vfs=None, vfs_mount_path=None, resource_limits=None, network=None, callbacks=None, mcp=None, volumes=None, on_stdout=None, on_stderr=None, result_variable=None))]
    #[allow(clippy::too_many_arguments)]
    fn create_session(
        &self,
        py: Python<'_>,
        vfs: Option<crate::vfs::VfsStorage>,
        vfs_mount_path: Option<String>,
        resource_limits: Option<ResourceLimits>,
        network: Option<NetConfig>,
        callbacks: Option<Bound<'_, PyAny>>,
        mcp: Option<PyRef<'_, crate::mcp::MCPManager>>,
        volumes: Option<Vec<crate::vfs::VolumeSpec>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
        result_variable: Option<String>,
    ) -> PyResult<Session> {
        // SAFETY: the bytes were produced by `PythonExecutor::precompile` or
        // loaded from a factory file created by this same API.
        let mut executor = unsafe { self.precompiled.to_executor() }.map_err(|e| {
            InitializationError::new_err(format!("failed to load factory runtime: {e}"))
        })?;
        executor = executor.with_python_stdlib(&self.stdlib_path);
        if let Some(path) = &self.site_packages_path {
            executor = executor.with_site_packages(path);
        }
        if let Some(name) = result_variable {
            executor = executor.with_result_variable(name);
        }
        let limits: eryx::ResourceLimits = resource_limits.unwrap_or_default().into();
        let callbacks = self.default_callbacks(py, callbacks);
        Session::from_executor(
            py,
            Arc::new(executor),
            Arc::clone(&self.runtime),
            vfs,
            vfs_mount_path,
            limits,
            network,
            callbacks,
            mcp,
            volumes,
            on_stdout,
            on_stderr,
            Some(Arc::clone(&self.extracted_packages)),
        )
    }

    /// Create a new sandbox from this factory.
    ///
    /// This is fast (~1ms) because the packages are already bundled
    /// into the factory's snapshot.
    ///
    /// Args:
    ///     site_packages: Optional path to additional site-packages.
    ///         If not provided, uses the site-packages from initialization.
    ///     resource_limits: Optional resource limits for the sandbox.
    ///     network: Optional network configuration. If provided, enables networking.
    ///     callbacks: Optional callbacks that sandboxed code can invoke.
    ///         Can be a CallbackRegistry or a list of callback dicts.
    ///     mcp: Optional MCPManager whose tools are exposed as callbacks.
    ///     secrets: Optional secrets, as for `Sandbox`.
    ///     scrub_stdout, scrub_stderr, scrub_files: Scrub secret placeholders
    ///         from that output. Default to True when secrets are provided.
    ///     scrub_result: Scrub secret placeholders from the structured result.
    ///         Defaults to False.
    ///     result_variable: Variable captured as `ExecuteResult.result`.
    ///         Defaults to `"result"`.
    ///
    /// Returns:
    ///     A new Sandbox ready to execute Python code.
    ///
    /// Raises:
    ///     InitializationError: If sandbox creation fails.
    ///
    /// Example:
    ///     sandbox = factory.create_sandbox()
    ///     result = sandbox.execute('print("Hello!")')
    ///
    ///     # With network access
    ///     net = NetConfig(allowed_hosts=["api.example.com"])
    ///     sandbox = factory.create_sandbox(network=net)
    ///
    ///     # With callbacks
    ///     def get_time():
    ///         import time
    ///         return {"timestamp": time.time()}
    ///
    ///     sandbox = factory.create_sandbox(callbacks=[
    ///         {"name": "get_time", "fn": get_time, "description": "Returns current time"}
    ///     ])
    ///
    ///     # With secrets
    ///     sandbox = factory.create_sandbox(
    ///         secrets={"API_KEY": {"value": "sk-xxx", "allowed_hosts": ["api.example.com"]}},
    ///         network=NetConfig(allowed_hosts=["api.example.com"]),
    ///     )
    #[pyo3(signature = (*, site_packages=None, resource_limits=None, network=None, callbacks=None, mcp=None, secrets=None, scrub_stdout=None, scrub_stderr=None, scrub_result=None, scrub_files=None, volumes=None, on_stdout=None, on_stderr=None, result_variable=None))]
    #[allow(clippy::too_many_arguments)]
    fn create_sandbox(
        &self,
        py: Python<'_>,
        site_packages: Option<PathBuf>,
        resource_limits: Option<ResourceLimits>,
        network: Option<NetConfig>,
        callbacks: Option<Bound<'_, PyAny>>,
        mcp: Option<PyRef<'_, crate::mcp::MCPManager>>,
        secrets: Option<Bound<'_, PyDict>>,
        scrub_stdout: Option<bool>,
        scrub_stderr: Option<bool>,
        scrub_result: Option<bool>,
        scrub_files: Option<bool>,
        volumes: Option<Vec<crate::vfs::VolumeSpec>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
        result_variable: Option<String>,
    ) -> PyResult<Sandbox> {
        // Use provided site_packages or fall back to the one from initialization
        let site_packages_path = site_packages.or_else(|| self.site_packages_path.clone());

        // Build sandbox from precompiled bytes. Trace collection (sys.settrace)
        // is always off for Python sandboxes; see `Sandbox::new`.
        // SAFETY: The precompiled bytes were created by PythonExecutor::precompile()
        // from a valid WASM component, so they are safe to deserialize.
        let mut builder = unsafe {
            eryx::Sandbox::builder()
                .with_precompiled_artifact(self.precompiled.clone())
                .with_python_stdlib(&self.stdlib_path)
                .with_trace_collection(false)
        };

        if let Some(path) = site_packages_path {
            builder = builder.with_site_packages(path);
        }

        if let Some(limits) = resource_limits {
            builder = builder.with_resource_limits(limits.into());
        }

        if let Some(name) = result_variable {
            builder = builder.with_result_variable(name);
        }

        if let Some(net) = network {
            builder = builder.with_network(net.into());
        }

        // Apply the caller's callbacks, or the factory's own (whose
        // declarations are baked into the snapshot) when none are given.
        if let Some(ref cbs) = self.default_callbacks(py, callbacks) {
            let python_callbacks = extract_callbacks(py, cbs)?;
            for callback in python_callbacks {
                builder = builder.with_callback(callback);
            }
        }

        if let Some(ref mcp_mgr) = mcp {
            for callback in mcp_mgr.as_callbacks() {
                builder = builder.with_callback(callback);
            }
        }

        // Apply secrets if provided
        let has_secrets = secrets.as_ref().is_some_and(|s| !s.is_empty());
        if let Some(ref secrets_dict) = secrets {
            builder = apply_secrets(builder, secrets_dict)?;
        }

        // Apply scrub policies (default to true when secrets are present)
        if scrub_stdout.unwrap_or(has_secrets) {
            builder = builder.scrub_stdout(true);
        }
        if scrub_stderr.unwrap_or(has_secrets) {
            builder = builder.scrub_stderr(true);
        }
        if scrub_files.unwrap_or(has_secrets) {
            builder = builder.scrub_files(true);
        }
        // Opt-in even with secrets, as for `Sandbox`.
        if scrub_result.unwrap_or(false) {
            builder = builder.scrub_result(true);
        }

        // Apply volume mounts if provided
        if let Some(vols) = volumes {
            builder = builder.with_volumes(vols.into_iter().map(eryx::VolumeMount::from));
        }

        // Apply output handler if stdout/stderr callbacks are provided
        if on_stdout.is_some() || on_stderr.is_some() {
            builder = builder.with_output_handler(PyOutputHandler {
                on_stdout,
                on_stderr,
            });
        }

        let inner = builder.build().map_err(eryx_error_to_py)?;

        Ok(Sandbox::from_inner(inner, Arc::clone(&self.runtime)))
    }

    /// Get the size of the pre-compiled runtime in bytes.
    #[getter]
    fn size_bytes(&self) -> usize {
        self.precompiled.len()
    }

    /// Get the pre-compiled runtime as bytes.
    ///
    /// This can be used for custom serialization or inspection.
    fn to_bytes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        PyBytes::new_with(py, self.precompiled.len(), |mut buf| {
            self.precompiled.write_to(&mut buf).map_err(|e| {
                InitializationError::new_err(format!("failed to read factory component: {e}"))
            })
        })
    }

    /// Create a sandbox pool from this factory.
    ///
    /// The pool reuses the factory's precompiled artifact, so when the idle
    /// queue is empty and a new sandbox must be created, it skips component
    /// deserialization (~1ms with caching enabled). Per-request state
    /// (callbacks, output handlers, resource limits) is set on each
    /// ``acquire()`` call and cleared automatically on release.
    ///
    /// Args:
    ///     max_size: Maximum concurrent sandboxes. Default: 10.
    ///     min_idle: Minimum warm sandboxes to keep ready. Default: 1.
    ///     acquire_timeout_ms: Max wait for a sandbox in ms. Default: 30000.
    ///     idle_timeout_ms: Idle sandbox eviction threshold in ms. Default: 300000.
    ///     network: Optional network configuration for every pooled sandbox.
    ///     secrets: Optional secrets for every pooled sandbox, as for `Sandbox`.
    ///     scrub_stdout, scrub_stderr, scrub_files: Scrub secret placeholders
    ///         from that output. Default to True when secrets are provided.
    ///
    /// Network and secrets are fixed for the pool's lifetime, unlike the
    /// per-request settings passed to ``acquire()``.
    ///
    /// Returns:
    ///     A ``SandboxPool`` ready to lease sandboxes.
    ///
    /// Raises:
    ///     InitializationError: If pool creation or pre-warming fails.
    ///
    /// Example:
    ///     factory = SandboxFactory(imports=["json"], cache=True)
    ///     pool = factory.create_pool(max_size=4, min_idle=1)
    ///     with pool.acquire() as sandbox:
    ///         result = sandbox.execute('import json; print(json.dumps([1]))')
    ///     pool.close()
    #[pyo3(signature = (*, max_size=10, min_idle=1, acquire_timeout_ms=30_000, idle_timeout_ms=300_000, network=None, secrets=None, scrub_stdout=None, scrub_stderr=None, scrub_files=None))]
    #[allow(clippy::too_many_arguments)]
    fn create_pool(
        &self,
        py: Python<'_>,
        max_size: usize,
        min_idle: usize,
        acquire_timeout_ms: u64,
        idle_timeout_ms: u64,
        network: Option<NetConfig>,
        secrets: Option<Bound<'_, PyDict>>,
        scrub_stdout: Option<bool>,
        scrub_stderr: Option<bool>,
        scrub_files: Option<bool>,
    ) -> PyResult<crate::pool::SandboxPool> {
        use std::time::Duration;

        let config = eryx::PoolConfig {
            max_size,
            min_idle,
            acquire_timeout: Duration::from_millis(acquire_timeout_ms),
            idle_timeout: Duration::from_millis(idle_timeout_ms),
        };

        let runtime = Arc::clone(&self.runtime);

        // Capture the factory's precompiled artifact and paths into a closure
        // that can recreate sandboxes. The PrecompiledArtifact's BLAKE3 cache
        // means deserialization is skipped on repeated calls.
        let precompiled = self.precompiled.clone();
        let stdlib_path = self.stdlib_path.clone();
        let site_packages_path = self.site_packages_path.clone();
        let network: Option<eryx::NetConfig> = network.map(Into::into);
        let secrets = secrets.as_ref().map(extract_secrets).transpose()?;
        let has_secrets = secrets.as_ref().is_some_and(|s| !s.is_empty());
        let scrub_stdout = scrub_stdout.unwrap_or(has_secrets);
        let scrub_stderr = scrub_stderr.unwrap_or(has_secrets);
        let scrub_files = scrub_files.unwrap_or(has_secrets);

        let builder_fn = move || {
            // SAFETY: the bytes were produced by `PythonExecutor::precompile`
            // or loaded from a factory file created by the same API.
            let mut builder = unsafe {
                eryx::Sandbox::builder()
                    .with_precompiled_artifact(precompiled.clone())
                    .with_python_stdlib(&stdlib_path)
                    .with_trace_collection(false)
            };
            if let Some(ref path) = site_packages_path {
                builder = builder.with_site_packages(path);
            }
            if let Some(ref net) = network {
                builder = builder.with_network(net.clone());
            }
            for (name, value, allowed_hosts) in secrets.iter().flatten() {
                builder = builder.with_secret(name, value, allowed_hosts.clone());
            }
            builder
                .scrub_stdout(scrub_stdout)
                .scrub_stderr(scrub_stderr)
                .scrub_files(scrub_files)
                .build()
        };

        let rt = runtime.clone();
        let pool = py.detach(|| {
            rt.block_on(eryx::SandboxPool::with_builder(builder_fn, config))
                .map_err(crate::error::pool_error_to_py)
        })?;

        Ok(crate::pool::SandboxPool::from_inner(
            pool,
            runtime,
            self.callbacks.as_ref().map(|cbs| cbs.clone_ref(py)),
            Arc::clone(&self.extracted_packages),
            self.data_files_dir.clone(),
        ))
    }

    fn __repr__(&self) -> String {
        format!(
            "SandboxFactory(size_bytes={}, site_packages={:?})",
            self.precompiled.len(),
            self.site_packages_path,
        )
    }
}

impl SandboxFactory {
    /// Write this factory to `path` in the current format.
    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let data_files = self
            .site_packages_path
            .as_ref()
            .map(|p| collect_data_files(p))
            .transpose()?
            .unwrap_or_default();
        let build_inputs = self.build_inputs.encode()?;
        write_factory_file(path, |w| {
            write_factory(&self.precompiled, &data_files, &build_inputs, w)
        })
    }

    /// Rebuild a factory that another pyeryx version saved, from the build
    /// inputs its file stores, reusing a copy cached by an earlier rebuild if
    /// there is one.
    fn rebuild(
        py: Python<'_>,
        request: RebuildRequest<'_>,
        callbacks: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let RebuildRequest {
            path,
            error,
            data_files_dir,
            data_files_path,
            build_inputs,
            site_packages,
            cache,
        } = request;
        let BuildInputs::InFile(stored) = &build_inputs else {
            return Err(incompatible_error(path, &error, &build_inputs));
        };
        let read_err = |e: std::io::Error| {
            InitializationError::new_err(format!(
                "failed to read the build inputs of {}: {e}",
                path.display()
            ))
        };
        let inputs = stored.read(&stored.build_inputs).map_err(read_err)?;
        let data_files = stored.read(&stored.data_files).map_err(read_err)?;
        let callbacks = callbacks.map(Bound::unbind);
        let runtime = crate::error::make_runtime()?;
        let embedded = eryx::embedded::EmbeddedResources::get().map_err(eryx_error_to_py)?;
        let stdlib_path = embedded.stdlib().to_path_buf();

        let cached_path = rebuilt_factory_path(&data_files, &inputs);
        if let Ok(cached_path) = &cached_path
            && cached_path.exists()
            && let Ok(cached) = load_factory_file(cached_path, cache)
            && let Ok(precompiled) = cached
                .precompiled
                .and_then(|p| check_compatible(&p).map(|()| p))
        {
            return Ok(Self {
                precompiled,
                stdlib_path,
                site_packages_path: site_packages.or(cached.data_files_path),
                extracted_packages: Arc::new(Vec::new()),
                callbacks,
                data_files_dir: cached.data_files_dir.map(Arc::new),
                runtime,
                build_inputs: cached.build_inputs,
            });
        }

        // The data files are the site-packages the factory was created with.
        let recipe = decode_build_inputs(&inputs).map_err(read_err)?;
        let (_, extensions, _) = process_packages(data_files_path.as_ref(), None)?;
        let precompiled = build_component(
            py,
            &runtime,
            &stdlib_path,
            data_files_path.as_deref(),
            extensions,
            &recipe,
            cache,
        )?;
        let mut factory = Self {
            precompiled,
            stdlib_path,
            site_packages_path: data_files_path,
            extracted_packages: Arc::new(Vec::new()),
            callbacks,
            data_files_dir: data_files_dir.map(Arc::new),
            runtime,
            build_inputs: BuildInputs::Bytes(inputs),
        };

        let saved = cached_path
            .map_err(|e| e.to_string())
            .and_then(|cached_path| {
                let saved = cached_path
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| factory.save_to(&cached_path));
                saved.map_err(|e| format!("{}: {e}", cached_path.display()))
            });
        if let Err(e) = saved {
            warn(
                py,
                format!(
                    "rebuilt the factory from {}, but couldn't cache the rebuilt \
                     copy ({e}), so the next load will rebuild it again",
                    path.display()
                ),
            )?;
        }

        // Only now apply an override, so the cached copy holds the factory's
        // own site-packages.
        if site_packages.is_some() {
            factory.site_packages_path = site_packages;
        }
        Ok(factory)
    }

    /// The callbacks a sandbox or session gets when the caller passes none:
    /// the factory's own.
    fn default_callbacks<'py>(
        &self,
        py: Python<'py>,
        callbacks: Option<Bound<'py, PyAny>>,
    ) -> Option<Bound<'py, PyAny>> {
        callbacks.or_else(|| self.callbacks.as_ref().map(|cbs| cbs.bind(py).clone()))
    }
}

impl std::fmt::Debug for SandboxFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxFactory")
            .field("size_bytes", &self.precompiled.len())
            .field("stdlib_path", &self.stdlib_path)
            .field("site_packages_path", &self.site_packages_path)
            .finish_non_exhaustive()
    }
}

// =============================================================================
// Factory file format (v4)
// =============================================================================
//
// [component_len bytes: Wasmtime precompiled component, starting at offset 0]
// [data_files_len bytes: zstd-compressed tar of data files]
// [build_inputs_len bytes: JSON build inputs, see `encode_build_inputs`]
// [8 bytes: component_len (little-endian)]
// [8 bytes: data_files_len (little-endian)]
// [8 bytes: build_inputs_len (little-endian)]
// [32 bytes: component content hash, for the instance-pre cache key]
// [4 bytes: version (4, little-endian)]
// [8 bytes: magic "ERYXFCT4"]
//
// The component comes first because Wasmtime can only memory-map a component
// that starts at offset 0 of a file; it ignores the bytes after it. `load()`
// reads just the footer and data files, and maps the component. The stored
// hash spares `load(cache=True)` from reading the whole component to compute
// its cache key; it is trusted like the component itself.
//
// The data files are the factory's site-packages, minus `.pyc` files. The
// build inputs are the rest of what it was created from: its imports, setup
// code and callback declarations. A component only loads in the eryx version
// that built it, so the two together are what lets another version rebuild
// the factory. An empty build inputs section means it can't be rebuilt.
//
// Version 3 files also leave out `.py` files, so after loading one, a module
// that wasn't imported while it was created can't be imported.
//
// Version 3: as version 4 without the build inputs, with footer
// [component_len u64][data_files_len u64][hash 32][version 3 u32]
// [magic "ERYXFCT3"].
//
// Version 2 (legacy): [magic "ERYX"][version 2 u32][component_len u64]
// [component][data_files_len u64][data files]. Read into memory; re-save to
// upgrade.
//
// Version 1 (legacy): raw precompiled bytes with no envelope. Detected by
// neither footer nor the v2 magic matching. Mapped like v4.

const FACTORY_FOOTER_MAGIC: &[u8; 8] = b"ERYXFCT4";
const FACTORY_FOOTER_LEN: u64 = 8 + 8 + 8 + 32 + 4 + 8;
const FACTORY_V3_FOOTER_MAGIC: &[u8; 8] = b"ERYXFCT3";
const FACTORY_V3_FOOTER_LEN: u64 = 8 + 8 + 32 + 4 + 8;

/// What a factory was created from, apart from its site-packages files.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FactoryRecipe {
    imports: Vec<String>,
    setup_code: Option<String>,
    callbacks: Vec<eryx::preinit::CallbackDeclaration>,
}

/// A factory's build inputs, in whichever form it has them.
#[derive(Debug)]
enum BuildInputs {
    /// Created with `SandboxFactory(...)`.
    Recipe(FactoryRecipe),
    /// Stored in the file the factory was loaded from.
    InFile(StoredSections),
    /// Already encoded, for a rebuilt factory.
    Bytes(Vec<u8>),
    /// Loaded from a file saved before factories stored their build inputs.
    Missing,
}

impl BuildInputs {
    /// The encoded section to save, or an empty one if there's none.
    fn encode(&self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Recipe(recipe) => encode_build_inputs(recipe),
            Self::InFile(stored) => stored.read(&stored.build_inputs),
            Self::Bytes(bytes) => Ok(bytes.clone()),
            Self::Missing => Ok(Vec::new()),
        }
    }
}

/// The data files and build inputs sections of a loaded factory file, read
/// from the same trusted handle that `load()` checked.
#[derive(Debug)]
struct StoredSections {
    file: Mutex<std::fs::File>,
    data_files: Range<u64>,
    build_inputs: Range<u64>,
}

impl StoredSections {
    fn read(&self, range: &Range<u64>) -> std::io::Result<Vec<u8>> {
        use std::io::{Read as _, Seek as _};

        let mut file = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("factory file lock poisoned"))?;
        file.seek(std::io::SeekFrom::Start(range.start))?;
        let mut bytes = Vec::new();
        (&mut *file)
            .take(range.end - range.start)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// Encode a factory's build inputs as JSON.
fn encode_build_inputs(recipe: &FactoryRecipe) -> std::io::Result<Vec<u8>> {
    let callbacks: Vec<_> = recipe
        .callbacks
        .iter()
        .map(|cb| {
            serde_json::json!({
                "name": cb.name,
                "description": cb.description,
                "parameters_schema_json": cb.parameters_schema_json,
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "imports": recipe.imports,
        "setup_code": recipe.setup_code,
        "callbacks": callbacks,
    }))
    .map_err(std::io::Error::other)
}

/// Decode [`encode_build_inputs`]' output.
fn decode_build_inputs(encoded: &[u8]) -> std::io::Result<FactoryRecipe> {
    let invalid = |msg: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid build inputs: {msg}"),
        )
    };
    let manifest: serde_json::Value =
        serde_json::from_slice(encoded).map_err(|e| invalid(&e.to_string()))?;

    let string = |value: &serde_json::Value, name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid(&format!("`{name}` is not a string")))
    };
    let array = |name: &str| {
        manifest
            .get(name)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| invalid(&format!("`{name}` is not an array")))
    };
    let imports = array("imports")?
        .iter()
        .map(|import| {
            import
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("an import is not a string"))
        })
        .collect::<Result<_, _>>()?;
    let setup_code = match manifest.get("setup_code") {
        None | Some(serde_json::Value::Null) => None,
        Some(_) => Some(string(&manifest, "setup_code")?),
    };
    let callbacks = array("callbacks")?
        .iter()
        .map(|cb| {
            Ok(eryx::preinit::CallbackDeclaration {
                name: string(cb, "name")?,
                description: string(cb, "description")?,
                parameters_schema_json: string(cb, "parameters_schema_json")?,
            })
        })
        .collect::<std::io::Result<_>>()?;

    Ok(FactoryRecipe {
        imports,
        setup_code,
        callbacks,
    })
}

/// Collect the files of a site-packages directory to save with a factory.
///
/// Returns a list of (relative_path, file_contents) pairs. `.py` files are
/// kept: the snapshot only holds the modules imported while the factory was
/// created, so the others are imported from these files. `.pyc` files are
/// skipped, since the interpreter recompiles them.
fn collect_data_files(site_packages: &Path) -> Result<Vec<(String, Vec<u8>)>, std::io::Error> {
    let mut files = Vec::new();
    if !site_packages.exists() {
        return Ok(files);
    }
    for entry in walkdir::WalkDir::new(site_packages) {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || path.extension().is_some_and(|ext| ext == "pyc") {
            continue;
        }
        let relative = path
            .strip_prefix(site_packages)
            .map_err(std::io::Error::other)?;
        let relative_str = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let contents = std::fs::read(path)?;
        files.push((relative_str, contents));
    }
    Ok(files)
}

/// A zstd-compressed tar of `files`, or nothing if there are none.
fn tar_zst(files: &[(String, Vec<u8>)]) -> std::io::Result<Vec<u8>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let mut tar_builder = tar::Builder::new(Vec::new());
    for (rel_path, contents) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_builder.append_data(&mut header, rel_path, contents.as_slice())?;
    }
    let tar_bytes = tar_builder.into_inner()?;
    zstd::encode_all(tar_bytes.as_slice(), 3)
}

/// Write a v4 factory file to `writer`.
fn write_factory(
    precompiled: &eryx::PrecompiledArtifact,
    data_files: &[(String, Vec<u8>)],
    build_inputs: &[u8],
    writer: &mut impl std::io::Write,
) -> Result<(), std::io::Error> {
    let data_files_tar_zst = tar_zst(data_files)?;
    let component_hash = precompiled.content_hash()?;
    precompiled.write_to(writer)?;
    writer.write_all(&data_files_tar_zst)?;
    writer.write_all(build_inputs)?;
    writer.write_all(&(precompiled.len() as u64).to_le_bytes())?;
    writer.write_all(&(data_files_tar_zst.len() as u64).to_le_bytes())?;
    writer.write_all(&(build_inputs.len() as u64).to_le_bytes())?;
    writer.write_all(&component_hash)?;
    writer.write_all(&FACTORY_VERSION.to_le_bytes())?;
    writer.write_all(FACTORY_FOOTER_MAGIC)?;
    Ok(())
}

/// Write a factory file that [`open_trusted_factory_file`] accepts whatever the
/// umask, replacing any existing file atomically.
///
/// The rename also keeps processes that have the old file mapped safe: they
/// keep the old inode. On Unix the file is mode `0644`: not `0600`, because
/// images bake in a root-owned factory that a non-root user loads.
fn write_factory_file(
    path: &std::path::Path,
    write: impl FnOnce(&mut std::io::BufWriter<&std::fs::File>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => std::path::Path::new("."),
    };
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    let mut writer = std::io::BufWriter::new(tmp.as_file());
    write(&mut writer)?;
    writer.flush()?;
    drop(writer);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Open a factory file, refusing one that another user could have written.
///
/// The file holds native code, so on Unix it must be owned by the current
/// user or root and not be group- or world-writable. The returned handle is
/// what gets mapped, so a path swapped after this check is never loaded.
fn open_trusted_factory_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::File::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let meta = file.metadata()?;
        let root_owned = meta.uid() == 0 && meta.mode() & 0o022 == 0;
        if !(eryx::cache::is_private(&meta) || root_owned) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file is owned by another user or writable by other users",
            ));
        }
    }
    Ok(file)
}

/// How a factory file is laid out, from [`read_factory_layout`].
#[derive(Debug, PartialEq, Eq)]
enum FactoryLayout {
    /// The component is the first `component_len` bytes; the data files
    /// archive and the build inputs follow it, each absent if its length is
    /// zero. v3 and v4 files carry the component's content hash; v1 files
    /// don't.
    Mapped {
        component_len: u64,
        data_files_len: u64,
        build_inputs_len: u64,
        component_hash: Option<[u8; 32]>,
    },
    /// A legacy v2 envelope, which has to be read into memory.
    V2,
}

/// Work out a factory file's layout from its footer and header.
fn read_factory_layout(
    file: &mut (impl std::io::Read + std::io::Seek),
) -> Result<FactoryLayout, std::io::Error> {
    use std::io::SeekFrom;

    let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);
    let file_len = file.seek(SeekFrom::End(0))?;

    let mut magic = [0u8; 8];
    if file_len >= 8 {
        file.seek(SeekFrom::End(-8))?;
        file.read_exact(&mut magic)?;
    }
    // (footer length, expected version, whether it records build inputs)
    let footer = match &magic {
        m if m == FACTORY_FOOTER_MAGIC => Some((FACTORY_FOOTER_LEN, FACTORY_VERSION, true)),
        m if m == FACTORY_V3_FOOTER_MAGIC => Some((FACTORY_V3_FOOTER_LEN, 3, false)),
        _ => None,
    };
    if let Some((footer_len, expected_version, has_build_inputs)) = footer
        && file_len >= footer_len
    {
        let mut footer = vec![0u8; footer_len as usize];
        file.seek(SeekFrom::End(-(footer_len as i64)))?;
        file.read_exact(&mut footer)?;
        let version_at = footer.len() - 12;
        let version = read_le_u32(&footer[version_at..version_at + 4])?;
        if version != expected_version {
            return Err(invalid(format!(
                "unsupported factory file version: {version}"
            )));
        }
        let component_len = read_le_u64(&footer[..8])?;
        let data_files_len = read_le_u64(&footer[8..16])?;
        let (build_inputs_len, hash_at) = if has_build_inputs {
            (read_le_u64(&footer[16..24])?, 24)
        } else {
            (0, 16)
        };
        let expected = component_len
            .checked_add(data_files_len)
            .and_then(|len| len.checked_add(build_inputs_len))
            .and_then(|len| len.checked_add(footer_len));
        if expected != Some(file_len) {
            return Err(invalid(format!(
                "factory file is {file_len} bytes but its footer describes \
                 {component_len} + {data_files_len} + {build_inputs_len} + {footer_len}"
            )));
        }
        let mut component_hash = [0u8; 32];
        component_hash.copy_from_slice(&footer[hash_at..hash_at + 32]);
        return Ok(FactoryLayout::Mapped {
            component_len,
            data_files_len,
            build_inputs_len,
            component_hash: Some(component_hash),
        });
    }

    let mut magic = [0u8; 4];
    file.seek(SeekFrom::Start(0))?;
    if file_len >= 4 {
        file.read_exact(&mut magic)?;
    }
    if &magic == FACTORY_MAGIC {
        Ok(FactoryLayout::V2)
    } else {
        // v1: the whole file is the component.
        Ok(FactoryLayout::Mapped {
            component_len: file_len,
            data_files_len: 0,
            build_inputs_len: 0,
            component_hash: None,
        })
    }
}

type ParsedFactory = (Vec<u8>, Option<tempfile::TempDir>, Option<PathBuf>);

fn read_le_u32(buf: &[u8]) -> Result<u32, std::io::Error> {
    let arr: [u8; 4] = buf.try_into().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "not enough bytes for u32")
    })?;
    Ok(u32::from_le_bytes(arr))
}

fn read_le_u64(buf: &[u8]) -> Result<u64, std::io::Error> {
    let arr: [u8; 8] = buf.try_into().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "not enough bytes for u64")
    })?;
    Ok(u64::from_le_bytes(arr))
}

/// Parse a legacy v2 factory file.
///
/// Returns (precompiled_bytes, optional_temp_dir, optional_site_packages_path).
fn parse_factory_v2(bytes: &[u8]) -> Result<ParsedFactory, std::io::Error> {
    let header_size = 4 + 4 + 8; // magic + version + precompiled_len
    if bytes.len() < header_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "factory file too short for v2 header",
        ));
    }

    let version = read_le_u32(&bytes[4..8])?;
    if version != 2 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported factory file version: {version}"),
        ));
    }

    let precompiled_len = read_le_u64(&bytes[8..16])? as usize;

    let precompiled_end = 16 + precompiled_len;
    if bytes.len() < precompiled_end + 8 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "factory file truncated (missing data files length)",
        ));
    }

    let precompiled_bytes = bytes[16..precompiled_end].to_vec();

    let data_files_len = read_le_u64(&bytes[precompiled_end..precompiled_end + 8])? as usize;

    if data_files_len == 0 {
        return Ok((precompiled_bytes, None, None));
    }

    let data_start = precompiled_end + 8;
    let data_end = data_start + data_files_len;
    if bytes.len() < data_end {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "factory file truncated (data files section)",
        ));
    }

    let (temp_dir, site_packages_path) = extract_data_files(&bytes[data_start..data_end])?;
    Ok((precompiled_bytes, Some(temp_dir), Some(site_packages_path)))
}

/// Extract a zstd-compressed tar of data files into a new temp directory.
///
/// Returns the temp directory (which must be kept alive) and its path.
fn extract_data_files(compressed: &[u8]) -> Result<(tempfile::TempDir, PathBuf), std::io::Error> {
    let tar_bytes = zstd::decode_all(compressed)?;
    let mut archive = tar::Archive::new(tar_bytes.as_slice());

    let temp_dir = tempfile::TempDir::new()?;
    let extract_path = temp_dir.path();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        // Reject absolute or parent-traversal paths
        if path.is_absolute()
            || path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsafe path in data files archive: {}", path.display()),
            ));
        }

        let out_path = extract_path.join(&path);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut outfile = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut outfile)?;
    }

    let site_packages_path = extract_path.to_path_buf();
    Ok((temp_dir, site_packages_path))
}

/// Process packages to extract site-packages path and native extensions.
///
/// Returns (site_packages_path, native_extensions, extracted_packages).
/// The extracted_packages must be kept alive to prevent temp directory cleanup.
fn process_packages(
    site_packages: Option<&PathBuf>,
    packages: Option<&Vec<PathBuf>>,
) -> PyResult<(
    Option<PathBuf>,
    Vec<eryx::preinit::NativeExtension>,
    Vec<eryx::ExtractedPackage>,
)> {
    let mut extensions = Vec::new();
    let mut extracted_packages = Vec::new();
    let mut final_site_packages = site_packages.cloned();

    // If packages are provided, extract them and collect native extensions
    if let Some(package_paths) = packages {
        // If we have multiple packages, we need a consolidated site-packages directory
        // For now, use the first package's directory and copy others into it
        // A better approach would be to extract all to a shared temp directory

        for path in package_paths {
            let package = eryx::ExtractedPackage::from_path(path).map_err(eryx_error_to_py)?;

            // Use the first package's python_path as site_packages if not already set
            if final_site_packages.is_none() {
                final_site_packages = Some(package.python_path.clone());
            } else if let Some(ref target_dir) = final_site_packages {
                // Copy this package's contents to the main site-packages directory
                copy_directory_contents(&package.python_path, target_dir)?;
            }

            // Collect native extensions with proper dlopen paths
            for ext in &package.native_extensions {
                // The dlopen path needs to be relative to /site-packages
                let dlopen_path = format!("/site-packages/{}", ext.relative_path);
                extensions.push(eryx::preinit::NativeExtension::new(
                    dlopen_path,
                    ext.bytes.clone(),
                ));
            }

            // Keep the extracted package alive
            extracted_packages.push(package);
        }
    }

    // If site_packages is provided, scan for additional native extensions
    if let Some(ref site_pkg_path) = final_site_packages
        && site_pkg_path.exists()
    {
        for entry in walkdir::WalkDir::new(site_pkg_path) {
            let entry = entry.map_err(|e| {
                InitializationError::new_err(format!("failed to walk site-packages: {e}"))
            })?;
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "so") {
                let relative = path.strip_prefix(site_pkg_path).map_err(|e| {
                    InitializationError::new_err(format!("failed to get relative path: {e}"))
                })?;
                let relative_str = relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                let dlopen_path = format!("/site-packages/{relative_str}");

                // Skip if we already have this extension from packages
                if extensions.iter().any(|e| e.name == dlopen_path) {
                    continue;
                }

                let bytes = std::fs::read(path).map_err(|e| {
                    InitializationError::new_err(format!("failed to read extension: {e}"))
                })?;
                extensions.push(eryx::preinit::NativeExtension::new(dlopen_path, bytes));
            }
        }
    }

    Ok((final_site_packages, extensions, extracted_packages))
}

/// Copy contents of one directory into another.
fn copy_directory_contents(src: &Path, dst: &Path) -> PyResult<()> {
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry
            .map_err(|e| InitializationError::new_err(format!("failed to walk directory: {e}")))?;
        let src_path = entry.path();
        let relative = src_path.strip_prefix(src).map_err(|e| {
            InitializationError::new_err(format!("failed to get relative path: {e}"))
        })?;
        let dst_path = dst.join(relative);

        if src_path.is_dir() {
            std::fs::create_dir_all(&dst_path).map_err(|e| {
                InitializationError::new_err(format!("failed to create directory: {e}"))
            })?;
        } else if src_path.is_file() {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    InitializationError::new_err(format!("failed to create parent directory: {e}"))
                })?;
            }
            std::fs::copy(src_path, &dst_path)
                .map_err(|e| InitializationError::new_err(format!("failed to copy file: {e}")))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tz_data_files() -> Vec<(String, Vec<u8>)> {
        vec![
            ("tzdata/zoneinfo/UTC".to_string(), b"TZif data".to_vec()),
            (
                "tzdata/zoneinfo/Asia/Tokyo".to_string(),
                b"TZif tokyo data".to_vec(),
            ),
        ]
    }

    fn build_v4(
        precompiled: &[u8],
        data_files: &[(String, Vec<u8>)],
        build_inputs: &[u8],
    ) -> Vec<u8> {
        let artifact = eryx::PrecompiledArtifact::new(precompiled.to_vec());
        let mut bytes = Vec::new();
        write_factory(&artifact, data_files, build_inputs, &mut bytes).unwrap();
        bytes
    }

    /// Build a v3 file, as `save()` wrote before build inputs were stored.
    fn build_v3(precompiled: &[u8], data_files: &[(String, Vec<u8>)]) -> Vec<u8> {
        let data_files = tar_zst(data_files).unwrap();
        let mut bytes = precompiled.to_vec();
        bytes.extend_from_slice(&data_files);
        bytes.extend_from_slice(&(precompiled.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(data_files.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&[7u8; 32]);
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(FACTORY_V3_FOOTER_MAGIC);
        bytes
    }

    fn recipe() -> FactoryRecipe {
        FactoryRecipe {
            imports: vec!["mypkg".to_string(), "json".to_string()],
            setup_code: Some("ready = True\n".to_string()),
            callbacks: vec![eryx::preinit::CallbackDeclaration {
                name: "get_time".to_string(),
                description: "Returns the time".to_string(),
                parameters_schema_json: r#"{"type":"object"}"#.to_string(),
            }],
        }
    }

    /// Build a legacy v2 envelope, as `save()` wrote before v3.
    fn build_v2(precompiled: &[u8], data_files_tar_zst: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FACTORY_MAGIC);
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&(precompiled.len() as u64).to_le_bytes());
        bytes.extend_from_slice(precompiled);
        bytes.extend_from_slice(&(data_files_tar_zst.len() as u64).to_le_bytes());
        bytes.extend_from_slice(data_files_tar_zst);
        bytes
    }

    #[test]
    fn v1_is_mapped_whole() {
        let raw = b"\x7fELF some precompiled bytes";
        let layout = read_factory_layout(&mut Cursor::new(raw)).unwrap();
        assert_eq!(
            layout,
            FactoryLayout::Mapped {
                component_len: raw.len() as u64,
                data_files_len: 0,
                build_inputs_len: 0,
                component_hash: None,
            }
        );
    }

    #[test]
    fn v4_component_starts_at_offset_zero() {
        let precompiled = b"precompiled wasm component";
        let bytes = build_v4(precompiled, &[], &[]);

        assert_eq!(&bytes[..precompiled.len()], precompiled);
        assert_eq!(&bytes[bytes.len() - 8..], FACTORY_FOOTER_MAGIC);
        let hash = eryx::PrecompiledArtifact::new(precompiled.to_vec())
            .content_hash()
            .unwrap();
        assert_eq!(
            read_factory_layout(&mut Cursor::new(&bytes)).unwrap(),
            FactoryLayout::Mapped {
                component_len: precompiled.len() as u64,
                data_files_len: 0,
                build_inputs_len: 0,
                component_hash: Some(hash),
            }
        );
    }

    #[test]
    fn v4_with_data_files() {
        let precompiled = b"precompiled wasm component";
        let bytes = build_v4(precompiled, &tz_data_files(), &[]);

        let FactoryLayout::Mapped {
            component_len,
            data_files_len,
            ..
        } = read_factory_layout(&mut Cursor::new(&bytes)).unwrap()
        else {
            panic!("expected a mapped layout");
        };
        assert_eq!(component_len, precompiled.len() as u64);

        let start = usize::try_from(component_len).unwrap();
        let end = start + usize::try_from(data_files_len).unwrap();
        let (_temp_dir, path) = extract_data_files(&bytes[start..end]).unwrap();

        let utc = std::fs::read(path.join("tzdata/zoneinfo/UTC")).unwrap();
        assert_eq!(utc, b"TZif data");
        let tokyo = std::fs::read(path.join("tzdata/zoneinfo/Asia/Tokyo")).unwrap();
        assert_eq!(tokyo, b"TZif tokyo data");
    }

    #[test]
    fn v4_rejects_footer_lengths_that_do_not_match() {
        let mut bytes = build_v4(b"precompiled", &tz_data_files(), b"inputs");
        // Dropping a leading byte keeps the footer but breaks its lengths.
        bytes.remove(0);
        assert!(read_factory_layout(&mut Cursor::new(&bytes)).is_err());
    }

    #[test]
    fn v4_rejects_overflowing_footer_lengths() {
        let mut bytes = build_v4(b"precompiled", &[], &[]);
        let footer = bytes.len() - FACTORY_FOOTER_LEN as usize;
        bytes[footer..footer + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(read_factory_layout(&mut Cursor::new(&bytes)).is_err());
    }

    #[test]
    fn v4_rejects_bad_version() {
        let mut bytes = build_v4(b"precompiled", &[], &[]);
        let version = bytes.len() - 12;
        bytes[version..version + 4].copy_from_slice(&99u32.to_le_bytes());
        assert!(read_factory_layout(&mut Cursor::new(&bytes)).is_err());
    }

    #[test]
    fn v4_records_the_build_inputs_after_the_data_files() {
        let precompiled = b"precompiled wasm component";
        let bytes = build_v4(precompiled, &tz_data_files(), b"build inputs");

        let FactoryLayout::Mapped {
            component_len,
            data_files_len,
            build_inputs_len,
            ..
        } = read_factory_layout(&mut Cursor::new(&bytes)).unwrap()
        else {
            panic!("expected a mapped layout");
        };
        let start = usize::try_from(component_len + data_files_len).unwrap();
        let end = start + usize::try_from(build_inputs_len).unwrap();
        assert_eq!(&bytes[start..end], b"build inputs");
    }

    #[test]
    fn v3_is_still_read() {
        let precompiled = b"precompiled wasm component";
        let bytes = build_v3(precompiled, &tz_data_files());

        let FactoryLayout::Mapped {
            component_len,
            data_files_len,
            build_inputs_len,
            component_hash,
        } = read_factory_layout(&mut Cursor::new(&bytes)).unwrap()
        else {
            panic!("expected a mapped layout");
        };
        assert_eq!(component_len, precompiled.len() as u64);
        assert_eq!(build_inputs_len, 0);
        assert_eq!(component_hash, Some([7u8; 32]));

        let start = usize::try_from(component_len).unwrap();
        let end = start + usize::try_from(data_files_len).unwrap();
        let (_temp_dir, path) = extract_data_files(&bytes[start..end]).unwrap();
        assert_eq!(
            std::fs::read(path.join("tzdata/zoneinfo/UTC")).unwrap(),
            b"TZif data"
        );
    }

    #[test]
    fn v3_rejects_bad_version() {
        let mut bytes = build_v3(b"precompiled", &[]);
        let version = bytes.len() - 12;
        bytes[version..version + 4].copy_from_slice(&4u32.to_le_bytes());
        assert!(read_factory_layout(&mut Cursor::new(&bytes)).is_err());
    }

    #[test]
    fn build_inputs_round_trip() {
        let encoded = encode_build_inputs(&recipe()).unwrap();
        assert_eq!(decode_build_inputs(&encoded).unwrap(), recipe());

        let empty = FactoryRecipe {
            imports: Vec::new(),
            setup_code: None,
            callbacks: Vec::new(),
        };
        let encoded = encode_build_inputs(&empty).unwrap();
        assert_eq!(decode_build_inputs(&encoded).unwrap(), empty);
    }

    #[test]
    fn decode_build_inputs_rejects_malformed_sections() {
        assert!(decode_build_inputs(b"not json").is_err());
        assert!(decode_build_inputs(br#"{"imports": [1], "callbacks": []}"#).is_err());
        assert!(decode_build_inputs(br#"{"imports": [], "callbacks": [{}]}"#).is_err());
        assert!(decode_build_inputs(br#"{"callbacks": []}"#).is_err());
    }

    #[test]
    fn v2_is_still_read() {
        let precompiled = b"precompiled wasm module";
        let bytes = build_v2(precompiled, &[]);
        assert_eq!(
            read_factory_layout(&mut Cursor::new(&bytes)).unwrap(),
            FactoryLayout::V2
        );

        let (parsed, temp_dir, path) = parse_factory_v2(&bytes).unwrap();
        assert_eq!(parsed, precompiled);
        assert!(temp_dir.is_none());
        assert!(path.is_none());
    }

    #[test]
    fn v2_with_data_files() {
        let precompiled = b"precompiled wasm module";
        let tar_zst = &tar_zst(&tz_data_files()).unwrap();
        let bytes = build_v2(precompiled, tar_zst);

        let (parsed, temp_dir, path) = parse_factory_v2(&bytes).unwrap();
        assert_eq!(parsed, precompiled);
        assert!(temp_dir.is_some());
        let utc = std::fs::read(path.unwrap().join("tzdata/zoneinfo/UTC")).unwrap();
        assert_eq!(utc, b"TZif data");
    }

    #[test]
    fn v2_rejects_truncated_header() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FACTORY_MAGIC);
        // Too short — missing version and lengths
        assert!(parse_factory_v2(&bytes).is_err());
    }

    #[test]
    fn v2_rejects_bad_version() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FACTORY_MAGIC);
        bytes.extend_from_slice(&99u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        assert!(parse_factory_v2(&bytes).is_err());
    }

    #[test]
    fn collect_data_files_keeps_py_and_skips_pyc() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();

        std::fs::create_dir_all(base.join("pkg")).unwrap();
        std::fs::write(base.join("pkg/__init__.py"), "# python").unwrap();
        std::fs::write(base.join("pkg/module.pyc"), "bytecode").unwrap();
        std::fs::write(base.join("pkg/data.bin"), "binary data").unwrap();
        std::fs::write(base.join("pkg/config.json"), "{}").unwrap();

        let files = collect_data_files(base).unwrap();
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"pkg/__init__.py"));
        assert!(!names.contains(&"pkg/module.pyc"));
        assert!(names.contains(&"pkg/data.bin"));
        assert!(names.contains(&"pkg/config.json"));
    }

    #[test]
    fn collect_data_files_nonexistent_dir() {
        let files = collect_data_files(Path::new("/nonexistent/path")).unwrap();
        assert!(files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn write_factory_file_replaces_group_writable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("factory.bin");
        std::fs::write(&path, b"stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        write_factory_file(&path, |w| w.write_all(b"factory")).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        let file = open_trusted_factory_file(&path).unwrap();
        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut &file, &mut contents).unwrap();
        assert_eq!(contents, b"factory");
    }

    #[cfg(unix)]
    #[test]
    fn open_trusted_factory_file_refuses_group_writable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("factory.bin");
        std::fs::write(&path, b"factory").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();

        let err = open_trusted_factory_file(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
