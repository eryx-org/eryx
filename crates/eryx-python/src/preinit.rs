//! Sandbox factory for Python.
//!
//! Provides the `SandboxFactory` class for creating sandboxes with custom packages.
//! The factory bundles packages and pre-imports into a reusable snapshot.

use std::path::{Path, PathBuf};

use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::error::{InitializationError, eryx_error_to_py};
use crate::resource_limits::ResourceLimits;
use crate::sandbox::Sandbox;

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
///     # Create sandboxes with packages already loaded (~10-20ms each)
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
    /// Pre-compiled component bytes (native code, not WASM).
    precompiled: Vec<u8>,
    /// Path to Python stdlib.
    stdlib_path: PathBuf,
    /// Path to site-packages (if any).
    site_packages_path: Option<PathBuf>,
    /// Extracted packages (kept alive to prevent temp dir cleanup).
    #[allow(dead_code)]
    extracted_packages: Vec<eryx::ExtractedPackage>,
}

#[pymethods]
impl SandboxFactory {
    /// Create a new sandbox factory with custom packages.
    ///
    /// This performs one-time initialization that can take 3-5 seconds,
    /// but subsequent sandbox creation will be very fast (~10-20ms).
    ///
    /// Args:
    ///     site_packages: Optional path to a directory containing Python packages.
    ///     packages: Optional list of paths to .whl or .tar.gz package files.
    ///         These are extracted and their native extensions are linked.
    ///     imports: Optional list of module names to pre-import during initialization.
    ///         Pre-imported modules are immediately available without import overhead.
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
    #[new]
    #[pyo3(signature = (*, site_packages=None, packages=None, imports=None))]
    fn new(
        site_packages: Option<PathBuf>,
        packages: Option<Vec<PathBuf>>,
        imports: Option<Vec<String>>,
    ) -> PyResult<Self> {
        // Create tokio runtime for async pre-initialization
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| InitializationError::new_err(format!("failed to create runtime: {e}")))?;

        // Get embedded resources for stdlib path
        let embedded = eryx::embedded::EmbeddedResources::get().map_err(eryx_error_to_py)?;
        let stdlib_path = embedded.stdlib().to_path_buf();

        // Process packages to extract site-packages and native extensions
        let (final_site_packages, extensions, extracted_packages) =
            process_packages(site_packages.as_ref(), packages.as_ref())?;

        // Convert imports to the format pre_initialize expects
        let import_refs: Vec<&str> = imports
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default();

        // Run pre-initialization
        let preinit_bytes = runtime.block_on(async {
            eryx::preinit::pre_initialize(
                &stdlib_path,
                final_site_packages.as_deref(),
                &import_refs,
                &extensions,
            )
            .await
            .map_err(|e| InitializationError::new_err(format!("pre-initialization failed: {e}")))
        })?;

        // Pre-compile to native code for faster instantiation
        let precompiled = eryx::PythonExecutor::precompile(&preinit_bytes)
            .map_err(|e| InitializationError::new_err(format!("pre-compilation failed: {e}")))?;

        Ok(Self {
            precompiled,
            stdlib_path,
            site_packages_path: final_site_packages,
            extracted_packages,
        })
    }

    /// Load a sandbox factory from a file.
    ///
    /// This loads a previously saved factory, which is much faster than
    /// creating a new one (~10ms vs ~3-5s).
    ///
    /// Args:
    ///     path: Path to the saved factory file.
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
    #[pyo3(signature = (path, *, site_packages=None))]
    fn load(path: PathBuf, site_packages: Option<PathBuf>) -> PyResult<Self> {
        // Get embedded resources for stdlib path
        let embedded = eryx::embedded::EmbeddedResources::get().map_err(eryx_error_to_py)?;
        let stdlib_path = embedded.stdlib().to_path_buf();

        // Load precompiled bytes from file
        let precompiled = std::fs::read(&path).map_err(|e| {
            InitializationError::new_err(format!(
                "failed to load factory from {}: {e}",
                path.display()
            ))
        })?;

        Ok(Self {
            precompiled,
            stdlib_path,
            site_packages_path: site_packages,
            extracted_packages: Vec::new(),
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
        std::fs::write(&path, &self.precompiled).map_err(|e| {
            InitializationError::new_err(format!(
                "failed to save factory to {}: {e}",
                path.display()
            ))
        })?;
        Ok(())
    }

    /// Create a new sandbox from this factory.
    ///
    /// This is fast (~10-20ms) because the packages are already bundled
    /// into the factory's snapshot.
    ///
    /// Args:
    ///     site_packages: Optional path to additional site-packages.
    ///         If not provided, uses the site-packages from initialization.
    ///     resource_limits: Optional resource limits for the sandbox.
    ///
    /// Returns:
    ///     A new Sandbox ready to execute Python code.
    ///
    /// Raises:
    ///     InitializationError: If sandbox creation fails.
    ///
    /// Example:
    ///     sandbox = preinit.create_sandbox()
    ///     result = sandbox.execute('print("Hello!")')
    #[pyo3(signature = (*, site_packages=None, resource_limits=None))]
    fn create_sandbox(
        &self,
        site_packages: Option<PathBuf>,
        resource_limits: Option<ResourceLimits>,
    ) -> PyResult<Sandbox> {
        // Use provided site_packages or fall back to the one from initialization
        let site_packages_path = site_packages.or_else(|| self.site_packages_path.clone());

        // Build sandbox from precompiled bytes
        // SAFETY: The precompiled bytes were created by PythonExecutor::precompile()
        // from a valid WASM component, so they are safe to deserialize.
        let mut builder = unsafe {
            eryx::Sandbox::builder()
                .with_precompiled_bytes(self.precompiled.clone())
                .with_python_stdlib(&self.stdlib_path)
        };

        if let Some(path) = site_packages_path {
            builder = builder.with_site_packages(path);
        }

        if let Some(limits) = resource_limits {
            builder = builder.with_resource_limits(limits.into());
        }

        let inner = builder.build().map_err(eryx_error_to_py)?;

        Sandbox::from_inner(inner)
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
        PyBytes::new(py, &self.precompiled)
    }

    fn __repr__(&self) -> String {
        format!(
            "SandboxFactory(size_bytes={}, site_packages={:?})",
            self.precompiled.len(),
            self.site_packages_path,
        )
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
                let dlopen_path = format!("/site-packages/{}", relative.display());

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
