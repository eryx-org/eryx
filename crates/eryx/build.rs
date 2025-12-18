//! Build script for the eryx crate.
//!
//! When the `embedded` feature is enabled, this embeds the pre-compiled Python
//! runtime into the binary for fast sandbox creation.
//!
//! The `runtime.cwasm` file must be generated beforehand using:
//!   `mise run precompile-eryx-runtime`

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

        // Rerun if the file changes
        println!("cargo::rerun-if-changed=../eryx-runtime/runtime.cwasm");

        if !cwasm_path.exists() {
            panic!(
                "Pre-compiled runtime not found at {}.\n\
                 \n\
                 Run `mise run precompile-eryx-runtime` to generate it, \n\
                 or use `mise run test` which handles this automatically.",
                cwasm_path.display()
            );
        }

        // Copy to OUT_DIR
        let dest = out_dir.join("runtime.cwasm");
        std::fs::copy(&cwasm_path, &dest).expect("Failed to copy runtime.cwasm");
    }
}
