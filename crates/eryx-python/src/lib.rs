//! Python bindings for the Eryx sandbox.
//!
//! This crate provides PyO3 bindings to expose Eryx's Python sandboxing
//! capabilities to Python users.
//!
//! # Quick Start
//!
//! ```python
//! import eryx
//!
//! sandbox = eryx.Sandbox()
//! result = sandbox.execute('print("Hello from the sandbox!")')
//! print(result.stdout)  # "Hello from the sandbox!\n"
//! ```

mod error;
mod preinit;
mod resource_limits;
mod result;
mod sandbox;

use pyo3::prelude::*;

/// Eryx: A Python sandbox powered by WebAssembly.
///
/// This module provides a secure sandbox for executing untrusted Python code.
/// The sandbox runs Python inside WebAssembly, providing complete isolation
/// from the host system.
///
/// Classes:
///     Sandbox: The main sandbox class for executing Python code.
///     ExecuteResult: Result of sandbox execution with stdout and stats.
///     ResourceLimits: Configuration for execution limits.
///
/// Exceptions:
///     EryxError: Base exception for all Eryx errors.
///     ExecutionError: Error during Python code execution.
///     InitializationError: Error during sandbox initialization.
///     ResourceLimitError: Resource limit exceeded during execution.
///     TimeoutError: Execution timed out.
///
/// Example:
///     >>> import eryx
///     >>> sandbox = eryx.Sandbox()
///     >>> result = sandbox.execute('print("Hello!")')
///     >>> print(result.stdout)
///     Hello!
#[pymodule]
fn _eryx(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Register classes
    m.add_class::<sandbox::Sandbox>()?;
    m.add_class::<preinit::PreInitializedRuntime>()?;
    m.add_class::<result::ExecuteResult>()?;
    m.add_class::<resource_limits::ResourceLimits>()?;

    // Register exceptions
    error::register_exceptions(m)?;

    // Module metadata
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;

    Ok(())
}
