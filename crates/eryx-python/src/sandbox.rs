//! Sandbox wrapper for Python.
//!
//! Provides the main `Sandbox` class that Python users interact with.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use eryx::{CancellationToken, OutputHandler};

use crate::callback::extract_callbacks;
use crate::error::{InitializationError, eryx_error_to_py};
use crate::execution::{CancelToken, call_token, run_async, run_blocking};
use crate::net_config::NetConfig;
use crate::resource_limits::ResourceLimits;
use crate::result::{ExecuteResult, ReplayOutcome};

/// An `OutputHandler` backed by Python callables for stdout and stderr.
pub(crate) struct PyOutputHandler {
    pub(crate) on_stdout: Option<Py<PyAny>>,
    pub(crate) on_stderr: Option<Py<PyAny>>,
}

#[async_trait]
impl OutputHandler for PyOutputHandler {
    async fn on_output(&self, chunk: &[u8]) {
        if let Some(ref callback) = self.on_stdout {
            let chunk = chunk.to_vec();
            Python::attach(|py| {
                if let Err(e) = callback.call1(py, (chunk.as_slice(),)) {
                    e.print(py);
                }
            });
        }
    }

    async fn on_stderr(&self, chunk: &[u8]) {
        if let Some(ref callback) = self.on_stderr {
            let chunk = chunk.to_vec();
            Python::attach(|py| {
                if let Err(e) = callback.call1(py, (chunk.as_slice(),)) {
                    e.print(py);
                }
            });
        }
    }
}

/// A Python sandbox powered by WebAssembly.
///
/// The Sandbox executes Python code in complete isolation from the host system.
/// Each sandbox has its own memory space and cannot access files, network,
/// or other system resources unless explicitly provided via callbacks.
///
/// Example:
///     # Basic sandbox
///     sandbox = Sandbox()
///     result = sandbox.execute('print("Hello from the sandbox!")')
///     print(result.stdout)  # "Hello from the sandbox!"
///
///     # Sandbox with packages (e.g., jinja2)
///     sandbox = Sandbox(
///         packages=["/path/to/jinja2-3.1.2-py3-none-any.whl"],
///         site_packages="/path/to/extracted/site-packages",
///     )
///     result = sandbox.execute('from jinja2 import Template; print(Template("{{ x }}").render(x=42))')
#[pyclass(module = "eryx")]
pub struct Sandbox {
    // Note: We don't derive Debug because tokio::runtime::Runtime doesn't implement it.
    // The __repr__ method provides Python-side introspection instead.
    /// The underlying eryx Sandbox.
    inner: eryx::Sandbox,
    /// Tokio runtime for executing async code.
    /// We use Arc<Runtime> to allow sharing with SandboxFactory.
    runtime: Arc<tokio::runtime::Runtime>,
    /// Keeps the site-packages directories of the factory that created this
    /// sandbox alive: they're mounted when it executes, which can be after the
    /// factory is gone.
    _factory_files: Option<crate::preinit::FactoryFiles>,
}

#[pymethods]
impl Sandbox {
    /// Create a new sandbox with the embedded Python runtime.
    ///
    /// This creates a fast sandbox (~1-5ms) using the pre-initialized Python runtime.
    /// The sandbox has access to Python's standard library but no third-party packages.
    ///
    /// Each call to `execute()` runs in complete isolation - Python state is not
    /// preserved between calls. For persistent state (including file storage),
    /// use `Session` instead.
    ///
    /// For sandboxes with custom packages, use `SandboxFactory` instead.
    ///
    /// Execution trace collection (`sys.settrace`) is disabled for Python
    /// sandboxes; `ExecuteResult` does not expose trace events, so there is
    /// no per-line tracing overhead.
    ///
    /// Args:
    ///     language: The guest language to run: "python" (the default),
    ///         "javascript" or "shell". The JavaScript and shell runtimes aren't
    ///         in the pyeryx wheel; see `runtime_path`.
    ///     runtime_path: A precompiled runtime (.cwasm) to run instead of the
    ///         built-in one. For JavaScript and the shell it is otherwise looked
    ///         up in `ERYX_RUNTIME_CWASM_<LANGUAGE>`, then the
    ///         `pyeryx-runtime-<language>` package (`pip install
    ///         pyeryx[javascript]`), then the cache `eryx-precompile setup
    ///         --language <language>` writes.
    ///     resource_limits: Optional resource limits for execution.
    ///     network: Optional network configuration. If provided, enables networking.
    ///     callbacks: Optional callbacks that sandboxed code can invoke.
    ///         Can be a CallbackRegistry or a list of callback dicts.
    ///     env: Optional environment variables, by name: in `os.environ`
    ///         (Python), `process.env` (JavaScript), or exported in the
    ///         shell, so its commands inherit them. For configuration, not
    ///         secrets: values aren't scrubbed from output. A name must match
    ///         `[A-Za-z_][A-Za-z0-9_]*` and not be one the sandbox sets
    ///         (HOME, PATH, PWD, USER, TMPDIR, TERM, LANG, LC_ALL) or a
    ///         secret's.
    ///     callback_namespace: Install the callbacks (and MCP tools) under
    ///         this name rather than as top-level names: with "tools", a
    ///         callback `get_time` is `await tools.get_time()` (the command
    ///         `tools.get_time` in the shell). Must be an identifier.
    ///     commands: For the shell only: external commands by name, each an
    ///         `eryx.Command`. They replace the uutils coreutils tools
    ///         (cat, head, ls, sort, ...) of the same name, which the shell
    ///         gets by default from the `pyeryx-runtime-shell` package.
    ///     child_network: For the shell only, with `network`: how its
    ///         external commands reach the network, through a proxy on the
    ///         host. None (the default) keeps them offline; "inherit" gives
    ///         them `network`'s policy; a `NetConfig` only what both it and
    ///         `network` allow. Children see secrets only as their
    ///         placeholders, which the proxy replaces for the secrets' hosts.
    ///
    /// Returns:
    ///     A new Sandbox instance ready to execute Python code.
    ///
    /// Raises:
    ///     InitializationError: If the sandbox fails to initialize.
    ///
    /// Example:
    ///     # Default sandbox (stdlib only, no network)
    ///     sandbox = Sandbox()
    ///     result = sandbox.execute('import json; print(json.dumps([1, 2, 3]))')
    ///
    ///     # Sandbox with custom limits
    ///     limits = ResourceLimits(execution_timeout_ms=5000)
    ///     sandbox = Sandbox(resource_limits=limits)
    ///
    ///     # Sandbox with network access
    ///     net = NetConfig(allowed_hosts=["api.example.com"])
    ///     sandbox = Sandbox(network=net)
    ///
    ///     # Sandbox with callbacks
    ///     def get_time():
    ///         import time
    ///         return {"timestamp": time.time()}
    ///
    ///     sandbox = Sandbox(callbacks=[
    ///         {"name": "get_time", "fn": get_time, "description": "Returns current time"}
    ///     ])
    ///     result = sandbox.execute('t = await get_time(); print(t)')
    ///
    ///     # Sandbox with secrets
    ///     sandbox = Sandbox(
    ///         secrets={
    ///             "API_KEY": {"value": "sk-real-key", "allowed_hosts": ["api.example.com"]},
    ///         },
    ///         network=NetConfig(allowed_hosts=["api.example.com"]),
    ///     )
    ///
    ///     # For custom packages, use SandboxFactory instead:
    ///     factory = SandboxFactory(
    ///         packages=["/path/to/jinja2.whl", "/path/to/markupsafe.whl"],
    ///         imports=["jinja2"],
    ///     )
    ///     sandbox = factory.create_sandbox()
    #[new]
    #[pyo3(signature = (*, language=None, runtime_path=None, resource_limits=None, network=None, callbacks=None, mcp=None, secrets=None, env=None, scrub_stdout=None, scrub_stderr=None, scrub_result=None, scrub_files=None, volumes=None, on_stdout=None, on_stderr=None, result_variable=None, commands=None, child_network=None, callback_namespace=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        language: Option<&str>,
        runtime_path: Option<PathBuf>,
        resource_limits: Option<ResourceLimits>,
        network: Option<NetConfig>,
        callbacks: Option<Bound<'_, PyAny>>,
        mcp: Option<PyRef<'_, crate::mcp::MCPManager>>,
        secrets: Option<Bound<'_, PyDict>>,
        env: Option<std::collections::BTreeMap<String, String>>,
        scrub_stdout: Option<bool>,
        scrub_stderr: Option<bool>,
        scrub_result: Option<bool>,
        scrub_files: Option<bool>,
        volumes: Option<Vec<crate::vfs::VolumeSpec>>,
        on_stdout: Option<Py<PyAny>>,
        on_stderr: Option<Py<PyAny>>,
        result_variable: Option<String>,
        commands: Option<crate::command::Commands<'_>>,
        child_network: Option<Bound<'_, PyAny>>,
        callback_namespace: Option<String>,
    ) -> PyResult<Self> {
        let language = parse_language(language)?;
        let child_network = parse_child_network(language, network.is_some(), child_network)?;
        let runtime = crate::error::make_runtime()?;

        // Build the eryx sandbox with embedded runtime. Trace collection
        // (sys.settrace) is always off: the Python ExecuteResult does not
        // expose trace events, and the hook makes instruction-heavy scripts
        // orders of magnitude slower.
        let (builder, source) = base_builder(py, language, runtime_path)?;
        let mut builder = builder.with_trace_collection(false);

        // Apply resource limits if provided
        if let Some(limits) = resource_limits {
            builder = builder.with_resource_limits(limits.into());
        }

        // Apply the result-capture variable name if provided
        if let Some(name) = result_variable {
            builder = builder.with_result_variable(name);
        }

        // Apply network config if provided
        if let Some(net) = network {
            builder = builder.with_network(net.into());
        }

        if let Some(namespace) = callback_namespace {
            builder = builder.with_callback_namespace(namespace);
        }

        // Apply callbacks if provided
        if let Some(ref cbs) = callbacks {
            let python_callbacks = extract_callbacks(py, cbs)?;
            for callback in python_callbacks {
                builder = builder.with_callback(callback);
            }
        }

        // Apply MCP callbacks if provided
        if let Some(ref mcp_mgr) = mcp {
            for callback in mcp_mgr.as_callbacks() {
                builder = builder.with_callback(callback);
            }
        }

        // Apply secrets if provided
        let has_secrets = secrets.as_ref().is_some_and(|s| !s.is_empty());
        if let Some(ref secrets_dict) = secrets {
            builder = apply_secrets(builder, secrets_dict)?;
        }
        for (name, value) in env.into_iter().flatten() {
            builder = builder.with_env(name, value);
        }

        // Apply scrub policies (default to true when secrets are present)
        builder = builder.scrub_stdout(scrub_stdout.unwrap_or(has_secrets));
        builder = builder.scrub_stderr(scrub_stderr.unwrap_or(has_secrets));
        builder = builder.scrub_files(scrub_files.unwrap_or(has_secrets));
        // The structured result scrubbing is opt-in (default off) even when secrets
        // are present: it's a programmatic side channel, not human/LLM-facing output.
        builder = builder.scrub_result(scrub_result.unwrap_or(false));

        // Apply volume mounts if provided
        if let Some(vols) = volumes {
            builder = builder.with_volumes(vols.into_iter().map(eryx::VolumeMount::from));
        }

        // Apply output handler if stdout/stderr callbacks are provided
        if on_stdout.is_some() || on_stderr.is_some() {
            builder = builder.with_output_handler(PyOutputHandler {
                on_stdout,
                on_stderr,
            });
        }

        let inner = build_for(py, (builder, source), language, commands, child_network)?;

        Ok(Self {
            inner,
            runtime,
            _factory_files: None,
        })
    }

    /// Execute Python code in the sandbox.
    ///
    /// The code runs in complete isolation. Any output to stdout is captured
    /// and returned in the result.
    ///
    /// The GIL is released while the code runs. Ctrl-C (or any signal handler
    /// that raises) stops the run and re-raises the handler's exception.
    ///
    /// Args:
    ///     code: Python source code to execute.
    ///     cancel: Optional CancelToken; cancelling it stops the run.
    ///     allowed_commands: For the shell only: the commands this call may
    ///         run, out of the sandbox's. None (the default) allows them all,
    ///         [] allows none, and unknown names are ignored. Others exit 127.
    ///         The shell's grep builtin and nested sh/bash count as
    ///         commands; other builtins and callbacks aren't affected.
    ///
    /// Returns:
    ///     ExecuteResult containing stdout, timing info, and statistics.
    ///
    /// Raises:
    ///     ExecutionError: If the Python code raises an exception.
    ///     TimeoutError: If execution exceeds the timeout limit.
    ///     ResourceLimitError: If a resource limit is exceeded.
    ///     CancelledError: If `cancel` was cancelled.
    ///     ValueError: If `allowed_commands` is given for another language.
    ///
    /// Example:
    ///     result = sandbox.execute('''
    ///     x = 2 + 2
    ///     print(f"2 + 2 = {x}")
    ///     ''')
    ///     print(result.stdout)  # "2 + 2 = 4"
    #[pyo3(signature = (code, *, cancel=None, allowed_commands=None))]
    fn execute(
        &self,
        py: Python<'_>,
        code: &str,
        cancel: Option<&CancelToken>,
        allowed_commands: Option<Vec<String>>,
    ) -> PyResult<ExecuteResult> {
        let options = execute_options(&self.inner, allowed_commands)?;
        let token = call_token(cancel);
        let run = run_cancellable(&self.inner, code, options, token.clone());
        run_blocking(py, &self.runtime, &token, run)?
    }

    /// Execute Python code in the sandbox from asyncio.
    ///
    /// Like `execute()`, but returns an awaitable so the event loop keeps
    /// running while the code executes. Cancelling the awaiting task stops
    /// the run.
    ///
    /// Args:
    ///     code: Python source code to execute.
    ///     cancel: Optional CancelToken; cancelling it stops the run.
    ///     allowed_commands: For the shell only: the commands this call may
    ///         run, out of the sandbox's. None (the default) allows them all,
    ///         [] allows none, and unknown names are ignored. Others exit 127.
    ///         The shell's grep builtin and nested sh/bash count as
    ///         commands; other builtins and callbacks aren't affected.
    ///
    /// Returns:
    ///     An awaitable resolving to an ExecuteResult.
    ///
    /// Example:
    ///     result = await sandbox.execute_async('print("hi")')
    #[pyo3(signature = (code, *, cancel=None, allowed_commands=None))]
    fn execute_async<'py>(
        &self,
        py: Python<'py>,
        code: &str,
        cancel: Option<&CancelToken>,
        allowed_commands: Option<Vec<String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let options = execute_options(&self.inner, allowed_commands)?;
        let token = call_token(cancel);
        let run_token = token.clone();
        run_async(py, &self.runtime, token, || {
            run_cancellable(&self.inner, code, options, run_token)
        })
    }

    /// Execute Python code, journaling callback results and replaying them
    /// from `journal` if one was given.
    ///
    /// Unlike `execute()`, this never raises for execution failures: the error
    /// is returned on the outcome alongside the journal, which is always
    /// populated so a later run can replay every callback that completed.
    ///
    /// Args:
    ///     code: Python source code to execute.
    ///     journal: Optional journal (from a previous `ReplayOutcome.journal`)
    ///         whose callback results are replayed instead of invoking callbacks
    ///         live. Trusted input: replayed results are returned to the sandbox
    ///         verbatim.
    ///     allowed_commands: For the shell only: the commands this call may
    ///         run, out of the sandbox's. None (the default) allows them all,
    ///         [] allows none, and unknown names are ignored. Others exit 127.
    ///         The shell's grep builtin and nested sh/bash count as
    ///         commands; other builtins and callbacks aren't affected.
    ///
    /// Returns:
    ///     ReplayOutcome with the result or error, the recorded journal, the
    ///     number of replayed callbacks, and any suspension details.
    ///
    /// Example:
    ///     outcome = sandbox.execute_with_journal(code)
    ///     if outcome.suspended:
    ///         save(outcome.journal)  # resume later with execute_with_journal(code, journal=...)
    ///     elif outcome.error:
    ///         raise outcome.error
    #[pyo3(signature = (code, journal=None, *, allowed_commands=None))]
    fn execute_with_journal(
        &self,
        py: Python<'_>,
        code: &str,
        journal: Option<&Bound<'_, PyAny>>,
        allowed_commands: Option<Vec<String>>,
    ) -> PyResult<ReplayOutcome> {
        let options = execute_options(&self.inner, allowed_commands)?;
        let journal = crate::result::journal_from_py(journal)?;
        let code = code.to_string();
        let runtime = self.runtime.clone();
        let inner = &self.inner;
        let outcome =
            py.detach(|| runtime.block_on(inner.execute_journaled(&code, journal, &options)));
        ReplayOutcome::from_outcome(py, outcome)
    }

    fn __repr__(&self) -> String {
        format!(
            "Sandbox(resource_limits={:?})",
            self.inner.resource_limits()
        )
    }
}

// Static assertions that Sandbox is Send + Sync (required for PyO3 thread safety)
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Sandbox>();
};

impl Sandbox {
    /// Create a Sandbox from an existing eryx::Sandbox, sharing the caller's
    /// Tokio runtime. Used by `SandboxFactory` so every child reuses one
    /// runtime instead of creating its own.
    pub(crate) fn from_factory(
        inner: eryx::Sandbox,
        runtime: Arc<tokio::runtime::Runtime>,
        factory_files: crate::preinit::FactoryFiles,
    ) -> Self {
        Self {
            inner,
            runtime,
            _factory_files: Some(factory_files),
        }
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("resource_limits", self.inner.resource_limits())
            .finish_non_exhaustive()
    }
}

/// A call's [`eryx::ExecuteOptions`], checking `allowed_commands`, which
/// only the shell takes.
pub(crate) fn execute_options(
    sandbox: &eryx::Sandbox,
    allowed_commands: Option<Vec<String>>,
) -> PyResult<eryx::ExecuteOptions> {
    let mut options = eryx::ExecuteOptions::new();
    if let Some(names) = allowed_commands {
        let language = sandbox.executor().language();
        if language != eryx::Language::Shell {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "allowed_commands= is only for language=\"shell\", not {language}"
            )));
        }
        options = options.with_allowed_commands(names);
    }
    Ok(options)
}

/// A future for running `code` on (a cheap clone of) `sandbox` with
/// `options`, stopped when `token` is cancelled.
pub(crate) fn run_cancellable(
    sandbox: &eryx::Sandbox,
    code: &str,
    options: eryx::ExecuteOptions,
    token: CancellationToken,
) -> impl Future<Output = PyResult<ExecuteResult>> + Send + 'static {
    let sandbox = sandbox.clone();
    let code = code.to_string();
    let options = options.with_cancellation(token);
    async move {
        sandbox
            .execute_with(&code, &options)
            .await
            .map(ExecuteResult::from)
            .map_err(eryx_error_to_py)
    }
}

/// Parse a Python dict of secrets and apply them to the sandbox builder.
///
/// Expected format: `{"NAME": {"value": "secret", "allowed_hosts": ["host.com"]}}`
/// The `allowed_hosts` key is optional and defaults to an empty list.
/// The builder a sandbox or session starts from: Python's embedded runtime,
/// or a precompiled runtime from [`find_runtime`] for the other languages,
/// which the pyeryx wheel doesn't embed.
pub(crate) fn base_builder(
    py: Python<'_>,
    language: eryx::Language,
    runtime_path: Option<PathBuf>,
) -> PyResult<(
    eryx::SandboxBuilder<eryx::language::Python, eryx::state::Has>,
    RuntimeSource,
)> {
    if language == eryx::Language::Python && runtime_path.is_none() {
        return Ok((eryx::Sandbox::embedded(), RuntimeSource::Embedded));
    }
    let (path, source) = find_runtime(py, language, runtime_path)?;
    // SAFETY: the runtime comes from the caller, its environment, an installed
    // pyeryx-runtime package or the user's private eryx cache, all trusted
    // like the pyeryx wheel itself. Its eryx version and language stamps are
    // checked when the sandbox is built.
    #[allow(unsafe_code)]
    let builder = unsafe { eryx::Sandbox::builder().with_precompiled_file(path) };
    Ok((builder, source))
}

/// Where a sandbox's runtime came from, which is where its shell finds
/// coreutils.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeSource {
    /// Built into the pyeryx wheel (Python).
    Embedded,
    /// `runtime_path=` or `ERYX_RUNTIME_CWASM_<LANGUAGE>`.
    Explicit,
    /// The `pyeryx-runtime-<language>` companion package.
    Companion,
    /// The cache `eryx-precompile setup` writes.
    SetupCache,
}

/// Finish `builder` (configured as Python, whose language-neutral options
/// carry over) as a sandbox for `language`. A shell gets uutils coreutils
/// from the `pyeryx-runtime-shell` package if it's installed, then
/// `commands`, which replace coreutils tools of the same name.
pub(crate) fn build_for(
    py: Python<'_>,
    (builder, source): (
        eryx::SandboxBuilder<eryx::language::Python, eryx::state::Has>,
        RuntimeSource,
    ),
    language: eryx::Language,
    commands: Option<crate::command::Commands<'_>>,
    child_network: eryx::ChildNetwork,
) -> PyResult<eryx::Sandbox> {
    if language != eryx::Language::Shell && commands.as_ref().is_some_and(|c| !c.is_empty()) {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "commands= is only for language=\"shell\", not {language}"
        )));
    }
    match language {
        eryx::Language::Python => builder.build(),
        eryx::Language::JavaScript => builder.javascript().build(),
        eryx::Language::Shell => {
            let mut builder = builder.shell();
            if let Some(coreutils) = crate::command::coreutils(py, source)? {
                builder = builder.with_commands(eryx::embedded::COREUTILS_TOOLS, coreutils);
            }
            for (name, command) in commands.into_iter().flatten() {
                builder = builder.with_command(name, command.inner.clone());
            }
            builder.with_child_network(child_network).build()
        }
        other => {
            return Err(InitializationError::new_err(format!(
                "unsupported language {other}"
            )));
        }
    }
    .map_err(eryx_error_to_py)
}

/// Find the precompiled runtime for `language`: `explicit`, then
/// `ERYX_RUNTIME_CWASM_<LANGUAGE>`, then the `pyeryx_runtime_<language>`
/// companion package, then the cache `eryx-precompile setup` writes.
pub(crate) fn find_runtime(
    py: Python<'_>,
    language: eryx::Language,
    explicit: Option<PathBuf>,
) -> PyResult<(PathBuf, RuntimeSource)> {
    if let Some(path) = explicit {
        return Ok((path, RuntimeSource::Explicit));
    }
    let var = format!(
        "ERYX_RUNTIME_CWASM_{}",
        language.as_str().to_ascii_uppercase()
    );
    if let Some(path) = std::env::var_os(&var) {
        return Ok((path.into(), RuntimeSource::Explicit));
    }
    // A companion package without its runtime (a source checkout's
    // skeleton) counts as not installed.
    let module = format!("pyeryx_runtime_{language}");
    match py
        .import(module.as_str())
        .and_then(|module| module.call_method0("cwasm_path"))
    {
        Ok(path) => return Ok((path.extract()?, RuntimeSource::Companion)),
        Err(e)
            if e.is_instance_of::<pyo3::exceptions::PyModuleNotFoundError>(py)
                || e.is_instance_of::<pyo3::exceptions::PyFileNotFoundError>(py) => {}
        Err(e) => return Err(e),
    }
    if let Some(path) = eryx::cache::setup_runtime(language) {
        return Ok((path, RuntimeSource::SetupCache));
    }
    Err(InitializationError::new_err(format!(
        "no {language} runtime found: pyeryx doesn't include it. Install it with \
         `pip install pyeryx[{language}]`, or run `eryx-precompile setup --language \
         {language}`, set {var}, or pass runtime_path="
    )))
}

/// `child_network=`: None, "inherit" or a narrowing `NetConfig`, for a
/// shell sandbox with `network=`.
pub(crate) fn parse_child_network(
    language: eryx::Language,
    has_network: bool,
    value: Option<Bound<'_, PyAny>>,
) -> PyResult<eryx::ChildNetwork> {
    let Some(value) = value.filter(|v| !v.is_none()) else {
        return Ok(eryx::ChildNetwork::None);
    };
    if language != eryx::Language::Shell {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "child_network= is only for language=\"shell\", not {language}"
        )));
    }
    if !has_network {
        return Err(pyo3::exceptions::PyValueError::new_err(
            "child_network= needs network=: children only get what it allows",
        ));
    }
    if let Ok(narrow) = value.extract::<NetConfig>() {
        return Ok(eryx::ChildNetwork::Narrow(Box::new(narrow.into())));
    }
    match value.extract::<String>() {
        Ok(mode) if mode == "inherit" => Ok(eryx::ChildNetwork::Inherit),
        Ok(mode) => Err(pyo3::exceptions::PyValueError::new_err(format!(
            "child_network= must be None, \"inherit\" or a NetConfig, not {mode:?}"
        ))),
        Err(_) => Err(pyo3::exceptions::PyTypeError::new_err(
            "child_network= must be None, \"inherit\" or a NetConfig",
        )),
    }
}

/// Parse the `language` argument; `None` is Python.
pub(crate) fn parse_language(language: Option<&str>) -> PyResult<eryx::Language> {
    language.map_or(Ok(eryx::Language::Python), |name| {
        name.parse()
            .map_err(pyo3::exceptions::PyValueError::new_err)
    })
}

pub(crate) fn apply_secrets<L: eryx::language::GuestLanguage, R>(
    mut builder: eryx::SandboxBuilder<L, R>,
    secrets_dict: &Bound<'_, PyDict>,
) -> PyResult<eryx::SandboxBuilder<L, R>> {
    for (name, value, allowed_hosts) in extract_secrets(secrets_dict)? {
        builder = builder.with_secret(name, value, allowed_hosts);
    }
    Ok(builder)
}

/// Parse a Python secrets dict into `(name, value, allowed_hosts)` triples.
pub(crate) fn extract_secrets(
    secrets_dict: &Bound<'_, PyDict>,
) -> PyResult<Vec<(String, String, Vec<String>)>> {
    let mut secrets = Vec::with_capacity(secrets_dict.len());
    for (key, value) in secrets_dict.iter() {
        let name: String = key
            .extract()
            .map_err(|_| pyo3::exceptions::PyTypeError::new_err("secret names must be strings"))?;

        let value_dict: Bound<'_, PyDict> = value.cast_into().map_err(|_| {
            pyo3::exceptions::PyTypeError::new_err(format!(
                "secret '{name}' value must be a dict with a 'value' key"
            ))
        })?;

        let secret_value: String = value_dict
            .get_item("value")?
            .ok_or_else(|| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "secret '{name}' dict must contain a 'value' key"
                ))
            })?
            .extract()
            .map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(format!(
                    "secret '{name}' value must be a string"
                ))
            })?;

        let allowed_hosts: Vec<String> = value_dict
            .get_item("allowed_hosts")?
            .map(|v| v.extract())
            .transpose()
            .map_err(|_| {
                pyo3::exceptions::PyTypeError::new_err(format!(
                    "secret '{name}' allowed_hosts must be a list of strings"
                ))
            })?
            .unwrap_or_default();

        secrets.push((name, secret_value, allowed_hosts));
    }
    Ok(secrets)
}
