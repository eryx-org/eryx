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

    /// Whether this is a component error caused by a wasm trap.
    pub(crate) fn is_wasm_trap(&self) -> bool {
        match &self.kind {
            ErrorKind::WasmComponent(source) => source
                .downcast_ref::<WasmtimeSource>()
                .is_some_and(|source| source.0.is::<wasmtime::Trap>()),
            _ => false,
        }
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
            network_bytes_sent: 0,
            network_bytes_received: 0,
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
error_from!(CallbackError, std::io::Error, serde_json::Error);

/// A wasmtime error carried by [`ErrorKind::WasmComponent`], so the crate can
/// still look at it (e.g. for a trap) while the public type stays opaque.
#[derive(Debug)]
pub(crate) struct WasmtimeSource(pub(crate) wasmtime::Error);

impl std::fmt::Display for WasmtimeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for WasmtimeSource {}

impl ErrorKind {
    /// A [`WasmComponent`](Self::WasmComponent) error from wasmtime.
    pub(crate) fn wasm_component(err: wasmtime::Error) -> Self {
        Self::WasmComponent(Box::new(WasmtimeSource(err)))
    }
}

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
    ///
    /// The source is boxed so the underlying engine's error type is not part
    /// of this crate's API.
    #[error("wasm component error: {0}")]
    WasmComponent(#[source] Box<dyn std::error::Error + Send + Sync>),

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

    /// A command imports interfaces children aren't given, so it can't be
    /// registered. [`Command::inspect`](crate::Command::inspect) lists all of
    /// a command's imports.
    #[error("{}", crate::process::missing_message(missing))]
    #[non_exhaustive]
    CommandImports {
        /// Each import children don't get, with why.
        missing: Vec<crate::CommandImport>,
    },

    /// A command file's sha256 isn't the pinned one
    /// ([`Command::from_file_expecting`](crate::Command::from_file_expecting),
    /// [`Command::load_cached`](crate::Command::load_cached)).
    #[error("command digest mismatch: expected {expected}, got {actual}")]
    #[non_exhaustive]
    CommandDigest {
        /// The pinned digest, `sha256:<hex>`.
        expected: String,
        /// The file's digest, or why `expected` isn't a sha256 digest.
        actual: String,
    },

    /// Error during execution machinery (e.g. a WASM trap, the store or
    /// bindings being unavailable, or invalid input). This is a *sandbox* failure
    /// — the runtime could not faithfully execute the script. A failure of the
    /// script itself is [`ErrorKind::ScriptFailed`] instead.
    #[error("execution failed: {0}")]
    Execution(String),

    /// The script failed: it raised an uncaught exception (Python,
    /// JavaScript) or exited with a failure status (a shell). This is a
    /// *script* failure, not a sandbox failure: the runtime ran the code
    /// faithfully and the code itself errored.
    ///
    /// The output the script wrote before it failed is kept in `stdout` and
    /// `stderr` (also via [`Error::stdout`] and [`Error::stderr`]); `stderr`
    /// includes the traceback, as for a successful run.
    #[error("{message}")]
    #[non_exhaustive]
    ScriptFailed {
        /// What the guest reported: for Python, the traceback exactly as
        /// CPython would have written it to stderr.
        message: String,
        /// The script's exit status, for guests that have one (a shell).
        /// `None` for Python and JavaScript, whose scripts fail by raising.
        exit_code: Option<i32>,
        /// Raw stdout captured before the failure.
        stdout: Vec<u8>,
        /// Raw stderr captured up to the failure, including any traceback.
        stderr: Vec<u8>,
    },

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
    #[non_exhaustive]
    FuelExhausted {
        /// Number of instructions consumed before exhaustion.
        consumed: u64,
        /// The fuel limit that was set.
        limit: u64,
    },

    /// Serialization/deserialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// The language's stdlib was not found during auto-detection.
    ///
    /// Only Python has a separate stdlib. Use
    /// [`SandboxBuilder::with_python_stdlib()`](crate::SandboxBuilder::with_python_stdlib)
    /// to specify the stdlib path explicitly, or enable the `embedded` feature and use
    /// [`Sandbox::embedded()`](crate::Sandbox::embedded).
    #[error("{language} stdlib not found{}", missing_stdlib_hint(*language))]
    #[non_exhaustive]
    MissingStdlib {
        /// The language whose stdlib is missing.
        language: crate::Language,
    },

    /// No runtime was supplied for the selected language, and this build
    /// embeds none for it (a language's runtime is embedded with the
    /// `embedded` feature and that language's feature).
    ///
    /// Supply one with [`SandboxBuilder::with_wasm_file()`](crate::SandboxBuilder::with_wasm_file)
    /// or a precompiled equivalent.
    #[error(
        "no {language} runtime is embedded in this build of eryx; supply one with with_wasm_file() or with_precompiled_file()"
    )]
    #[non_exhaustive]
    MissingRuntime {
        /// The language that has no runtime.
        language: crate::Language,
    },

    /// State snapshot error.
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// A state snapshot was written by a newer eryx, in a format this one
    /// can't read.
    #[error(
        "this snapshot uses format version {format_version}, which eryx {} can't read; \
         it was written by a newer eryx",
        env!("CARGO_PKG_VERSION")
    )]
    #[non_exhaustive]
    IncompatibleSnapshot {
        /// The format version stored in the snapshot.
        format_version: u8,
    },

    /// The guest can't read a state snapshot's data: its guest-specific state
    /// format (for the shell, the brush version that serialized it) isn't the
    /// one this runtime reads.
    #[error("this snapshot's state is in format {snapshot:?}, but this runtime reads {runtime:?}")]
    #[non_exhaustive]
    IncompatibleStateFormat {
        /// The format the snapshot's state is in (empty if it has none).
        snapshot: String,
        /// The format this runtime reads.
        runtime: String,
    },

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

/// How to supply `language`'s stdlib, for [`ErrorKind::MissingStdlib`].
fn missing_stdlib_hint(language: crate::Language) -> &'static str {
    match language {
        crate::Language::Python => {
            ". Set ERYX_STDLIB, use with_python_stdlib(), or use Sandbox::embedded()"
        }
        // Only Python has a stdlib apart from its runtime, so only Python
        // raises this; the Python hints would mislead for anything else.
        _ => "",
    }
}

impl Error {
    /// The stdout the script wrote before it failed, or `None` if the guest
    /// did not get as far as running it (and for a timeout, fuel limit or
    /// cancellation, which stop the guest before it can report its output).
    #[must_use]
    pub fn stdout(&self) -> Option<&[u8]> {
        match &self.kind {
            ErrorKind::ScriptFailed { stdout, .. } => Some(stdout),
            _ => None,
        }
    }

    /// The stderr captured up to the failure, including any traceback;
    /// `None` in the same cases as [`Error::stdout`].
    #[must_use]
    pub fn stderr(&self) -> Option<&[u8]> {
        match &self.kind {
            ErrorKind::ScriptFailed { stderr, .. } => Some(stderr),
            _ => None,
        }
    }
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
/// ([`ErrorKind::Execution`]) rather than being mistaken for a script failure
/// ([`ErrorKind::ScriptFailed`]).
pub(crate) fn validate_user_code(code: &str) -> Result<(), Error> {
    if code.contains('\0') {
        return Err(ErrorKind::Execution(
            "code contains NUL bytes, which cannot be executed".to_string(),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_stdlib_hints_are_per_language() {
        let python = Error::from(ErrorKind::MissingStdlib {
            language: crate::Language::Python,
        });
        assert!(python.to_string().contains("with_python_stdlib()"));
        let shell = Error::from(ErrorKind::MissingStdlib {
            language: crate::Language::Shell,
        });
        assert_eq!(shell.to_string(), "shell stdlib not found");
    }
}
