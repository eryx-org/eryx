//! External commands for shell sandboxes (`eryx.Command`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use pyo3::prelude::*;

use crate::error::eryx_error_to_py;
use crate::sandbox::RuntimeSource;

/// A WASI command that a shell sandbox can run as an external command, such
/// as uutils coreutils, ripgrep or curl built for `wasm32-wasip2`, `gh`
/// built for `wasm32-wasip3`, or a WASI preview 1 CLI (`wasm32-wasip1`),
/// which is adapted into a component when loaded.
///
/// Pass commands by name to a shell `Sandbox` or `Session`:
/// `Sandbox(language="shell", commands={"rg": eryx.Command.from_file("rg.wasm")})`.
/// The same `Command` under several names (a multicall binary, which picks
/// the tool from `argv[0]`) is compiled once. A child sees the sandbox's
/// volumes and nothing else of the host, and the network only through the
/// sandbox's `child_network=` proxy.
///
/// `allow_tunnels=True` lets the command open blind `CONNECT` tunnels through
/// that proxy, for protocols it can't inspect (e.g. git over SSH): requests in
/// a tunnel get no secret substitution (placeholders go out as they are) and
/// no HTTP rules, and hosts with rules are refused one.
#[pyclass(module = "eryx", frozen)]
#[derive(Debug)]
pub struct Command {
    pub(crate) inner: eryx::Command,
}

impl Command {
    fn new(inner: eryx::Command, allow_tunnels: bool) -> Self {
        let inner = if allow_tunnels {
            inner.allow_tunnels()
        } else {
            inner
        };
        Self { inner }
    }
}

#[pymethods]
impl Command {
    /// Compile a command from a `.wasm` file: a WASI 0.2 or 0.3 command
    /// component, or a WASI preview 1 command module, adapted into a
    /// component first.
    ///
    /// Raises:
    ///     InitializationError: If the file can't be read or compiled, or
    ///         isn't a WASI command.
    #[staticmethod]
    #[pyo3(signature = (path, *, allow_tunnels=false))]
    fn from_file(path: PathBuf, allow_tunnels: bool) -> PyResult<Self> {
        eryx::Command::from_file(path)
            .map(|inner| Self::new(inner, allow_tunnels))
            .map_err(eryx_error_to_py)
    }

    /// Compile a command from its `.wasm` bytes: a WASI 0.2 or 0.3 command
    /// component, or a WASI preview 1 command module, adapted into a
    /// component first.
    ///
    /// Raises:
    ///     InitializationError: If the bytes don't compile, or aren't a WASI
    ///         command.
    #[staticmethod]
    #[pyo3(signature = (wasm, *, allow_tunnels=false))]
    fn from_bytes(wasm: Vec<u8>, allow_tunnels: bool) -> PyResult<Self> {
        eryx::Command::from_bytes(&wasm)
            .map(|inner| Self::new(inner, allow_tunnels))
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
    ///         isn't a WASI 0.2 or 0.3 command.
    #[staticmethod]
    #[pyo3(signature = (path, *, allow_tunnels=false))]
    fn from_precompiled_file(path: PathBuf, allow_tunnels: bool) -> PyResult<Self> {
        // SAFETY: documented above; the caller vouches for the file.
        #[allow(unsafe_code)]
        unsafe { eryx::Command::from_precompiled_file(path) }
            .map(|inner| Self::new(inner, allow_tunnels))
            .map_err(eryx_error_to_py)
    }

    /// Python itself as a command, for a shell sandbox to register as
    /// `python` and `python3`:
    /// `commands={"python": py, "python3": py}` with `py = Command.python()`.
    /// The shell can then run `python3 -c CODE`, `python3 script.py` and
    /// `... | python3`. Each run is a fresh interpreter (pyeryx's embedded
    /// one, with the standard library) on the shell's files, environment,
    /// working directory and stdio, within the execution's limits, with no
    /// callbacks and no network.
    ///
    /// Raises:
    ///     InitializationError: If the embedded runtime can't be loaded.
    #[staticmethod]
    fn python() -> PyResult<Self> {
        eryx::Command::guest(eryx::Language::Python)
            .map(|inner| Self { inner })
            .map_err(eryx_error_to_py)
    }

    /// JavaScript as a command, for a shell sandbox to register as `node`
    /// and `js`: `commands={"node": js, "js": js}` with `js = Command.node()`.
    /// The shell can then run `node -e CODE`, `node -p EXPR`,
    /// `node script.js` and `... | node`. Each run is a fresh QuickJS
    /// instance with Node's `process`, `require` and `node:fs` on the
    /// shell's files, environment, working directory and stdio, within the
    /// execution's limits, with no callbacks and no network.
    ///
    /// The runtime is found as for `Sandbox(language="javascript")`:
    /// `runtime_path`, then `ERYX_RUNTIME_CWASM_JAVASCRIPT`, then the
    /// `pyeryx-runtime-javascript` package, then the cache `eryx-precompile
    /// setup --language javascript` writes.
    ///
    /// Raises:
    ///     InitializationError: If there's no JavaScript runtime, or it
    ///         can't be loaded.
    #[staticmethod]
    #[pyo3(signature = (*, runtime_path=None))]
    fn node(py: Python<'_>, runtime_path: Option<PathBuf>) -> PyResult<Self> {
        let (path, _) = crate::sandbox::find_runtime(py, eryx::Language::JavaScript, runtime_path)?;
        // SAFETY: an explicit runtime is trusted as `Sandbox(runtime_path=)`
        // is; the others come from the companion package or the private
        // setup cache.
        #[allow(unsafe_code)]
        unsafe {
            eryx::Command::guest_from_precompiled_file(eryx::Language::JavaScript, path, None)
        }
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
