//! Error types for the Eryx sandbox.

use crate::callback::CallbackError;

/// The main error type for Eryx operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Error during sandbox initialization.
    #[error("initialization failed: {0}")]
    Initialization(String),

    /// Error during WASM engine creation.
    #[error("wasm engine error: {0}")]
    WasmEngine(String),

    /// The process-wide epoch ticker could not be started or restarted, or an
    /// existing ticker worker terminated unexpectedly.
    ///
    /// This is an execution-time prerequisite for timeouts and cancellation,
    /// rather than a failure to initialize the sandbox itself.
    #[error("epoch ticker error: {0}")]
    EpochTicker(String),

    /// Error during WASM component loading or instantiation.
    #[error("wasm component error: {0}")]
    WasmComponent(#[from] wasmtime::Error),

    /// A precompiled runtime (a `.cwasm` file, embedded runtime or saved
    /// factory) was built by a different eryx version.
    ///
    /// Precompiled files only load in the eryx version that built them, so
    /// they have to be rebuilt after every eryx upgrade: with the matching
    /// `eryx-precompile`, or by re-saving the factory.
    #[error(
        "this precompiled runtime was built by {}, but this is {expected}. \
         Precompiled runtimes and saved factories only load in the eryx version \
         that built them: rebuild it with eryx-precompile {}, or re-save the factory",
        describe_built_by(built_by),
        env!("CARGO_PKG_VERSION")
    )]
    #[non_exhaustive]
    IncompatiblePrecompiled {
        /// The version string stored in the file, e.g. `eryx 0.13.0 (wasmtime 50)`.
        /// Files from eryx releases before 0.13.0 store only wasmtime's major
        /// version.
        built_by: String,
        /// The version string this eryx build expects.
        expected: String,
    },

    /// Error during Python execution machinery (e.g. a WASM trap, the store or
    /// bindings being unavailable, or invalid input). This is a *sandbox* failure
    /// — the runtime could not faithfully execute the script. An uncaught
    /// exception raised *by* the script is [`Error::PythonException`] instead.
    #[error("execution failed: {0}")]
    Execution(String),

    /// The script raised an uncaught Python exception. The contained string is
    /// the traceback exactly as CPython would have written it to stderr. This is
    /// a *script* failure, not a sandbox failure: the runtime ran the code
    /// faithfully and the code itself errored.
    #[error("{0}")]
    PythonException(String),

    /// A callback error occurred during execution.
    #[error("callback error: {0}")]
    Callback(#[from] CallbackError),

    /// Resource limit exceeded.
    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),

    /// Execution timed out.
    #[error("execution timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Execution ran out of fuel (instruction limit exceeded).
    #[error("execution ran out of fuel after {consumed} instructions (limit: {limit})")]
    FuelExhausted {
        /// Number of instructions consumed before exhaustion.
        consumed: u64,
        /// The fuel limit that was set.
        limit: u64,
    },

    /// Serialization/deserialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Python stdlib not found during auto-detection.
    ///
    /// Use [`SandboxBuilder::with_python_stdlib()`](crate::SandboxBuilder::with_python_stdlib)
    /// to specify the stdlib path explicitly, or enable the `embedded` feature and use
    /// [`Sandbox::embedded()`](crate::Sandbox::embedded).
    #[error(
        "Python stdlib not found. Set ERYX_PYTHON_STDLIB, use with_python_stdlib(), or use Sandbox::embedded()"
    )]
    MissingPythonStdlib,

    /// State snapshot error.
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// Execution was cancelled.
    #[error("execution cancelled")]
    Cancelled,

    /// Execution was suspended by a callback returning
    /// [`CallbackError::Suspend`](crate::CallbackError::Suspend).
    ///
    /// The guest was halted (its fuel poisoned) the instant the callback
    /// suspended, so no normal output is produced. This is distinct from
    /// [`FuelExhausted`](Self::FuelExhausted), which signals a real fuel-limit
    /// overrun. The opaque reason and the suspended callback's metadata are
    /// surfaced separately (e.g. via
    /// [`ReplayOutcome::suspended`](crate::ReplayOutcome::suspended)).
    #[error("execution suspended: {0}")]
    Suspended(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Name the eryx version recorded in a precompiled file.
///
/// Since 0.13.0, eryx stores its own version in the file. Earlier releases
/// stored wasmtime's major version alone, e.g. `50`.
fn describe_built_by(built_by: &str) -> String {
    if built_by.starts_with("eryx ") {
        built_by.to_owned()
    } else {
        format!("an eryx release before 0.13.0 (wasmtime {built_by})")
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

/// Reject user code the guest cannot run, before it reaches the WASM boundary.
///
/// Currently this rejects embedded NUL bytes: the code is handed to the guest
/// as a C string, so a NUL would be caught there and surface as an opaque guest
/// error. Validating host-side keeps it classified as a sandbox/input failure
/// ([`Error::Execution`]) rather than being mistaken for an uncaught Python
/// exception ([`Error::PythonException`]).
pub(crate) fn validate_user_code(code: &str) -> Result<(), Error> {
    if code.contains('\0') {
        return Err(Error::Execution(
            "code contains NUL bytes, which cannot be executed".to_string(),
        ));
    }
    Ok(())
}
