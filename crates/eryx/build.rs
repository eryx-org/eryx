//! Build script for the eryx crate.
//!
//! When the `embedded` feature is enabled, this embeds the pre-compiled Python
//! runtime into the binary for fast sandbox creation.
//!
//! The build script prefers to use an existing `runtime.cwasm` file (which may
//! include Python pre-initialization for ~300x faster session creation) over
//! compiling from `runtime.wasm` directly.
//!
//! To generate the optimal `runtime.cwasm` with pre-initialization:
//!   `mise run precompile-eryx-runtime`
//!
//! If `runtime.cwasm` doesn't exist, falls back to compiling from `runtime.wasm`.

// Build scripts should panic on errors, so expect/unwrap are appropriate here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

fn main() {
    #[cfg(feature = "embedded")]
    embedded_runtime::prepare();
}

#[cfg(feature = "embedded")]
mod embedded_runtime {
    use std::path::PathBuf;

    pub fn prepare() {
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
        let cwasm_path = PathBuf::from("../eryx-runtime/runtime.cwasm");
        let wasm_path = PathBuf::from("../eryx-runtime/runtime.wasm");

        // Rerun if either file changes
        println!("cargo::rerun-if-changed=../eryx-runtime/runtime.cwasm");
        println!("cargo::rerun-if-changed=../eryx-runtime/runtime.wasm");

        // Prefer pre-existing runtime.cwasm (may include pre-initialization)
        if cwasm_path.exists() {
            println!(
                "cargo::warning=Using pre-compiled runtime from {}",
                cwasm_path.display()
            );

            // Copy to OUT_DIR
            let dest = out_dir.join("runtime.cwasm");
            std::fs::copy(&cwasm_path, &dest).expect("Failed to copy runtime.cwasm");
            return;
        }

        // Fall back to compiling from runtime.wasm
        if !wasm_path.exists() {
            panic!(
                "Neither runtime.cwasm nor runtime.wasm found. \
                 Run `mise run build-eryx-runtime` first, or disable the `embedded` feature.\n\
                 Looked for:\n  - {}\n  - {}",
                cwasm_path.display(),
                wasm_path.display()
            );
        }

        println!(
            "cargo::warning=Pre-compiled runtime.cwasm not found, compiling from runtime.wasm. \
             For faster session creation (~300x), run: mise run precompile-eryx-runtime"
        );

        // Read the WASM bytes
        let wasm_bytes = std::fs::read(&wasm_path).expect("Failed to read runtime.wasm");

        // Create a wasmtime engine with the same configuration as PythonExecutor
        let mut config = wasmtime::Config::new();
        config.async_support(true);
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);

        // Enable memory optimizations
        config.memory_init_cow(true);

        // Optimize for smaller generated code
        config.cranelift_opt_level(wasmtime::OptLevel::SpeedAndSize);

        let engine = wasmtime::Engine::new(&config).expect("Failed to create wasmtime engine");

        // Pre-compile the component
        let precompiled = engine
            .precompile_component(&wasm_bytes)
            .expect("Failed to precompile runtime.wasm");

        // Write the pre-compiled bytes
        let dest = out_dir.join("runtime.cwasm");
        std::fs::write(&dest, &precompiled).expect("Failed to write runtime.cwasm");
    }
}
