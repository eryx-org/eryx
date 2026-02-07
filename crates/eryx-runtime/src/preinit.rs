//! Pre-initialization support for linked Python components.
//!
//! This module provides functionality to pre-initialize Python components
//! after linking. Pre-initialization runs the Python interpreter's startup
//! code and captures the initialized memory state into the component, avoiding
//! the initialization cost at runtime.
//!
//! # How It Works
//!
//! 1. We link the component with real WASI imports
//! 2. We use `wasmtime-wizer` to instrument the component (adding state accessors)
//! 3. We instantiate the instrumented component - Python initializes
//! 4. Optionally run imports (e.g., `import numpy`) to capture more state
//! 5. Call `finalize-preinit` to reset WASI file handle state
//! 6. The memory state is captured and embedded into the original component
//! 7. The resulting component starts with Python already initialized
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
use std::collections::HashSet;
use std::path::Path;
use tempfile::TempDir;
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Instance, Linker, ResourceTable, Val},
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wizer::{WasmtimeWizerComponent, Wizer};

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
    let imports: Vec<String> = imports.iter().map(|s| (*s).to_string()).collect();

    // Link the component with real WASI adapter.
    let original_component = link_with_extensions(extensions)
        .map_err(|e| anyhow!("Failed to link component with extensions: {}", e))?;

    // Phase 1: Instrument the component (synchronous).
    // This adds state accessor exports that wasmtime-wizer uses to read
    // memory/global state for the snapshot.
    let wizer = Wizer::new();
    let (cx, instrumented_wasm) = wizer
        .instrument_component(&original_component)
        .context("Failed to instrument component")?;

    // Phase 2: Instantiate and run the instrumented component.
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.async_support(true);

    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, &instrumented_wasm)?;

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
            python_stdlib,
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
    let temp_dir = if let Some(site_pkg) = site_packages {
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

    // Phase 3: Snapshot the initialized state back into the component.
    let snapshot_bytes = wizer
        .snapshot_component(
            cx,
            &mut WasmtimeWizerComponent {
                store: &mut store,
                instance,
            },
        )
        .await
        .context("Failed to pre-initialize component")?;

    // Phase 4: Restore _initialize exports stripped by wasmtime-wizer.
    //
    // wasmtime-wizer removes _initialize exports from all pre-initialized modules,
    // but the component's CoreInstance sections still reference them as instantiation
    // arguments. We add back empty (no-op) _initialize functions so the component
    // remains valid when loaded into wasmtime.
    restore_initialize_exports(&snapshot_bytes)
}

/// Restore `_initialize` exports that wasmtime-wizer strips during snapshot.
///
/// wasmtime-wizer's rewrite step removes `_initialize` from all pre-initialized
/// modules. However, the component's `CoreInstance` sections still reference
/// `_initialize` as instantiation arguments. This function adds back no-op
/// `_initialize` function exports to any module that's missing one.
fn restore_initialize_exports(component_bytes: &[u8]) -> Result<Vec<u8>> {
    // Pass 1: Find which modules have _initialize and which import it.
    let mut modules_with_init: HashSet<u32> = HashSet::new();
    let mut any_module_imports_init = false;
    let mut module_index = 0u32;

    for payload in wasmparser::Parser::new(0).parse_all(component_bytes) {
        if let wasmparser::Payload::ModuleSection {
            unchecked_range: range,
            ..
        } = payload?
        {
            let module_bytes = &component_bytes[range.start..range.end];
            // Use a fresh parser at offset 0 for the module slice
            for inner in wasmparser::Parser::new(0).parse_all(module_bytes) {
                match inner? {
                    wasmparser::Payload::ExportSection(reader) => {
                        for export in reader {
                            if export?.name == "_initialize" {
                                modules_with_init.insert(module_index);
                            }
                        }
                    }
                    wasmparser::Payload::ImportSection(reader) => {
                        for import in reader {
                            if import?.name == "_initialize" {
                                any_module_imports_init = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            module_index += 1;
        }
    }

    if !any_module_imports_init {
        return Ok(component_bytes.to_vec());
    }

    // Pass 2: Rebuild the component, adding _initialize to modules that lack it.
    let mut component = wasm_encoder::Component::new();
    module_index = 0;
    let mut depth = 0u32;

    for payload in wasmparser::Parser::new(0).parse_all(component_bytes) {
        let payload = payload?;

        // Track nesting depth — only process top-level sections
        match &payload {
            wasmparser::Payload::Version { .. } => {
                if depth > 0 {
                    // Nested component/module version — skip, handled by parent
                    depth += 1;
                    continue;
                }
                depth += 1;
                continue; // Skip — Component::new() writes the header
            }
            wasmparser::Payload::End { .. } => {
                depth -= 1;
                continue; // Skip — finish() writes this
            }
            _ => {
                if depth > 1 {
                    // Inside a nested module/component — skip individual payloads
                    continue;
                }
            }
        }

        match payload {
            wasmparser::Payload::ModuleSection {
                unchecked_range: range,
                ..
            } => {
                let module_bytes = &component_bytes[range.start..range.end];

                if !modules_with_init.contains(&module_index) {
                    let patched = add_noop_initialize(module_bytes)?;
                    component.section(&wasm_encoder::RawSection {
                        id: wasm_encoder::ComponentSectionId::CoreModule as u8,
                        data: &patched,
                    });
                } else {
                    component.section(&wasm_encoder::RawSection {
                        id: wasm_encoder::ComponentSectionId::CoreModule as u8,
                        data: module_bytes,
                    });
                }
                module_index += 1;
            }
            other => {
                if let Some((id, range)) = other.as_section() {
                    component.section(&wasm_encoder::RawSection {
                        id,
                        data: &component_bytes[range.start..range.end],
                    });
                }
            }
        }
    }

    Ok(component.finish())
}

/// Add a no-op `_initialize` function export to a core module.
///
/// Parses the module to find type/function counts, then rebuilds it
/// section-by-section, appending a new type (if needed), function declaration,
/// code body, and export entry for `_initialize`.
fn add_noop_initialize(module_bytes: &[u8]) -> Result<Vec<u8>> {
    use wasm_encoder::reencode::{Reencode, RoundtripReencoder};

    let mut num_types = 0u32;
    let mut num_imported_funcs = 0u32;
    let mut num_defined_funcs = 0u32;
    let mut noop_type_idx = None;

    // First pass: count types/functions and find existing () -> () type
    for payload in wasmparser::Parser::new(0).parse_all(module_bytes) {
        match payload? {
            wasmparser::Payload::TypeSection(reader) => {
                for ty in reader.into_iter() {
                    let ty = ty?;
                    for sub in ty.types() {
                        if let wasmparser::CompositeInnerType::Func(func_ty) =
                            &sub.composite_type.inner
                            && func_ty.params().is_empty()
                            && func_ty.results().is_empty()
                        {
                            noop_type_idx = Some(num_types);
                        }
                        num_types += 1;
                    }
                }
            }
            wasmparser::Payload::ImportSection(reader) => {
                for import in reader {
                    if matches!(import?.ty, wasmparser::TypeRef::Func(_)) {
                        num_imported_funcs += 1;
                    }
                }
            }
            wasmparser::Payload::FunctionSection(reader) => {
                num_defined_funcs = reader.count();
            }
            wasmparser::Payload::CodeSectionStart { .. } => {}
            _ => {}
        }
    }

    let num_funcs = num_imported_funcs + num_defined_funcs;
    let noop_type = noop_type_idx.unwrap_or(num_types);
    let noop_func_index = num_funcs;
    let needs_new_type = noop_type_idx.is_none();

    // Second pass: rebuild module using reencode for most sections.
    // For the code section, we use the saved range to create a CodeSectionReader.
    let mut encoder = wasm_encoder::Module::new();
    let mut reencode = RoundtripReencoder;

    for payload in wasmparser::Parser::new(0).parse_all(module_bytes) {
        match payload? {
            wasmparser::Payload::Version { .. } => {}
            wasmparser::Payload::TypeSection(reader) => {
                let mut types = wasm_encoder::TypeSection::new();
                reencode.parse_type_section(&mut types, reader)?;
                if needs_new_type {
                    types.ty().function([], []);
                }
                encoder.section(&types);
            }
            wasmparser::Payload::FunctionSection(reader) => {
                let mut funcs = wasm_encoder::FunctionSection::new();
                reencode.parse_function_section(&mut funcs, reader)?;
                funcs.function(noop_type);
                encoder.section(&funcs);
            }
            wasmparser::Payload::ExportSection(reader) => {
                let mut exports = wasm_encoder::ExportSection::new();
                reencode.parse_export_section(&mut exports, reader)?;
                exports.export(
                    "_initialize",
                    wasm_encoder::ExportKind::Func,
                    noop_func_index,
                );
                encoder.section(&exports);
            }
            wasmparser::Payload::CodeSectionStart { range, .. } => {
                // Re-parse the code section from the saved range and reencode it,
                // then append our noop function.
                let section_data = &module_bytes[range.start..range.end];
                let code_reader = wasmparser::CodeSectionReader::new(
                    wasmparser::BinaryReader::new(section_data, 0),
                )?;

                let mut code = wasm_encoder::CodeSection::new();
                reencode.parse_code_section(&mut code, code_reader)?;

                // Append noop function body
                let mut noop_func = wasm_encoder::Function::new([]);
                noop_func.instructions().end();
                code.function(&noop_func);
                encoder.section(&code);
            }
            wasmparser::Payload::CodeSectionEntry(_) => {
                // Already handled in CodeSectionStart above
            }
            wasmparser::Payload::End { .. } => {}
            other => {
                if let Some((id, range)) = other.as_section() {
                    encoder.section(&wasm_encoder::RawSection {
                        id,
                        data: &module_bytes[range.start..range.end],
                    });
                }
            }
        }
    }

    Ok(encoder.finish())
}

/// Add stub implementations for sandbox imports during pre-init.
fn add_sandbox_stubs(linker: &mut Linker<PreInitCtx>) -> Result<()> {
    use wasmtime::component::Accessor;

    // The component imports "invoke" for callbacks (wasmtime 40+ uses plain name)
    linker.root().func_wrap_concurrent(
        "invoke",
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

    // Add network stubs (TCP and TLS interfaces)
    add_network_stubs(linker)?;

    Ok(())
}

/// TCP error type for pre-init stubs.
/// This mirrors the WIT variant `tcp-error` so wasmtime can lower/lift it.
#[derive(
    wasmtime::component::ComponentType, wasmtime::component::Lift, wasmtime::component::Lower,
)]
#[component(variant)]
enum PreInitTcpError {
    #[component(name = "connection-refused")]
    ConnectionRefused,
    #[component(name = "connection-reset")]
    ConnectionReset,
    #[component(name = "timed-out")]
    TimedOut,
    #[component(name = "host-not-found")]
    HostNotFound,
    #[component(name = "io-error")]
    IoError(String),
    #[component(name = "not-permitted")]
    NotPermitted(String),
    #[component(name = "invalid-handle")]
    InvalidHandle,
}

/// TLS error type for pre-init stubs.
/// This mirrors the WIT variant `tls-error`.
#[derive(
    wasmtime::component::ComponentType, wasmtime::component::Lift, wasmtime::component::Lower,
)]
#[component(variant)]
enum PreInitTlsError {
    #[component(name = "tcp")]
    Tcp(PreInitTcpError),
    #[component(name = "handshake-failed")]
    HandshakeFailed(String),
    #[component(name = "certificate-error")]
    CertificateError(String),
    #[component(name = "invalid-handle")]
    InvalidHandle,
}

/// Add stub implementations for network imports during pre-init.
///
/// These stubs return errors if called - networking isn't available during pre-init.
/// The stubs are needed so the component can be instantiated.
///
/// Note: The WIT declares these as sync `func` but we use fiber-based async on the host
/// (`func_wrap_async`), which appears blocking to the guest but allows async I/O on the host.
fn add_network_stubs(linker: &mut Linker<PreInitCtx>) -> Result<()> {
    // Get or create the eryx:net/tcp interface
    let mut tcp_instance = linker
        .instance("eryx:net/tcp@0.1.0")
        .context("Failed to get eryx:net/tcp instance")?;

    // tcp.connect: func(host: string, port: u16) -> result<tcp-handle, tcp-error>
    tcp_instance.func_wrap_async(
        "connect",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>, (_host, _port): (String, u16)| {
            Box::new(async move {
                Ok((Result::<u32, PreInitTcpError>::Err(
                    PreInitTcpError::NotPermitted(
                        "networking not available during pre-init".into(),
                    ),
                ),))
            })
        },
    )?;

    // tcp.read: func(handle: tcp-handle, len: u32) -> result<list<u8>, tcp-error>
    tcp_instance.func_wrap_async(
        "read",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>, (_handle, _len): (u32, u32)| {
            Box::new(async move {
                Ok((Result::<Vec<u8>, PreInitTcpError>::Err(
                    PreInitTcpError::NotPermitted(
                        "networking not available during pre-init".into(),
                    ),
                ),))
            })
        },
    )?;

    // tcp.write: func(handle: tcp-handle, data: list<u8>) -> result<u32, tcp-error>
    tcp_instance.func_wrap_async(
        "write",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>, (_handle, _data): (u32, Vec<u8>)| {
            Box::new(async move {
                Ok((Result::<u32, PreInitTcpError>::Err(
                    PreInitTcpError::NotPermitted(
                        "networking not available during pre-init".into(),
                    ),
                ),))
            })
        },
    )?;

    // tcp.close: func(handle: tcp-handle)
    tcp_instance.func_wrap(
        "close",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>, (_handle,): (u32,)| {
            // No-op - handle doesn't exist anyway
            Ok(())
        },
    )?;

    // Get or create the eryx:net/tls interface
    let mut tls_instance = linker
        .instance("eryx:net/tls@0.1.0")
        .context("Failed to get eryx:net/tls instance")?;

    // tls.upgrade: func(tcp: tcp-handle, hostname: string) -> result<tls-handle, tls-error>
    tls_instance.func_wrap_async(
        "upgrade",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>,
         (_tcp_handle, _hostname): (u32, String)| {
            Box::new(async move {
                Ok((Result::<u32, PreInitTlsError>::Err(
                    PreInitTlsError::HandshakeFailed(
                        "networking not available during pre-init".into(),
                    ),
                ),))
            })
        },
    )?;

    // tls.read: func(handle: tls-handle, len: u32) -> result<list<u8>, tls-error>
    tls_instance.func_wrap_async(
        "read",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>, (_handle, _len): (u32, u32)| {
            Box::new(async move {
                Ok((Result::<Vec<u8>, PreInitTlsError>::Err(
                    PreInitTlsError::HandshakeFailed(
                        "networking not available during pre-init".into(),
                    ),
                ),))
            })
        },
    )?;

    // tls.write: func(handle: tls-handle, data: list<u8>) -> result<u32, tls-error>
    tls_instance.func_wrap_async(
        "write",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>, (_handle, _data): (u32, Vec<u8>)| {
            Box::new(async move {
                Ok((Result::<u32, PreInitTlsError>::Err(
                    PreInitTlsError::HandshakeFailed(
                        "networking not available during pre-init".into(),
                    ),
                ),))
            })
        },
    )?;

    // tls.close: func(handle: tls-handle)
    tls_instance.func_wrap(
        "close",
        |_ctx: wasmtime::StoreContextMut<'_, PreInitCtx>, (_handle,): (u32,)| {
            // No-op - handle doesn't exist anyway
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
