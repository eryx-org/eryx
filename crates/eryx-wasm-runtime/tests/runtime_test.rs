//! Test to verify the runtime component can be instantiated and called.
//!
//! # Prerequisites
//!
//! Before running tests, you need to extract the Python stdlib from componentize-py:
//!
//! ```bash
//! # From the eryx-wasm-runtime crate directory:
//! mkdir -p tests/python-stdlib tests/site-packages
//! tar -xf ../eryx-runtime/.venv/lib/python3.12/site-packages/componentize_py/python-lib.tar.zst \
//!     -C tests/python-stdlib
//! ```
//!
//! # Running
//!
//! ```bash
//! cargo test --package eryx-wasm-runtime --test runtime_test
//! ```

use std::io::Cursor;
use std::path::PathBuf;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wit_component::{StringEncoding, embed_component_metadata};

fn decompress_zstd(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(Cursor::new(data)).expect("failed to decompress")
}

struct State {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// Load a library from decompressed dir, or decompress from .zst if needed
fn load_lib(libs_dir: &std::path::Path, name: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let decompressed = libs_dir.join("decompressed").join(name);
    if decompressed.exists() {
        return Ok(std::fs::read(&decompressed)?);
    }

    let compressed = libs_dir.join(format!("{name}.zst"));
    if compressed.exists() {
        return Ok(decompress_zstd(&std::fs::read(&compressed)?));
    }

    Err(format!("Library not found: {name} (checked {decompressed:?} and {compressed:?})").into())
}

fn build_component() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project_root = manifest_dir.parent().unwrap().parent().unwrap();
    let libs_dir = project_root.join("crates/eryx-runtime/libs");

    // Path to our runtime .so
    let runtime_path = manifest_dir.join("target/liberyx_runtime.so");
    if !runtime_path.exists() {
        panic!(
            "Runtime not found at {}. Run ./build.sh first",
            runtime_path.display()
        );
    }

    let runtime = std::fs::read(&runtime_path)?;

    // Load base libraries
    let libc = load_lib(&libs_dir, "libc.so")?;
    let libcxx = load_lib(&libs_dir, "libc++.so")?;
    let libcxxabi = load_lib(&libs_dir, "libc++abi.so")?;
    let wasi_clocks = load_lib(&libs_dir, "libwasi-emulated-process-clocks.so")?;
    let wasi_signal = load_lib(&libs_dir, "libwasi-emulated-signal.so")?;
    let wasi_mman = load_lib(&libs_dir, "libwasi-emulated-mman.so")?;
    let wasi_getpid = load_lib(&libs_dir, "libwasi-emulated-getpid.so")?;
    let libpython = load_lib(&libs_dir, "libpython3.14.so")?;
    let adapter = load_lib(&libs_dir, "wasi_snapshot_preview1.reactor.wasm")?;

    // Parse the runtime.wit file
    let wit_path = project_root.join("crates/eryx-runtime/runtime.wit");
    let mut resolve = wit_parser::Resolve::default();
    let (pkg_id, _) = resolve.push_path(&wit_path)?;
    let world_id = resolve.select_world(&[pkg_id], Some("sandbox"))?;

    // Generate bindings pointing to our runtime
    let mut opts = wit_dylib::DylibOpts {
        interpreter: Some("liberyx_runtime.so".to_string()),
        async_: wit_dylib::AsyncFilterSet::default(),
    };

    let mut bindings = wit_dylib::create(&resolve, world_id, Some(&mut opts));
    embed_component_metadata(&mut bindings, &resolve, world_id, StringEncoding::UTF8)?;

    // Link - order matters! Dependencies must come before dependents
    let linker = wit_component::Linker::default()
        .validate(true)
        .use_built_in_libdl(true)
        // WASI emulation libs
        .library("libwasi-emulated-process-clocks.so", &wasi_clocks, false)?
        .library("libwasi-emulated-signal.so", &wasi_signal, false)?
        .library("libwasi-emulated-mman.so", &wasi_mman, false)?
        .library("libwasi-emulated-getpid.so", &wasi_getpid, false)?
        // C/C++ runtime
        .library("libc.so", &libc, false)?
        .library("libc++abi.so", &libcxxabi, false)?
        .library("libc++.so", &libcxx, false)?
        // Python
        .library("libpython3.14.so", &libpython, false)?
        // Our runtime and bindings
        .library("liberyx_runtime.so", &runtime, false)?
        .library("liberyx_bindings.so", &bindings, false)?
        // WASI adapter
        .adapter("wasi_snapshot_preview1", &adapter)?;

    Ok(linker.encode()?)
}

#[tokio::test]
async fn test_instantiate_component() -> Result<(), Box<dyn std::error::Error>> {
    println!("Building component...");
    let component_bytes = build_component()?;
    println!("Component size: {} bytes", component_bytes.len());

    // Create engine with async and component model support
    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);

    let engine = Engine::new(&config)?;

    println!("Loading component into wasmtime...");
    let component = Component::new(&engine, &component_bytes)?;

    // Set up paths for Python stdlib
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let stdlib_path = manifest_dir.join("tests/python-stdlib");
    let site_packages_path = manifest_dir.join("tests/site-packages");

    if !stdlib_path.exists() {
        panic!(
            "Python stdlib not found at {}. Extract python-lib.tar.zst first.",
            stdlib_path.display()
        );
    }

    // Create site-packages if it doesn't exist
    std::fs::create_dir_all(&site_packages_path)?;

    // Create WASI context with preopened directories for Python
    let wasi = WasiCtxBuilder::new()
        .inherit_stdio()
        // Set PYTHONPATH so Python can find stdlib during initialization
        .env("PYTHONPATH", "/python-stdlib:/site-packages")
        // Mount stdlib at /python-stdlib (matches what initialize_python expects)
        .preopened_dir(
            &stdlib_path,
            "/python-stdlib",
            wasmtime_wasi::DirPerms::READ,
            wasmtime_wasi::FilePerms::READ,
        )?
        // Mount site-packages
        .preopened_dir(
            &site_packages_path,
            "/site-packages",
            wasmtime_wasi::DirPerms::READ,
            wasmtime_wasi::FilePerms::READ,
        )?
        .build();

    let state = State {
        ctx: wasi,
        table: ResourceTable::new(),
    };

    let mut store = Store::new(&engine, state);

    // Create linker and add WASI
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    // Add stub implementations for sandbox imports
    use wasmtime::component::{Accessor, Val};

    // [async]invoke: async func(name: string, arguments-json: string) -> result<string, string>
    // This implementation handles a few test callbacks:
    // - "get_time": returns a fixed timestamp as JSON
    // - "echo": echoes back the arguments
    // - "add": adds two numbers
    linker.root().func_wrap_concurrent(
        "[async]invoke",
        |_accessor: &Accessor<State>, (name, args): (String, String)| {
            Box::pin(async move {
                let result: Result<String, String> = match name.as_str() {
                    "get_time" => Ok(r#"{"timestamp": 1234567890}"#.to_string()),
                    "echo" => {
                        // Echo back the arguments
                        Ok(args)
                    }
                    "add" => {
                        // Simple JSON parsing for {"a": x, "b": y}
                        // Extract numbers using basic string operations
                        let extract_num = |key: &str| -> i64 {
                            args.find(&format!("\"{key}\":"))
                                .and_then(|pos| {
                                    let start = pos + key.len() + 3; // skip "key":
                                    let rest = &args[start..];
                                    let rest = rest.trim_start();
                                    let end = rest
                                        .find(|c: char| !c.is_ascii_digit() && c != '-')
                                        .unwrap_or(rest.len());
                                    rest[..end].parse().ok()
                                })
                                .unwrap_or(0)
                        };
                        let a = extract_num("a");
                        let b = extract_num("b");
                        Ok(format!(r#"{{"result": {}}}"#, a + b))
                    }
                    "http.get" => {
                        // Extract URL from {"url": "..."}
                        let url = args
                            .find("\"url\":")
                            .and_then(|pos| {
                                let rest = &args[pos + 6..];
                                let rest = rest.trim_start();
                                if rest.starts_with('"') {
                                    let rest = &rest[1..];
                                    rest.find('"').map(|end| &rest[..end])
                                } else {
                                    None
                                }
                            })
                            .unwrap_or("unknown");
                        Ok(format!(
                            r#"{{"status": 200, "url": "{url}", "body": "test response"}}"#
                        ))
                    }
                    _ => Err(format!("unknown callback: {name}")),
                };
                Ok((result,))
            })
        },
    )?;

    // list-callbacks: func() -> list<callback-info>
    // Returns a list of available callbacks for testing
    linker.root().func_new(
        "list-callbacks",
        |_ctx: wasmtime::StoreContextMut<'_, State>,
         _func_ty: wasmtime::component::types::ComponentFunc,
         _params: &[Val],
         results: &mut [Val]| {
            // Create callback info records
            let callbacks = vec![
                // Simple callback
                Val::Record(vec![
                    ("name".to_string(), Val::String("get_time".to_string())),
                    (
                        "description".to_string(),
                        Val::String("Get current timestamp".to_string()),
                    ),
                    (
                        "parameters-schema-json".to_string(),
                        Val::String("{}".to_string()),
                    ),
                ]),
                // Echo callback
                Val::Record(vec![
                    ("name".to_string(), Val::String("echo".to_string())),
                    (
                        "description".to_string(),
                        Val::String("Echo back arguments".to_string()),
                    ),
                    (
                        "parameters-schema-json".to_string(),
                        Val::String("{}".to_string()),
                    ),
                ]),
                // Add callback
                Val::Record(vec![
                    ("name".to_string(), Val::String("add".to_string())),
                    (
                        "description".to_string(),
                        Val::String("Add two numbers".to_string()),
                    ),
                    (
                        "parameters-schema-json".to_string(),
                        Val::String(r#"{"a": "number", "b": "number"}"#.to_string()),
                    ),
                ]),
                // Dotted namespace callback
                Val::Record(vec![
                    ("name".to_string(), Val::String("http.get".to_string())),
                    (
                        "description".to_string(),
                        Val::String("HTTP GET request".to_string()),
                    ),
                    (
                        "parameters-schema-json".to_string(),
                        Val::String(r#"{"url": "string"}"#.to_string()),
                    ),
                ]),
            ];

            results[0] = Val::List(callbacks);
            Ok(())
        },
    )?;

    // report-trace: func(lineno: u32, event-json: string, context-json: string)
    linker.root().func_new(
        "report-trace",
        |_ctx: wasmtime::StoreContextMut<'_, State>,
         _func_ty: wasmtime::component::types::ComponentFunc,
         _params: &[Val],
         _results: &mut [Val]| { Ok(()) },
    )?;

    println!("Instantiating component...");
    let instance = linker.instantiate_async(&mut store, &component).await?;

    println!("SUCCESS! Component instantiated");

    // Get the execute function
    // execute: async func(code: string) -> result<string, string>
    println!("Looking for execute function...");

    let execute = instance
        .get_typed_func::<(String,), (Result<String, String>,)>(&mut store, "[async]execute")
        .or_else(|_| {
            instance.get_typed_func::<(String,), (Result<String, String>,)>(&mut store, "execute")
        })?;

    // Test 1: Simple print statement
    println!("Test 1: execute('print(1+1)')...");
    let (result,) = execute
        .call_async(&mut store, ("print(1+1)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert_eq!(output.trim(), "2", "print(1+1) should output '2'");
        }
        Err(error) => {
            panic!("Test 1 failed with error: {error}");
        }
    }

    // Test 2: Multiple prints
    println!("Test 2: Multiple print statements...");
    let (result,) = execute
        .call_async(&mut store, ("print('hello')\nprint('world')".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert_eq!(output, "hello\nworld\n", "Should have two lines of output");
        }
        Err(error) => {
            panic!("Test 2 failed with error: {error}");
        }
    }

    // Test 3: Variable assignment (no output)
    println!("Test 3: Variable assignment with no output...");
    let (result,) = execute
        .call_async(&mut store, ("x = 42".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert_eq!(output, "", "Assignment should produce no output");
        }
        Err(error) => {
            panic!("Test 3 failed with error: {error}");
        }
    }

    // Test 4: Syntax error should return Err
    println!("Test 4: Syntax error...");
    let (result,) = execute
        .call_async(&mut store, ("def broken(".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            panic!("Test 4 should have failed, but got: {output:?}");
        }
        Err(error) => {
            println!("  Expected error: {error}");
            assert!(
                error.contains("SyntaxError") || error.contains("syntax"),
                "Error should mention syntax: {error}"
            );
        }
    }

    // Test 5: Runtime error should return Err
    println!("Test 5: Runtime error (NameError)...");
    let (result,) = execute
        .call_async(&mut store, ("print(undefined_variable)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            panic!("Test 5 should have failed, but got: {output:?}");
        }
        Err(error) => {
            println!("  Expected error: {error}");
            assert!(
                error.contains("NameError") || error.contains("undefined"),
                "Error should mention NameError: {error}"
            );
        }
    }

    // Test 6: State persists between calls
    println!("Test 6: State persistence...");
    let (result,) = execute
        .call_async(&mut store, ("my_var = 'persisted'".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;
    assert!(result.is_ok(), "Assignment should succeed");

    let (result,) = execute
        .call_async(&mut store, ("print(my_var)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert_eq!(
                output.trim(),
                "persisted",
                "Variable should persist between calls"
            );
        }
        Err(error) => {
            panic!("Test 6 failed with error: {error}");
        }
    }

    // Test 7: Import stdlib module
    println!("Test 7: Import stdlib (math)...");
    let (result,) = execute
        .call_async(&mut store, ("import math; print(math.pi)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.starts_with("3.14"),
                "math.pi should start with 3.14: {output}"
            );
        }
        Err(error) => {
            panic!("Test 7 failed with error: {error}");
        }
    }

    // =========================================================================
    // State Management Tests
    // =========================================================================

    // Get the state management functions
    let snapshot_state = instance
        .get_typed_func::<(), (Result<Vec<u8>, String>,)>(&mut store, "[async]snapshot-state")
        .or_else(|_| {
            instance.get_typed_func::<(), (Result<Vec<u8>, String>,)>(&mut store, "snapshot-state")
        })?;

    let restore_state = instance
        .get_typed_func::<(Vec<u8>,), (Result<(), String>,)>(&mut store, "[async]restore-state")
        .or_else(|_| {
            instance
                .get_typed_func::<(Vec<u8>,), (Result<(), String>,)>(&mut store, "restore-state")
        })?;

    let clear_state = instance
        .get_typed_func::<(), ()>(&mut store, "[async]clear-state")
        .or_else(|_| instance.get_typed_func::<(), ()>(&mut store, "clear-state"))?;

    // Test 8: Snapshot state
    println!("Test 8: Snapshot state...");
    // First set some variables
    let (result,) = execute
        .call_async(
            &mut store,
            ("x = 42\ny = 'hello'\nz = [1, 2, 3]".to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;
    assert!(result.is_ok(), "Setting variables should succeed");

    // Take a snapshot
    let (snapshot_result,) = snapshot_state.call_async(&mut store, ()).await?;
    snapshot_state.post_return_async(&mut store).await?;

    let snapshot_data = match snapshot_result {
        Ok(data) => {
            println!("  OK: Snapshot taken, {} bytes", data.len());
            assert!(!data.is_empty(), "Snapshot should not be empty");
            data
        }
        Err(error) => {
            panic!("Test 8 failed: {error}");
        }
    };

    // Test 9: Clear state
    println!("Test 9: Clear state...");
    clear_state.call_async(&mut store, ()).await?;
    clear_state.post_return_async(&mut store).await?;

    // Verify variables are gone
    let (result,) = execute
        .call_async(&mut store, ("print(x)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            panic!("Test 9 should have failed (x should be cleared), but got: {output:?}");
        }
        Err(error) => {
            println!("  OK: Variable x is cleared (error: {error})");
            assert!(
                error.contains("NameError") || error.contains("not defined"),
                "Error should indicate x is not defined: {error}"
            );
        }
    }

    // Test 10: Restore state
    println!("Test 10: Restore state...");
    let (restore_result,) = restore_state
        .call_async(&mut store, (snapshot_data,))
        .await?;
    restore_state.post_return_async(&mut store).await?;

    match &restore_result {
        Ok(()) => {
            println!("  OK: State restored");
        }
        Err(error) => {
            panic!("Test 10 failed: {error}");
        }
    }

    // Verify variables are restored
    let (result,) = execute
        .call_async(&mut store, ("print(x, y, z)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: Restored values: {output:?}");
            assert!(
                output.contains("42") && output.contains("hello") && output.contains("[1, 2, 3]"),
                "Restored values should match: {output}"
            );
        }
        Err(error) => {
            panic!("Test 10 verification failed: {error}");
        }
    }

    // =========================================================================
    // Callback Infrastructure Tests
    // =========================================================================
    // Note: invoke() is not yet fully implemented (raises RuntimeError).
    // These tests verify that the callback infrastructure is in place:
    // - list_callbacks() function exists and is callable
    // - invoke() function exists and raises the expected error
    // - Callback wrappers and namespaces are generated

    // Test 11: list_callbacks() function exists and is callable
    println!("Test 11: list_callbacks() is callable...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
# Verify list_callbacks exists and is callable
cbs = list_callbacks()
print(f"list_callbacks returned: {type(cbs).__name__}")
print(f"count: {len(cbs)}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.contains("list_callbacks returned: list"),
                "list_callbacks should return a list: {output}"
            );
        }
        Err(error) => {
            panic!("Test 11 failed: {error}");
        }
    }

    // Test 12: invoke() raises RuntimeError (not yet implemented)
    println!("Test 12: invoke() raises RuntimeError...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
try:
    invoke("get_time")
    print("ERROR: invoke should have raised RuntimeError")
except RuntimeError as e:
    print("OK: RuntimeError raised")
    error_msg = str(e)
    if "not yet implemented" in error_msg:
        print("CORRECT: error mentions 'not yet implemented'")
    else:
        print(f"ERROR: unexpected message: {error_msg}")
except Exception as e:
    print(f"ERROR: wrong exception type: {type(e).__name__}: {e}")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.contains("RuntimeError raised"),
                "invoke() should raise RuntimeError: {output}"
            );
            assert!(
                output.contains("CORRECT"),
                "Error should mention 'not yet implemented': {output}"
            );
        }
        Err(error) => {
            panic!("Test 12 failed: {error}");
        }
    }

    // Test 13: Direct callback wrapper raises RuntimeError
    // (Callback wrappers call invoke() under the hood)
    println!("Test 13: Callback wrappers raise RuntimeError...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
# Check if callback wrappers are generated and raise correct error
# Note: These only exist if callbacks were discovered from list-callbacks
try:
    # Try to call a wrapper - it should raise RuntimeError from invoke()
    if 'add' in dir():
        add(a=1, b=2)
        print("ERROR: add() should have raised RuntimeError")
    elif 'get_time' in dir():
        get_time()
        print("ERROR: get_time() should have raised RuntimeError")
    else:
        # No wrappers generated (callback discovery may have failed)
        # This is OK - just verify invoke raises error
        invoke("test")
        print("ERROR: invoke should have raised")
except RuntimeError as e:
    print("OK: RuntimeError raised")
    if "not yet implemented" in str(e):
        print("CORRECT: from callback system")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.contains("RuntimeError raised"),
                "Should raise RuntimeError: {output}"
            );
        }
        Err(error) => {
            panic!("Test 13 failed: {error}");
        }
    }

    // Test 14: Verify invoke and list_callbacks are in globals
    println!("Test 14: Callback functions in globals...");
    let (result,) = execute
        .call_async(
            &mut store,
            (r#"
has_invoke = 'invoke' in dir()
has_list = 'list_callbacks' in dir()
print(f"invoke in globals: {has_invoke}")
print(f"list_callbacks in globals: {has_list}")
if has_invoke and has_list:
    print("OK: callback functions available")
else:
    print("ERROR: missing callback functions")
"#
            .to_string(),),
        )
        .await?;
    execute.post_return_async(&mut store).await?;

    match &result {
        Ok(output) => {
            println!("  OK: {output:?}");
            assert!(
                output.contains("invoke in globals: True"),
                "invoke should be in globals: {output}"
            );
            assert!(
                output.contains("list_callbacks in globals: True"),
                "list_callbacks should be in globals: {output}"
            );
        }
        Err(error) => {
            panic!("Test 14 failed: {error}");
        }
    }

    println!("\nAll tests passed!");
    Ok(())
}
