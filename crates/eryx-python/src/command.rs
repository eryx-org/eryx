//! External commands for shell sandboxes (`eryx.Command`).

use std::collections::HashMap;
use std::path::PathBuf;

use pyo3::prelude::*;

use crate::error::eryx_error_to_py;

/// A WASI 0.2 command component that a shell sandbox can run as an external
/// command, such as uutils coreutils, ripgrep or curl built for
/// `wasm32-wasip2`.
///
/// Pass commands by name to a shell `Sandbox` or `Session`:
/// `Sandbox(language="shell", commands={"rg": eryx.Command.from_file("rg.wasm")})`.
/// The same `Command` under several names (a multicall binary, which picks
/// the tool from `argv[0]`) is compiled once. A child sees the sandbox's
/// volumes and nothing else of the host, and has no network.
#[pyclass(module = "eryx", frozen)]
#[derive(Debug)]
pub struct Command {
    pub(crate) inner: eryx::Command,
}

#[pymethods]
impl Command {
    /// Compile a command component from a `.wasm` file.
    ///
    /// Raises:
    ///     InitializationError: If the file can't be read or compiled, or
    ///         isn't a WASI 0.2 command.
    #[staticmethod]
    fn from_file(path: PathBuf) -> PyResult<Self> {
        eryx::Command::from_file(path)
            .map(|inner| Self { inner })
            .map_err(eryx_error_to_py)
    }

    /// Compile a command component from its `.wasm` bytes.
    ///
    /// Raises:
    ///     InitializationError: If the bytes don't compile, or aren't a WASI
    ///         0.2 command.
    #[staticmethod]
    fn from_bytes(wasm: Vec<u8>) -> PyResult<Self> {
        eryx::Command::from_bytes(&wasm)
            .map(|inner| Self { inner })
            .map_err(eryx_error_to_py)
    }

    /// Load a command precompiled by this pyeryx version's eryx
    /// (`eryx-precompile compile`, without `--language`).
    ///
    /// Only load files you trust: a precompiled file is native code that
    /// can't be fully validated, as for `SandboxFactory.load`.
    ///
    /// Raises:
    ///     InitializationError: If another eryx version precompiled it, or it
    ///         isn't a WASI 0.2 command.
    #[staticmethod]
    fn from_precompiled_file(path: PathBuf) -> PyResult<Self> {
        // SAFETY: documented above; the caller vouches for the file.
        #[allow(unsafe_code)]
        unsafe { eryx::Command::from_precompiled_file(path) }
            .map(|inner| Self { inner })
            .map_err(eryx_error_to_py)
    }

    fn __repr__(&self) -> &'static str {
        "Command(...)"
    }
}

/// A shell sandbox's commands, by name, as passed to `commands=`.
pub(crate) type Commands<'py> = HashMap<String, PyRef<'py, Command>>;

/// The shell's default commands, uutils coreutils, from the
/// `pyeryx-runtime-shell` companion package, if it's installed and has them.
pub(crate) fn coreutils(py: Python<'_>) -> PyResult<Option<eryx::Command>> {
    let path: PathBuf = match py
        .import("pyeryx_runtime_shell")
        .and_then(|module| module.call_method0("coreutils_path"))
    {
        Ok(path) => path.extract()?,
        // Not installed, or one without coreutils (a source checkout's
        // skeleton, or a stand-in without the function).
        Err(e)
            if e.is_instance_of::<pyo3::exceptions::PyModuleNotFoundError>(py)
                || e.is_instance_of::<pyo3::exceptions::PyImportError>(py)
                || e.is_instance_of::<pyo3::exceptions::PyFileNotFoundError>(py)
                || e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    // SAFETY: shipped in the companion package, trusted like its runtime.
    #[allow(unsafe_code)]
    unsafe { eryx::Command::from_precompiled_file(path) }
        .map(Some)
        .map_err(eryx_error_to_py)
}
