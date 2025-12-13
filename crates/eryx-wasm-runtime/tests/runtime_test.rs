//! Test to verify the runtime component can be instantiated and called.
//!
//! Run with: cargo test --package eryx-wasm-runtime --test runtime_test

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

    // Load base libraries (zstd compressed)
    let libc = decompress_zstd(&std::fs::read(libs_dir.join("libc.so.zst"))?);
    let wasi_clocks = decompress_zstd(&std::fs::read(
        libs_dir.join("libwasi-emulated-process-clocks.so.zst"),
    )?);
    let adapter = decompress_zstd(&std::fs::read(
        libs_dir.join("wasi_snapshot_preview1.reactor.wasm.zst"),
    )?);

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

    // Link
    let linker = wit_component::Linker::default()
        .validate(true)
        .use_built_in_libdl(true)
        .library("libc.so", &libc, false)?
        .library("libwasi-emulated-process-clocks.so", &wasi_clocks, false)?
        .library("liberyx_runtime.so", &runtime, false)?
        .library("liberyx_bindings.so", &bindings, false)?
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

    // Create WASI context
    let wasi = WasiCtxBuilder::new().inherit_stdio().inherit_env().build();

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
    linker.root().func_wrap_concurrent(
        "[async]invoke",
        |_accessor: &Accessor<State>, (_name, _args): (String, String)| {
            Box::pin(async move { Ok((Result::<String, String>::Err("not implemented".into()),)) })
        },
    )?;

    // list-callbacks: func() -> list<callback-info>
    linker.root().func_new(
        "list-callbacks",
        |_ctx: wasmtime::StoreContextMut<'_, State>,
         _func_ty: wasmtime::component::types::ComponentFunc,
         _params: &[Val],
         results: &mut [Val]| {
            results[0] = Val::List(vec![]);
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

    // Try calling the execute export
    // execute: async func(code: string) -> result<string, string>
    println!("Looking for execute function...");

    let execute = instance
        .get_typed_func::<(String,), (Result<String, String>,)>(&mut store, "[async]execute")
        .or_else(|_| {
            instance.get_typed_func::<(String,), (Result<String, String>,)>(&mut store, "execute")
        })?;

    println!("Calling execute('print(1+1)')...");
    let (result,) = execute
        .call_async(&mut store, ("print(1+1)".to_string(),))
        .await?;
    execute.post_return_async(&mut store).await?;

    match result {
        Ok(output) => println!("Execute returned OK: {output}"),
        Err(error) => println!("Execute returned Err: {error}"),
    }

    Ok(())
}
