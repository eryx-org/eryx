//! External commands for shell sandboxes (`eryx.Command`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use pyo3::prelude::*;

use crate::error::eryx_error_to_py;
use crate::sandbox::RuntimeSource;

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

/// The shell's default commands, uutils coreutils, from where its runtime
/// came from: the `pyeryx-runtime-shell` companion package or the cache
/// `eryx-precompile setup --language shell` writes. For a runtime given
/// explicitly, either, if one loads. None if there are none.
pub(crate) fn coreutils(py: Python<'_>, source: RuntimeSource) -> PyResult<Option<eryx::Command>> {
    match source {
        RuntimeSource::Companion => companion_coreutils(py)?.map(|path| load(&path)).transpose(),
        RuntimeSource::SetupCache => eryx::cache::setup_coreutils()
            .map(|path| load(&path))
            .transpose(),
        // Not this runtime's own coreutils, so one that doesn't load (another
        // eryx version's, say) is as good as none.
        RuntimeSource::Explicit | RuntimeSource::Embedded => {
            let candidates = [companion_coreutils(py)?, eryx::cache::setup_coreutils()];
            for path in candidates.into_iter().flatten() {
                if let Ok(command) = load(&path) {
                    return Ok(Some(command));
                }
            }
            Ok(None)
        }
    }
}

/// The companion package's `coreutils.cwasm`, if it's installed and has one.
fn companion_coreutils(py: Python<'_>) -> PyResult<Option<PathBuf>> {
    match py
        .import("pyeryx_runtime_shell")
        .and_then(|module| module.call_method0("coreutils_path"))
    {
        Ok(path) => path.extract().map(Some),
        // Not installed, or one without coreutils (a source checkout's
        // skeleton, or a stand-in without the function).
        Err(e)
            if e.is_instance_of::<pyo3::exceptions::PyModuleNotFoundError>(py)
                || e.is_instance_of::<pyo3::exceptions::PyImportError>(py)
                || e.is_instance_of::<pyo3::exceptions::PyFileNotFoundError>(py)
                || e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Load a precompiled coreutils once per path: it's 14 MB of native code,
/// which every shell Sandbox and Session would otherwise map again.
fn load(path: &Path) -> PyResult<eryx::Command> {
    static LOADED: Mutex<Option<HashMap<PathBuf, eryx::Command>>> = Mutex::new(None);
    let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);
    let loaded = loaded.get_or_insert_with(HashMap::new);
    if let Some(command) = loaded.get(path) {
        return Ok(command.clone());
    }
    // SAFETY: from the companion package or the private setup cache, trusted
    // like the runtime they came with.
    #[allow(unsafe_code)]
    let command = unsafe { eryx::Command::from_precompiled_file(path) }.map_err(|e| {
        crate::error::InitializationError::new_err(format!(
            "the shell's coreutils {}: {e}",
            path.display()
        ))
    })?;
    loaded.insert(path.to_owned(), command.clone());
    Ok(command)
}
