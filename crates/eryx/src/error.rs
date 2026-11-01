//! Error types for the Eryx sandbox.

use crate::callback::CallbackError;
use crate::sandbox::ExecuteStats;
use std::time::Duration;

/// The main error type for Eryx operations.
///
/// An `Error` says what went wrong ([`kind`](Self::kind)) and, when the guest
/// had started running, what the run used up to the point it failed
/// ([`stats`](Self::stats)). The statistics matter for metering: a run that
/// times out, raises or is cancelled has still spent its fuel.
///
/// ```
/// # use eryx::{Error, ErrorKind};
/// # fn classify(err: &Error) {
/// match err.kind() {
///     ErrorKind::Timeout(_) | ErrorKind::Cancelled => {
///         let fuel = err.stats().and_then(|s| s.fuel_consumed);
///         println!("stopped after {fuel:?} instructions");
///     }
///     other => println!("failed: {other}"),
/// }
/// # }
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub struct Error {
    kind: ErrorKind,
    stats: Option<Box<ExecuteStats>>,
    output_truncated: bool,
}

impl Error {
    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    pub(crate) fn kind_mut(&mut self) -> &mut ErrorKind {
        &mut self.kind
    }

    /// Consume the error, keeping only its [`ErrorKind`].
    #[must_use]
    pub fn into_kind(self) -> ErrorKind {
        self.kind
    }

    /// What the run used before it failed.
    ///
    /// `None` when the failure happened before the guest ran (invalid code,
    /// initialization, a token cancelled up front) or when the run was
    /// suspended: a suspension halts the guest by poisoning its fuel, so the
    /// figures would be meaningless. `fuel_consumed` is always set otherwise.
    /// For a run stopped by a timeout or a cancellation it is accurate to
    /// within a few million instructions, not to the instruction.
    #[must_use]
    pub fn stats(&self) -> Option<&ExecuteStats> {
        self.stats.as_deref()
    }

    /// Whether the run dropped streamed stdout/stderr chunks for exceeding the
    /// per-execution stream budget before it failed. Unlike
    /// [`ExecuteResult::stream_truncated`](crate::ExecuteResult::stream_truncated)
    /// it ignores dropped trace events.
    #[must_use]
    pub fn output_truncated(&self) -> bool {
        self.output_truncated
    }

    /// Record that the failed run dropped streamed output.
    #[must_use]
    pub(crate) fn with_output_truncated(mut self, truncated: bool) -> Self {
        self.output_truncated = truncated;
        self
    }

    /// Attach the statistics of the run that failed.
    #[must_use]
    pub(crate) fn with_stats(mut self, stats: ExecuteStats) -> Self {
        self.stats = Some(Box::new(stats));
        self
    }

    /// Attach what a run that reached the guest had used when it failed.
    ///
    /// A suspension is left without statistics, as its fuel was poisoned to
    /// halt the guest. `callback_invocations` is 0: it is counted by the
    /// caller's callback handler, which fills it in.
    #[must_use]
    pub(crate) fn with_run_stats(
        self,
        duration: Duration,
        peak_memory_bytes: u64,
        fuel_consumed: Option<u64>,
    ) -> Self {
        if matches!(self.kind, ErrorKind::Suspended(_)) {
            return self;
        }
        self.with_stats(ExecuteStats {
            duration,
            callback_invocations: 0,
            peak_memory_bytes: Some(peak_memory_bytes),
            fuel_consumed,
        })
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(f)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(&self.kind)
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Self {
            kind,
            stats: None,
            output_truncated: false,
        }
    }
}

/// Let `?` convert the source errors `ErrorKind` wraps straight into `Error`.
macro_rules! error_from {
    ($($source:ty),+) => {$(
        impl From<$source> for Error {
            fn from(err: $source) -> Self {
                ErrorKind::from(err).into()
            }
        }
    )+};
}
error_from!(
    wasmtime::Error,
    CallbackError,
    std::io::Error,
    serde_json::Error
);

/// What went wrong, as a matchable category. See [`Error::kind`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ErrorKind {
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
    /// exception raised *by* the script is [`ErrorKind::PythonException`] instead.
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

impl From<serde_json::Error> for ErrorKind {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

/// Reject user code the guest cannot run, before it reaches the WASM boundary.
///
/// Currently this rejects embedded NUL bytes: the code is handed to the guest
/// as a C string, so a NUL would be caught there and surface as an opaque guest
/// error. Validating host-side keeps it classified as a sandbox/input failure
/// ([`ErrorKind::Execution`]) rather than being mistaken for an uncaught Python
/// exception ([`ErrorKind::PythonException`]).
pub(crate) fn validate_user_code(code: &str) -> Result<(), Error> {
    if code.contains('\0') {
        return Err(ErrorKind::Execution(
            "code contains NUL bytes, which cannot be executed".to_string(),
        )
        .into());
    }
    Ok(())
}
