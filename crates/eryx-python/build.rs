//! Build script for eryx-python PyO3 bindings.
//!
//! This script configures the Python extension module linking.

fn main() {
    pyo3_build_config::add_extension_module_link_args();
}
