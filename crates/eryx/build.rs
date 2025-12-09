//! Build script for the eryx crate.
//!
//! When the `embedded-runtime` feature is enabled, this pre-compiles the Python
//! runtime WASM component to native code at build time, enabling the safe
//! `with_embedded_runtime()` API that provides ~50x faster sandbox creation
//! with zero configuration.

// Build scripts should panic on errors, so expect/unwrap are appropriate here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

fn main() {
    #[cfg(feature = "embedded-runtime")]
    embedded_runtime::precompile();
}

#[cfg(feature = "embedded-runtime")]
mod embedded_runtime {
    use std::path::PathBuf;

    pub fn precompile() {
        // Only rerun if the runtime WASM changes
        println!("cargo::rerun-if-changed=../eryx-runtime/runtime.wasm");

        let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
        let wasm_path = PathBuf::from("../eryx-runtime/runtime.wasm");

        // Check if runtime.wasm exists
        if !wasm_path.exists() {
            panic!(
                "runtime.wasm not found at {:?}. \
                 Build the eryx-runtime crate first, or disable the `embedded-runtime` feature.",
                wasm_path.canonicalize().unwrap_or(wasm_path)
            );
        }

        // Read the WASM bytes
        let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read runtime.wasm");

        // Create a wasmtime engine with the same configuration as PythonExecutor
        let mut config = wasmtime::Config::new();
        config.async_support(true);
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);

        // Configure pooling allocator for fast instantiation
        let mut pool_config = wasmtime::PoolingAllocationConfig::default();
        pool_config.max_component_instance_size(1 << 20); // 1 MB
        pool_config.max_memories_per_component(10);
        pool_config.max_tables_per_component(10);
        pool_config.total_memories(100);
        pool_config.total_tables(100);
        pool_config.total_stacks(100);
        pool_config.total_core_instances(100);
        pool_config.total_component_instances(100);

        config.allocation_strategy(wasmtime::InstanceAllocationStrategy::Pooling(pool_config));

        // Enable memory optimizations
        config.memory_init_cow(true);

        let engine = wasmtime::Engine::new(&config).expect("Failed to create wasmtime engine");

        // Pre-compile the component
        let precompiled = engine
            .precompile_component(&wasm_bytes)
            .expect("Failed to precompile runtime.wasm");

        // Write the pre-compiled bytes
        let cwasm_path = out_dir.join("runtime.cwasm");
        std::fs::write(&cwasm_path, &precompiled).expect("Failed to write runtime.cwasm");
    }
}
