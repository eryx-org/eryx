//! Sandbox factory for Python.
//!
//! Provides the `SandboxFactory` class for creating sandboxes with custom packages.
//! The factory bundles packages and pre-imports into a reusable snapshot.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use crate::callback::extract_callbacks;
use crate::error::{InitializationError, eryx_error_to_py};
use crate::net_config::NetConfig;
use crate::resource_limits::ResourceLimits;
use crate::sandbox::{PyOutputHandler, Sandbox, apply_secrets};
use crate::session::Session;

const FACTORY_MAGIC: &[u8; 4] = b"ERYX";
const FACTORY_VERSION: u32 = 2;

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
    #[allow(dead_code)]
    extracted_packages: Arc<Vec<eryx::ExtractedPackage>>,
    /// Callbacks whose declarations were baked into the snapshot (or, for a
    /// loaded factory, the ones the caller says were). `create_sandbox()` and
    /// `create_session()` register them when given no callbacks of their own.
    callbacks: Option<Py<PyAny>>,
    /// Holds the temp directory for data files extracted during `load()`.
    #[allow(dead_code)]
    data_files_dir: Option<tempfile::TempDir>,
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
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    InitializationError::new_err(format!("failed to create runtime: {e}"))
                })?,
        );

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

        let mut options = eryx::preinit::PreInitOptions::new(&stdlib_path)
            .imports(imports.unwrap_or_default())
            .extensions(extensions)
            .callbacks(declarations);
        if let Some(path) = &final_site_packages {
            options = options.site_packages(path);
        }
        if let Some(code) = setup_code {
            options = options.setup_code(code);
        }

        // Run pre-initialization
        let preinit_bytes = runtime.block_on(async {
            eryx::preinit::pre_initialize_with_options(options)
                .await
                .map_err(|e| {
                    InitializationError::new_err(format!("pre-initialization failed: {e}"))
                })
        })?;

        // Pre-compile to native code for faster instantiation
        let precompiled = eryx::PythonExecutor::precompile(&preinit_bytes)
            .map_err(|e| InitializationError::new_err(format!("pre-compilation failed: {e}")))?;
        let precompiled = make_precompiled_artifact(precompiled, cache);

        Ok(Self {
            precompiled,
            stdlib_path,
            site_packages_path: final_site_packages,
            extracted_packages: Arc::new(extracted_packages),
            callbacks: callbacks.map(Bound::unbind),
            data_files_dir: None,
            runtime,
        })
    }

    /// Load a sandbox factory from a file.
    ///
    /// This loads a previously saved factory, which is much faster than
    /// creating a new one (~10ms vs ~3-5s).
    ///
    /// Args:
    ///     path: Path to the saved factory file.
    ///     callbacks: The callbacks the factory was created with, if any. The
    ///         file holds their baked declarations but not the Python callables,
    ///         so pass the same callbacks here to get the setup-free fast path;
    ///         a different set still works, it just installs per sandbox.
    ///     cache: Whether to cache the pre-compiled component in the process-global
    ///         cache. When enabled, a BLAKE3 content hash is computed once during
    ///         loading and subsequent ``create_sandbox()`` calls skip component
    ///         deserialization entirely (~0.8ms vs ~8ms per call). Defaults to True.
    ///
    /// Returns:
    ///     A SandboxFactory loaded from the file.
    ///
    /// Raises:
    ///     InitializationError: If loading fails.
    ///
    /// Example:
    ///     factory = SandboxFactory.load("/path/to/jinja2-factory.bin")
    ///     sandbox = factory.create_sandbox()
    #[staticmethod]
    #[pyo3(signature = (path, *, site_packages=None, callbacks=None, cache=true))]
    fn load(
        path: PathBuf,
        site_packages: Option<PathBuf>,
        callbacks: Option<Bound<'_, PyAny>>,
        cache: bool,
    ) -> PyResult<Self> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    InitializationError::new_err(format!("failed to create runtime: {e}"))
                })?,
        );

        // Get embedded resources for stdlib path
        let embedded = eryx::embedded::EmbeddedResources::get().map_err(eryx_error_to_py)?;
        let stdlib_path = embedded.stdlib().to_path_buf();

        let file_bytes = std::fs::read(&path).map_err(|e| {
            InitializationError::new_err(format!(
                "failed to load factory from {}: {e}",
                path.display()
            ))
        })?;

        let (precompiled_bytes, data_files_dir, data_files_path) = parse_factory_file(&file_bytes)
            .map_err(|e| {
                InitializationError::new_err(format!(
                    "failed to parse factory file {}: {e}",
                    path.display()
                ))
            })?;

        let precompiled = make_precompiled_artifact(precompiled_bytes, cache);

        let site_packages_path = site_packages.or(data_files_path);

        Ok(Self {
            precompiled,
            stdlib_path,
            site_packages_path,
            extracted_packages: Arc::new(Vec::new()),
            callbacks: callbacks.map(Bound::unbind),
            data_files_dir,
            runtime,
        })
    }

    /// Save the sandbox factory to a file.
    ///
    /// The saved file can be loaded later with `SandboxFactory.load()`,
    /// which is much faster than creating a new factory.
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
        let data_files = self
            .site_packages_path
            .as_ref()
            .map(|p| collect_data_files(p))
            .transpose()
            .map_err(|e| {
                InitializationError::new_err(format!("failed to collect data files: {e}"))
            })?
            .unwrap_or_default();

        let factory_bytes =
            build_factory_file(self.precompiled.as_bytes(), &data_files).map_err(|e| {
                InitializationError::new_err(format!("failed to build factory file: {e}"))
            })?;

        std::fs::write(&path, factory_bytes).map_err(|e| {
            InitializationError::new_err(format!(
                "failed to save factory to {}: {e}",
                path.display()
            ))
        })?;
        Ok(())
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
    ///     resource_limits: Optional session limits. Factory defaults are no
    ///         execution, memory, or fuel limit, a 10-second callback timeout,
    ///         and 1000 callback invocations.
    ///     network: Optional network configuration.
    ///     callbacks: Optional callbacks that sandboxed code can invoke.
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
    #[pyo3(signature = (*, vfs=None, vfs_mount_path=None, resource_limits=None, network=None, callbacks=None, volumes=None, on_stdout=None, on_stderr=None, result_variable=None))]
    #[allow(clippy::too_many_arguments)]
    fn create_session(
        &self,
        py: Python<'_>,
        vfs: Option<crate::vfs::VfsStorage>,
        vfs_mount_path: Option<String>,
        resource_limits: Option<ResourceLimits>,
        network: Option<NetConfig>,
        callbacks: Option<Bound<'_, PyAny>>,
        volumes: Option<Vec<(String, String, bool)>>,
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
        let limits = resource_limits.unwrap_or(ResourceLimits {
            execution_timeout_ms: None,
            callback_timeout_ms: Some(10_000),
            max_memory_bytes: None,
            max_callback_invocations: Some(1000),
            max_fuel: None,
            max_vfs_bytes: None,
        });
        let limits: eryx::ResourceLimits = (&limits).into();
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
            None,
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
    #[pyo3(signature = (*, site_packages=None, resource_limits=None, network=None, callbacks=None, secrets=None, scrub_stdout=None, scrub_stderr=None, scrub_files=None, volumes=None, on_stdout=None, on_stderr=None))]
    #[allow(clippy::too_many_arguments)]
    fn create_sandbox(
        &self,
        py: Python<'_>,
        site_packages: Option<PathBuf>,
        resource_limits: Option<ResourceLimits>,
        network: Option<NetConfig>,
        callbacks: Option<Bound<'_, PyAny>>,
        secrets: Option<Bound<'_, PyDict>>,
        scrub_stdout: Option<bool>,
        scrub_stderr: Option<bool>,
        scrub_files: Option<bool>,
        volumes: Option<Vec<(String, String, bool)>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
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

        // Apply volume mounts if provided
        if let Some(vols) = volumes {
            for (host_path, guest_path, read_only) in vols {
                let volume = if read_only {
                    eryx::VolumeMount::read_only(host_path, guest_path)
                } else {
                    eryx::VolumeMount::new(host_path, guest_path)
                };
                builder = builder.with_volume(volume);
            }
        }

        // Apply output handler if stdout/stderr callbacks are provided
        if on_stdout.is_some() || on_stderr.is_some() {
            builder = builder.with_output_handler(PyOutputHandler {
                on_stdout,
                on_stderr,
            });
        }

        let inner = builder.build().map_err(eryx_error_to_py)?;

        Sandbox::from_inner(inner, Arc::clone(&self.runtime))
    }

    /// Get the size of the pre-compiled runtime in bytes.
    #[getter]
    fn size_bytes(&self) -> usize {
        self.precompiled.len()
    }

    /// Get the pre-compiled runtime as bytes.
    ///
    /// This can be used for custom serialization or inspection.
    fn to_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.precompiled.as_bytes())
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
// Factory file format (v2 envelope)
// =============================================================================
//
// [4 bytes: magic "ERYX"]
// [4 bytes: version (2, little-endian)]
// [8 bytes: precompiled_len (little-endian)]
// [precompiled_len bytes: Wasmtime precompiled module]
// [8 bytes: data_files_len (little-endian)]
// [data_files_len bytes: zstd-compressed tar of data files]
//
// Version 1 (legacy): raw precompiled bytes with no envelope. Detected by
// checking whether the first 4 bytes match the magic; if not, treat as v1.

/// Collect non-Python data files from a site-packages directory.
///
/// Returns a list of (relative_path, file_contents) pairs. Skips `.py` and
/// `.pyc` files since those are already in the interpreter snapshot.
fn collect_data_files(site_packages: &Path) -> Result<Vec<(String, Vec<u8>)>, std::io::Error> {
    let mut files = Vec::new();
    if !site_packages.exists() {
        return Ok(files);
    }
    for entry in walkdir::WalkDir::new(site_packages) {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path
            .extension()
            .is_some_and(|ext| ext == "py" || ext == "pyc")
        {
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

/// Build a v2 factory file with the envelope format.
fn build_factory_file(
    precompiled: &[u8],
    data_files: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, std::io::Error> {
    let data_files_tar_zst = if data_files.is_empty() {
        Vec::new()
    } else {
        let mut tar_builder = tar::Builder::new(Vec::new());
        for (rel_path, contents) in data_files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar_builder.append_data(&mut header, rel_path, contents.as_slice())?;
        }
        let tar_bytes = tar_builder.into_inner()?;
        zstd::encode_all(tar_bytes.as_slice(), 3)?
    };

    let precompiled_len = precompiled.len() as u64;
    let data_files_len = data_files_tar_zst.len() as u64;

    let total = 4 + 4 + 8 + precompiled.len() + 8 + data_files_tar_zst.len();
    let mut buf = Vec::with_capacity(total);

    buf.write_all(FACTORY_MAGIC)?;
    buf.write_all(&FACTORY_VERSION.to_le_bytes())?;
    buf.write_all(&precompiled_len.to_le_bytes())?;
    buf.write_all(precompiled)?;
    buf.write_all(&data_files_len.to_le_bytes())?;
    buf.write_all(&data_files_tar_zst)?;

    Ok(buf)
}

type ParsedFactory = (Vec<u8>, Option<tempfile::TempDir>, Option<PathBuf>);

/// Parse a factory file, handling both v1 (raw) and v2 (envelope) formats.
///
/// Returns (precompiled_bytes, optional_temp_dir, optional_site_packages_path).
fn parse_factory_file(bytes: &[u8]) -> Result<ParsedFactory, std::io::Error> {
    if bytes.len() >= 4 && &bytes[..4] == FACTORY_MAGIC {
        parse_factory_v2(bytes)
    } else {
        Ok((bytes.to_vec(), None, None))
    }
}

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

fn parse_factory_v2(bytes: &[u8]) -> Result<ParsedFactory, std::io::Error> {
    let header_size = 4 + 4 + 8; // magic + version + precompiled_len
    if bytes.len() < header_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "factory file too short for v2 header",
        ));
    }

    let version = read_le_u32(&bytes[4..8])?;
    if version != FACTORY_VERSION {
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

    let compressed = &bytes[data_start..data_end];
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
    Ok((precompiled_bytes, Some(temp_dir), Some(site_packages_path)))
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

    #[test]
    fn v1_round_trip() {
        let raw = b"some precompiled bytes";
        let (parsed, temp_dir, path) = parse_factory_file(raw).unwrap();
        assert_eq!(parsed, raw);
        assert!(temp_dir.is_none());
        assert!(path.is_none());
    }

    #[test]
    fn v2_no_data_files() {
        let precompiled = b"precompiled wasm module";
        let factory_bytes = build_factory_file(precompiled, &[]).unwrap();

        assert_eq!(&factory_bytes[..4], FACTORY_MAGIC);
        let version = u32::from_le_bytes(factory_bytes[4..8].try_into().unwrap());
        assert_eq!(version, FACTORY_VERSION);

        let (parsed, temp_dir, path) = parse_factory_file(&factory_bytes).unwrap();
        assert_eq!(parsed, precompiled);
        assert!(temp_dir.is_none());
        assert!(path.is_none());
    }

    #[test]
    fn v2_with_data_files() {
        let precompiled = b"precompiled wasm module";
        let data_files = vec![
            ("tzdata/zoneinfo/UTC".to_string(), b"TZif data".to_vec()),
            (
                "tzdata/zoneinfo/Asia/Tokyo".to_string(),
                b"TZif tokyo data".to_vec(),
            ),
        ];
        let factory_bytes = build_factory_file(precompiled, &data_files).unwrap();
        let (parsed, temp_dir, path) = parse_factory_file(&factory_bytes).unwrap();

        assert_eq!(parsed, precompiled);
        assert!(temp_dir.is_some());
        let path = path.unwrap();

        let utc = std::fs::read(path.join("tzdata/zoneinfo/UTC")).unwrap();
        assert_eq!(utc, b"TZif data");

        let tokyo = std::fs::read(path.join("tzdata/zoneinfo/Asia/Tokyo")).unwrap();
        assert_eq!(tokyo, b"TZif tokyo data");
    }

    #[test]
    fn v2_rejects_truncated_header() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FACTORY_MAGIC);
        // Too short — missing version and lengths
        assert!(parse_factory_file(&bytes).is_err());
    }

    #[test]
    fn v2_rejects_bad_version() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FACTORY_MAGIC);
        bytes.extend_from_slice(&99u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        assert!(parse_factory_file(&bytes).is_err());
    }

    #[test]
    fn collect_data_files_skips_py() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();

        std::fs::create_dir_all(base.join("pkg")).unwrap();
        std::fs::write(base.join("pkg/__init__.py"), "# python").unwrap();
        std::fs::write(base.join("pkg/module.pyc"), "bytecode").unwrap();
        std::fs::write(base.join("pkg/data.bin"), "binary data").unwrap();
        std::fs::write(base.join("pkg/config.json"), "{}").unwrap();

        let files = collect_data_files(base).unwrap();
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"pkg/__init__.py"));
        assert!(!names.contains(&"pkg/module.pyc"));
        assert!(names.contains(&"pkg/data.bin"));
        assert!(names.contains(&"pkg/config.json"));
    }

    #[test]
    fn collect_data_files_nonexistent_dir() {
        let files = collect_data_files(Path::new("/nonexistent/path")).unwrap();
        assert!(files.is_empty());
    }
}
