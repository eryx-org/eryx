//! Stub WASI adapters for pre-initialization.
//!
//! During pre-initialization, we need to prevent Python from opening file handles
//! that would be captured in the memory snapshot and become invalid when the
//! component is instantiated in a new WASI context.
//!
//! This module generates "stub" WASI adapter modules where every function immediately
//! traps with `unreachable`. When we link the component with these stubs during
//! pre-init, any attempt to perform I/O will trap, preventing file handles from
//! being captured.
//!
//! The approach is based on componentize-py's stubwasi.rs implementation.
//!
//! # How It Works
//!
//! 1. Extract WASI imports from all libraries in the component
//! 2. Generate stub adapter modules that export those functions but just trap
//! 3. Link the component with stub adapters instead of the real WASI adapter
//! 4. Use the stubbed component for pre-initialization
//! 5. The output component references real WASI imports (not stubs)
//!
//! # Module Index Mapping
//!
//! The stubbed component may have a different number of adapter modules than the
//! original component. `component-init-transform` needs a mapping function to
//! translate module indices from the stubbed component to the original.

use std::collections::HashMap;

use anyhow::{Error, bail};
use wasm_encoder::{
    CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction as Ins, Module,
    TypeSection,
    reencode::{Reencode, RoundtripReencoder as R},
};
use wasmparser::{FuncType, Parser, Payload, TypeRef};

use crate::linker::{NativeExtension, base_libraries};

/// Result type for stub linking: (stubbed_component_bytes, module_index_map)
pub type LinkedStubModules = Option<(Vec<u8>, Box<dyn Fn(u32) -> u32 + Send + Sync>)>;

/// Link libraries with stub WASI adapters for pre-initialization.
///
/// This creates a component where all WASI imports are satisfied by stub modules
/// that trap on any call. This is used during pre-init to prevent file handles
/// from being captured in the memory snapshot.
///
/// # Arguments
///
/// * `extensions` - Native extensions to include (same as for normal linking)
///
/// # Returns
///
/// A tuple of (stubbed_component_bytes, module_index_map_fn), or None if there
/// are no WASI imports to stub.
pub fn link_stub_modules(extensions: &[NativeExtension]) -> Result<LinkedStubModules, Error> {
    use wit_component::Linker;

    // Decompress base libraries
    let libc = decompress_zstd(base_libraries::LIBC)?;
    let libcxx = decompress_zstd(base_libraries::LIBCXX)?;
    let libcxxabi = decompress_zstd(base_libraries::LIBCXXABI)?;
    let libpython = decompress_zstd(base_libraries::LIBPYTHON)?;
    let wasi_mman = decompress_zstd(base_libraries::LIBWASI_EMULATED_MMAN)?;
    let wasi_clocks = decompress_zstd(base_libraries::LIBWASI_EMULATED_PROCESS_CLOCKS)?;
    let wasi_getpid = decompress_zstd(base_libraries::LIBWASI_EMULATED_GETPID)?;
    let wasi_signal = decompress_zstd(base_libraries::LIBWASI_EMULATED_SIGNAL)?;
    let runtime = decompress_zstd(base_libraries::LIBERYX_RUNTIME)?;
    let bindings = decompress_zstd(base_libraries::LIBERYX_BINDINGS)?;

    // Collect all libraries with their bytes
    let libraries: Vec<(&str, &[u8], bool)> = vec![
        // WASI emulation libraries
        ("libwasi-emulated-process-clocks.so", &wasi_clocks, false),
        ("libwasi-emulated-signal.so", &wasi_signal, false),
        ("libwasi-emulated-mman.so", &wasi_mman, false),
        ("libwasi-emulated-getpid.so", &wasi_getpid, false),
        // C/C++ runtime
        ("libc.so", &libc, false),
        ("libc++abi.so", &libcxxabi, false),
        ("libc++.so", &libcxx, false),
        // Python
        ("libpython3.14.so", &libpython, false),
        // Our runtime and bindings
        ("liberyx_runtime.so", &runtime, false),
        ("liberyx_bindings.so", &bindings, false),
    ];

    // Collect WASI imports from all libraries
    let mut wasi_imports: HashMap<&str, HashMap<&str, FuncType>> = HashMap::new();

    for (name, module, _) in &libraries {
        add_wasi_imports(module, &mut wasi_imports)
            .map_err(|e| anyhow::anyhow!("failed to extract WASI imports from {}: {}", name, e))?;
    }

    // Also check extensions for WASI imports
    for ext in extensions {
        add_wasi_imports(&ext.bytes, &mut wasi_imports).map_err(|e| {
            anyhow::anyhow!("failed to extract WASI imports from {}: {}", ext.name, e)
        })?;
    }

    if wasi_imports.is_empty() {
        return Ok(None);
    }

    // Build linker with libraries
    let mut linker = Linker::default().validate(true).use_built_in_libdl(true);

    for (name, module, dl_openable) in &libraries {
        linker = linker
            .library(name, module, *dl_openable)
            .map_err(|e| anyhow::anyhow!("failed to add library {}: {}", name, e))?;
    }

    // Add native extensions
    for ext in extensions {
        linker = linker
            .library(&ext.name, &ext.bytes, true)
            .map_err(|e| anyhow::anyhow!("failed to add extension {}: {}", ext.name, e))?;
    }

    // Add stub adapters for each WASI module
    for (module, imports) in &wasi_imports {
        let stub = make_stub_adapter(module, imports);
        linker = linker
            .adapter(module, &stub)
            .map_err(|e| anyhow::anyhow!("failed to add stub adapter for {}: {}", module, e))?;
    }

    let component = linker.encode()?;

    // Module index mapping.
    //
    // As of this writing, `wit_component::Linker` generates a component such
    // that the first module is the `main` one, followed by any adapters,
    // followed by any libraries, followed by the `init` module, which is
    // finally followed by any shim modules.
    //
    // Given that the stubbed component may contain more adapters than the
    // non-stubbed version, we need to tell `component-init-transform` how to
    // translate module indexes from the former to the latter.
    //
    // TODO: this is pretty fragile in that it could silently break if
    // `wit_component::Linker`'s implementation changes. Can we make it more
    // robust?
    let old_adapter_count = 1u32; // The real component has just wasi_snapshot_preview1
    let new_adapter_count = u32::try_from(wasi_imports.len())?;
    assert!(
        new_adapter_count >= old_adapter_count,
        "expected at least {} WASI adapters, got {}",
        old_adapter_count,
        new_adapter_count
    );

    Ok(Some((
        component,
        Box::new(move |index: u32| {
            if index == 0 {
                // `main` module - stays at index 0
                0
            } else if index <= new_adapter_count {
                // adapter module - all map to the single real adapter
                old_adapter_count
            } else {
                // one of the other kinds of module (libraries, init, shims)
                index + old_adapter_count - new_adapter_count
            }
        }),
    )))
}

/// Extract WASI imports from a WASM module.
///
/// Scans the module's import section for imports from `wasi_snapshot_preview1`
/// or any `wasi:*` namespace.
fn add_wasi_imports<'a>(
    module: &'a [u8],
    imports: &mut HashMap<&'a str, HashMap<&'a str, FuncType>>,
) -> Result<(), Error> {
    let mut types = Vec::new();

    for payload in Parser::new(0).parse_all(module) {
        match payload? {
            Payload::TypeSection(reader) => {
                types = reader
                    .into_iter_err_on_gc_types()
                    .collect::<Result<Vec<_>, _>>()?;
            }

            Payload::ImportSection(reader) => {
                for import in reader {
                    let import = import?;

                    if import.module == "wasi_snapshot_preview1"
                        || import.module.starts_with("wasi:")
                    {
                        if let TypeRef::Func(ty) = import.ty {
                            imports
                                .entry(import.module)
                                .or_default()
                                .insert(import.name, types[usize::try_from(ty)?].clone());
                        } else {
                            bail!("encountered non-function import from WASI namespace")
                        }
                    }
                }
                break;
            }

            _ => {}
        }
    }

    Ok(())
}

/// Generate a stub WASI adapter module.
///
/// Creates a minimal WASM module that exports all the specified functions,
/// but each function body is just `unreachable` - it will trap if called.
#[allow(clippy::expect_used)] // Safe: bounded number of WASI functions, valid WASM types
fn make_stub_adapter(_module: &str, stubs: &HashMap<&str, FuncType>) -> Vec<u8> {
    let mut types = TypeSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();

    for (index, (name, ty)) in stubs.iter().enumerate() {
        let index = u32::try_from(index).expect("too many stub functions");

        // Add function type
        types.ty().function(
            ty.params()
                .iter()
                .map(|&v| R.val_type(v).expect("valid val type")),
            ty.results()
                .iter()
                .map(|&v| R.val_type(v).expect("valid val type")),
        );

        // Add function referencing the type
        functions.function(index);

        // Export the function
        exports.export(name, ExportKind::Func, index);

        // Function body: just unreachable (will trap if called)
        let mut function = Function::new([]);
        function.instruction(&Ins::Unreachable);
        function.instruction(&Ins::End);
        code.function(&function);
    }

    let mut module = Module::new();
    module.section(&types);
    module.section(&functions);
    module.section(&exports);
    module.section(&code);

    module.finish()
}

fn decompress_zstd(data: &[u8]) -> Result<Vec<u8>, Error> {
    use std::io::Cursor;
    zstd::decode_all(Cursor::new(data)).map_err(|e| anyhow::anyhow!("decompression failed: {}", e))
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Tests use expect for simplicity
mod tests {
    use super::*;

    #[test]
    fn test_make_stub_adapter() {
        let mut stubs = HashMap::new();

        // Create a simple function type: () -> i32
        let func_type = FuncType::new([], [wasmparser::ValType::I32]);
        stubs.insert("test_func", func_type);

        let adapter = make_stub_adapter("test", &stubs);

        // Verify it's valid WASM by parsing it
        let parser = Parser::new(0);
        for payload in parser.parse_all(&adapter) {
            payload.expect("valid WASM payload");
        }
    }

    #[test]
    fn test_make_stub_adapter_multiple_functions() {
        let mut stubs = HashMap::new();

        // fd_read: (i32, i32, i32, i32) -> i32
        stubs.insert(
            "fd_read",
            FuncType::new(
                [
                    wasmparser::ValType::I32,
                    wasmparser::ValType::I32,
                    wasmparser::ValType::I32,
                    wasmparser::ValType::I32,
                ],
                [wasmparser::ValType::I32],
            ),
        );

        // fd_write: (i32, i32, i32, i32) -> i32
        stubs.insert(
            "fd_write",
            FuncType::new(
                [
                    wasmparser::ValType::I32,
                    wasmparser::ValType::I32,
                    wasmparser::ValType::I32,
                    wasmparser::ValType::I32,
                ],
                [wasmparser::ValType::I32],
            ),
        );

        // clock_time_get: (i32, i64, i32) -> i32
        stubs.insert(
            "clock_time_get",
            FuncType::new(
                [
                    wasmparser::ValType::I32,
                    wasmparser::ValType::I64,
                    wasmparser::ValType::I32,
                ],
                [wasmparser::ValType::I32],
            ),
        );

        let adapter = make_stub_adapter("wasi_snapshot_preview1", &stubs);

        // Verify it's valid WASM
        let parser = Parser::new(0);
        let mut found_exports = 0;
        for payload in parser.parse_all(&adapter) {
            if let Payload::ExportSection(reader) = payload.expect("valid WASM payload") {
                for export in reader {
                    export.expect("valid export");
                    found_exports += 1;
                }
            }
        }
        assert_eq!(found_exports, 3);
    }
}
