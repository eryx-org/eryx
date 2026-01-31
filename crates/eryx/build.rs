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

    /// Check if we're building the precompile example (which bootstraps without runtime.cwasm).
    fn is_precompile_bootstrap() -> bool {
        // The precompile example needs to build without runtime.cwasm existing.
        // Check if ERYX_PRECOMPILE_BOOTSTRAP is set to skip the file check.
        std::env::var("ERYX_PRECOMPILE_BOOTSTRAP").is_ok()
    }

    pub fn prepare() {
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
        let cwasm_path = PathBuf::from("../eryx-runtime/runtime.cwasm");

        // Rerun if the file changes
        println!("cargo::rerun-if-changed=../eryx-runtime/runtime.cwasm");

        if !cwasm_path.exists() {
            if is_precompile_bootstrap() {
                // Precompile bootstrap: create empty placeholder, will be replaced after precompile runs
                let dest = out_dir.join("runtime.cwasm");
                std::fs::write(&dest, b"").expect("Failed to write placeholder runtime.cwasm");
                return;
            }
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
