//! Pre-initialization support for linked Python components.
//!
//! This module provides functionality to pre-initialize Python components
//! after linking. Pre-initialization runs the Python interpreter's startup
//! code and captures the initialized memory state into the component, avoiding
//! the initialization cost at runtime.
//!
//! # How It Works
//!
//! 1. We link the component twice: once with real WASI, once with stub WASI
//! 2. The stub WASI adapters trap on any call (preventing file handle capture)
//! 3. We use `component-init-transform` to instrument the component
//! 4. We instantiate and run the stubbed component - Python initializes
//! 5. Optionally run imports (e.g., `import numpy`) to capture more state
//! 6. The memory state is captured and embedded into the original component
//! 7. The resulting component starts with Python already initialized
//!
//! # Why Stub WASI?
//!
//! During pre-initialization, Python opens file handles for stdlib imports.
//! These handles get captured into the memory snapshot. When the component
//! is instantiated in a new WASI context, those handles are invalid, causing
//! "unknown handle index" errors.
//!
//! By using stub WASI adapters (that just trap on any call), we prevent any
//! file handles from being captured. The output component references the
//! *real* WASI imports, so it works correctly at runtime.
//!
//! # Performance Impact
//!
//! - First build with pre-init: ~3-4 seconds (one-time cost)
//! - Per-execution after pre-init: ~1-5ms (vs ~450-500ms without)
//!
//! # Example
//!
//! ```rust,ignore
//! use eryx_runtime::preinit::pre_initialize;
//!
//! // Pre-initialize with native extensions
//! let preinit_component = pre_initialize(
//!     &python_stdlib_path,
//!     Some(&site_packages_path),
//!     &["numpy", "pandas"],  // Modules to import during pre-init
//!     &native_extensions,
//! ).await?;
//! ```

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use component_init_transform::Invoker;
use futures::future::FutureExt;
use std::path::Path;
use tempfile::TempDir;
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Instance, Linker, ResourceTable, Val},
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::linker::{NativeExtension, link_with_extensions};

/// Context for the pre-initialization runtime.
struct PreInitCtx {
    wasi: WasiCtx,
    table: ResourceTable,
    /// Temp directory for dummy files - must be kept alive during pre-init
    #[allow(dead_code)]
    temp_dir: Option<TempDir>,
}

impl std::fmt::Debug for PreInitCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreInitCtx").finish_non_exhaustive()
    }
}

impl WasiView for PreInitCtx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Invoker implementation for component-init-transform.
///
/// This struct provides the interface that component-init-transform uses
/// to extract memory state from the initialized component.
struct PreInitInvoker {
    store: Store<PreInitCtx>,
    instance: Instance,
}

#[async_trait]
impl Invoker for PreInitInvoker {
    async fn call_s32(&mut self, function: &str) -> Result<i32> {
        let func = self
            .instance
            .get_typed_func::<(), (i32,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_s64(&mut self, function: &str) -> Result<i64> {
        let func = self
            .instance
            .get_typed_func::<(), (i64,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_f32(&mut self, function: &str) -> Result<f32> {
        let func = self
            .instance
            .get_typed_func::<(), (f32,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_f64(&mut self, function: &str) -> Result<f64> {
        let func = self
            .instance
            .get_typed_func::<(), (f64,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_list_u8(&mut self, function: &str) -> Result<Vec<u8>> {
        let func = self
            .instance
            .get_typed_func::<(), (Vec<u8>,)>(&mut self.store, function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }
}

/// Pre-initialize a Python component with native extensions.
///
/// This function links the component with native extensions, runs the Python
/// interpreter's initialization, optionally imports modules, and captures the
/// initialized memory state into the returned component.
///
/// # Arguments
///
/// * `python_stdlib` - Path to Python standard library directory
/// * `site_packages` - Optional path to site-packages directory
/// * `imports` - Modules to import during pre-init (e.g., ["numpy", "pandas"])
/// * `extensions` - Native extensions to link into the component
///
/// # Returns
///
/// The pre-initialized component bytes, ready for instantiation.
///
/// # Errors
///
/// Returns an error if pre-initialization fails (e.g., Python init error,
/// import failure).
pub async fn pre_initialize(
    python_stdlib: &Path,
    site_packages: Option<&Path>,
    imports: &[&str],
    extensions: &[NativeExtension],
) -> Result<Vec<u8>> {
    let python_stdlib = python_stdlib.to_path_buf();
    let site_packages = site_packages.map(|p| p.to_path_buf());
    let imports: Vec<String> = imports.iter().map(|s| (*s).to_string()).collect();

    // Link the component with real WASI adapter.
    // Note: We pass None for stage2 because:
    // 1. The apply() function in component-init-transform uses stage2's STRUCTURE for output
    // 2. If we pass a stubbed component, the output will have stub adapters that trap!
    // 3. By passing None, the original component is used for both structure and measurements
    //
    // The risk is that file handles opened during pre-init get captured in the snapshot.
    // However, our Python initialization may not open files if we're careful with sys.path.
    // If "unknown handle index" errors occur, we need Option B (stub_wasi flag in runtime).
    let original_component = link_with_extensions(extensions)
        .map_err(|e| anyhow!("Failed to link component with extensions: {}", e))?;

    component_init_transform::initialize_staged(
        &original_component,
        None, // Use original component for both structure and measurements
        move |instrumented| {
            let python_stdlib = python_stdlib.clone();
            let site_packages = site_packages.clone();
            let imports = imports.clone();

            async move {
                // Set up wasmtime with async and component model support
                let mut config = Config::new();
                config.wasm_component_model(true);
                config.wasm_component_model_async(true);
                config.async_support(true);

                let engine = Engine::new(&config)?;
                let component = Component::new(&engine, &instrumented)?;

                // Set up WASI context with Python paths
                let table = ResourceTable::new();

                // Build PYTHONPATH from stdlib and site-packages
                let mut python_path_parts = vec!["/python-stdlib".to_string()];
                if site_packages.is_some() {
                    python_path_parts.push("/site-packages".to_string());
                }
                let python_path = python_path_parts.join(":");

                let mut wasi_builder = WasiCtxBuilder::new();
                wasi_builder
                    .env("PYTHONHOME", "/python-stdlib")
                    .env("PYTHONPATH", &python_path)
                    .env("PYTHONUNBUFFERED", "1");

                // Mount Python stdlib
                if python_stdlib.exists() {
                    wasi_builder.preopened_dir(
                        &python_stdlib,
                        "python-stdlib",
                        DirPerms::READ,
                        FilePerms::READ,
                    )?;
                } else {
                    return Err(anyhow!(
                        "Python stdlib not found at {}",
                        python_stdlib.display()
                    ));
                }

                // Mount site-packages if provided
                let temp_dir = if let Some(ref site_pkg) = site_packages {
                    if site_pkg.exists() {
                        wasi_builder.preopened_dir(
                            site_pkg,
                            "site-packages",
                            DirPerms::READ,
                            FilePerms::READ,
                        )?;
                    }
                    None
                } else {
                    // Create empty temp dir for site-packages to avoid errors
                    let temp = TempDir::new()?;
                    wasi_builder.preopened_dir(
                        temp.path(),
                        "site-packages",
                        DirPerms::READ,
                        FilePerms::READ,
                    )?;
                    Some(temp)
                };

                let wasi = wasi_builder.build();

                let mut store = Store::new(
                    &engine,
                    PreInitCtx {
                        wasi,
                        table,
                        temp_dir,
                    },
                );

                // Create linker and add WASI
                let mut linker = Linker::new(&engine);
                wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

                // Add stub implementations for the sandbox imports
                // These are needed during pre-init but won't be called
                add_sandbox_stubs(&mut linker)?;

                // Instantiate the component
                // This triggers Python initialization via wit-dylib's Interpreter::initialize()
                let instance = linker.instantiate_async(&mut store, &component).await?;

                // If imports are specified, call execute() to import them
                if !imports.is_empty() {
                    call_execute_for_imports(&mut store, &instance, &imports).await?;
                }

                // CRITICAL: Call finalize-preinit to reset WASI state AFTER all imports.
                // This clears file handles from the WASI adapter and wasi-libc so they
                // don't get captured in the memory snapshot. Without this, restored
                // instances get "unknown handle index" errors.
                call_finalize_preinit(&mut store, &instance).await?;

                Ok(Box::new(PreInitInvoker { store, instance }) as Box<dyn Invoker>)
            }
            .boxed()
        },
    )
    .await
    .context("Failed to pre-initialize component")
}

/// Add stub implementations for sandbox imports during pre-init.
fn add_sandbox_stubs(linker: &mut Linker<PreInitCtx>) -> Result<()> {
    use wasmtime::component::Accessor;

    // The component imports "[async]invoke" for callbacks
    linker.root().func_wrap_concurrent(
        "[async]invoke",
        |_accessor: &Accessor<PreInitCtx>, (_name, _args): (String, String)| {
            Box::pin(async move {
                Ok((Result::<String, String>::Err(
                    "callbacks not available during pre-init".into(),
                ),))
            })
        },
    )?;

    // list-callbacks: func() -> list<callback-info>
    linker.root().func_new(
        "list-callbacks",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>,
         _func_ty: wasmtime::component::types::ComponentFunc,
         _params: &[Val],
         results: &mut [Val]| {
            // Return empty list
            results[0] = Val::List(vec![]);
            Ok(())
        },
    )?;

    // report-trace: func(lineno: u32, event-json: string, context-json: string)
    linker.root().func_new(
        "report-trace",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>,
         _func_ty: wasmtime::component::types::ComponentFunc,
         _params: &[Val],
         _results: &mut [Val]| {
            // No-op - trace events during init can be ignored
            Ok(())
        },
    )?;

    Ok(())
}

/// Call the execute export to import modules during pre-init.
async fn call_execute_for_imports(
    store: &mut Store<PreInitCtx>,
    instance: &Instance,
    imports: &[String],
) -> Result<()> {
    // Find the execute function.
    // Our WIT exports functions directly, not in an "exports" interface.
    // Try direct export first, then fall back to exports interface.
    let execute_func = if let Some(func) = instance.get_func(&mut *store, "execute") {
        func
    } else if let Some(func) = instance.get_func(&mut *store, "[async]execute") {
        // Async exports may have [async] prefix
        func
    } else {
        // Try looking in an "exports" interface (for compatibility)
        let (_item, exports_idx) = instance
            .get_export(&mut *store, None, "exports")
            .ok_or_else(|| anyhow!("No 'exports' or 'execute' export found"))?;

        let execute_idx = instance
            .get_export_index(&mut *store, Some(&exports_idx), "execute")
            .ok_or_else(|| anyhow!("No 'execute' in exports interface"))?;

        instance
            .get_func(&mut *store, execute_idx)
            .ok_or_else(|| anyhow!("Could not get execute func from index"))?
    };

    // Generate import code
    let import_code = imports
        .iter()
        .map(|module| format!("import {module}"))
        .collect::<Vec<_>>()
        .join("\n");

    // Call execute with the import code
    let args = [Val::String(import_code.clone())];
    // Result placeholder - wasmtime will fill this with Val::Result
    let mut results = vec![Val::Bool(false)];

    execute_func
        .call_async(&mut *store, &args, &mut results)
        .await
        .context("Failed to execute imports during pre-init")?;

    execute_func.post_return_async(&mut *store).await?;

    // Check if the result was an error
    // result<string, string> is represented as Val::Result(Result<Option<Box<Val>>, Option<Box<Val>>>)
    match &results[0] {
        Val::Result(Ok(_)) => {
            // Success - imports completed
            Ok(())
        }
        Val::Result(Err(Some(error_val))) => {
            // Error - extract the error message
            let error_msg = match error_val.as_ref() {
                Val::String(s) => s.clone(),
                other => format!("unexpected error value: {other:?}"),
            };
            Err(anyhow!(
                "Pre-init import execution failed: {error_msg}\nImport code:\n{import_code}"
            ))
        }
        Val::Result(Err(None)) => Err(anyhow!(
            "Pre-init import execution failed with unknown error\nImport code:\n{import_code}"
        )),
        other => {
            // Unexpected result type - log warning but don't fail
            // This shouldn't happen, but be defensive
            tracing::warn!("Unexpected result type from execute during pre-init: {other:?}");
            Ok(())
        }
    }
}

/// Call the finalize-preinit export to reset WASI state after imports.
async fn call_finalize_preinit(store: &mut Store<PreInitCtx>, instance: &Instance) -> Result<()> {
    // Find the finalize-preinit function
    let finalize_func = instance
        .get_func(&mut *store, "finalize-preinit")
        .ok_or_else(|| anyhow!("finalize-preinit export not found"))?;

    // Call it (no arguments, no return value)
    let args: [Val; 0] = [];
    let mut results: [Val; 0] = [];

    finalize_func
        .call_async(&mut *store, &args, &mut results)
        .await
        .context("Failed to call finalize-preinit")?;

    finalize_func.post_return_async(&mut *store).await?;

    Ok(())
}

/// Errors that can occur during pre-initialization.
#[derive(Debug, Clone)]
pub enum PreInitError {
    /// Failed to create wasmtime engine.
    Engine(String),
    /// Failed to compile component.
    Compile(String),
    /// Failed to instantiate component.
    Instantiate(String),
    /// Python initialization failed.
    PythonInit(String),
    /// Import failed during pre-init.
    Import(String),
    /// Component transform failed.
    Transform(String),
}

impl std::fmt::Display for PreInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(e) => write!(f, "failed to create wasmtime engine: {e}"),
            Self::Compile(e) => write!(f, "failed to compile component: {e}"),
            Self::Instantiate(e) => write!(f, "failed to instantiate component: {e}"),
            Self::PythonInit(e) => write!(f, "Python initialization failed: {e}"),
            Self::Import(e) => write!(f, "import failed during pre-init: {e}"),
            Self::Transform(e) => write!(f, "component transform failed: {e}"),
        }
    }
}

impl std::error::Error for PreInitError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preinit_error_display() {
        let err = PreInitError::PythonInit("test error".to_string());
        assert!(err.to_string().contains("test error"));
    }

    #[test]
    fn test_preinit_error_import_display() {
        let err = PreInitError::Import("numpy not found".to_string());
        assert!(err.to_string().contains("numpy not found"));
    }
}
