//! Driving executions from Python: cancellation, Ctrl-C and asyncio.
//!
//! Every execution runs as a task on the shared Tokio runtime. Blocking calls
//! wait for it in short slices with the GIL released, checking for signals in
//! between; `*_async` calls hand it to asyncio as an awaitable.

use std::cell::OnceCell;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use eryx::CancellationToken;
use pyo3::prelude::*;
use pyo3_async_runtimes::TaskLocals;
use pyo3_async_runtimes::generic::{ContextExt, Runtime};

use crate::error::ExecutionError;

/// How often a blocking execution checks for signals such as Ctrl-C.
const SIGNAL_CHECK_INTERVAL: Duration = Duration::from_millis(50);

/// A token for cancelling executions from any thread.
///
/// Pass it as ``cancel=`` to ``execute()`` or ``execute_async()``; calling
/// ``cancel()`` stops the run, which then raises ``CancelledError``. A token
/// stays cancelled, so later executions given the same token are cancelled
/// before they start. One token may be shared by several executions.
///
/// Example:
///     token = CancelToken()
///     threading.Timer(1.0, token.cancel).start()
///     try:
///         sandbox.execute("while True: pass", cancel=token)
///     except CancelledError:
///         print("stopped")
#[pyclass(module = "eryx", frozen)]
#[derive(Debug, Default)]
pub struct CancelToken {
    inner: CancellationToken,
}

#[pymethods]
impl CancelToken {
    #[new]
    fn new() -> Self {
        Self::default()
    }

    /// Cancel every execution using this token. Safe to call from any thread,
    /// and more than once.
    fn cancel(&self) {
        self.inner.cancel();
    }

    /// Whether `cancel()` has been called.
    #[getter]
    fn cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    fn __repr__(&self) -> String {
        format!("CancelToken(cancelled={})", self.cancelled())
    }
}

/// The token for one execution: cancelled by the caller's token, if any, and
/// also by Ctrl-C or the awaiting asyncio task being cancelled.
pub(crate) fn call_token(cancel: Option<&CancelToken>) -> CancellationToken {
    cancel.map_or_else(CancellationToken::new, |token| token.inner.child_token())
}

/// Run `fut` to completion on `runtime` without holding the GIL.
///
/// Signals are checked every [`SIGNAL_CHECK_INTERVAL`]. If a handler raises
/// (Ctrl-C raises `KeyboardInterrupt`), `cancel` is cancelled, the run is
/// waited for so the sandbox is no longer in use, and the handler's exception
/// is raised.
pub(crate) fn run_blocking<T: Send + 'static>(
    py: Python<'_>,
    runtime: &tokio::runtime::Runtime,
    cancel: &CancellationToken,
    fut: impl Future<Output = T> + Send + 'static,
) -> PyResult<T> {
    let mut task = runtime.spawn(fut);
    loop {
        let slice = py.detach(|| {
            runtime.block_on(async { tokio::time::timeout(SIGNAL_CHECK_INTERVAL, &mut task).await })
        });
        match slice {
            Ok(joined) => return joined.map_err(task_failed),
            Err(_elapsed) => {
                if let Err(signal) = py.check_signals() {
                    cancel.cancel();
                    // The outcome is moot: the signal's exception wins.
                    let _ = py.detach(|| runtime.block_on(task));
                    return Err(signal);
                }
            }
        }
    }
}

/// Cancels its token when dropped.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Start the future `start` returns on `runtime` and return an asyncio
/// awaitable for its result.
///
/// `start` is only called once the running event loop is known, so calling
/// this outside one raises without starting anything. Cancelling the
/// awaiting asyncio task cancels `cancel`, which stops the run.
pub(crate) fn run_async<'a, T, F>(
    py: Python<'a>,
    runtime: &tokio::runtime::Runtime,
    cancel: CancellationToken,
    start: impl FnOnce() -> F,
) -> PyResult<Bound<'a, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    let locals = pyo3_async_runtimes::generic::get_current_locals::<EryxRuntime>(py)?;
    let task = runtime.spawn(start());
    pyo3_async_runtimes::generic::future_into_py_with_locals::<EryxRuntime, _, T>(
        py,
        locals,
        async move {
            // Dropped without completing when the asyncio task is cancelled.
            let _cancel_on_drop = CancelOnDrop(cancel);
            task.await.map_err(task_failed)?
        },
    )
}

fn task_failed(error: tokio::task::JoinError) -> PyErr {
    ExecutionError::new_err(format!("execution task failed: {error}"))
}

/// Lets `pyo3-async-runtimes` resolve awaitables on the shared eryx runtime
/// instead of starting a runtime of its own.
struct EryxRuntime;

tokio::task_local! {
    static TASK_LOCALS: OnceCell<TaskLocals>;
}

impl Runtime for EryxRuntime {
    type JoinError = tokio::task::JoinError;
    type JoinHandle = tokio::task::JoinHandle<()>;

    fn spawn<F>(fut: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        shared_runtime().spawn(fut)
    }

    fn spawn_blocking<F>(f: F) -> Self::JoinHandle
    where
        F: FnOnce() + Send + 'static,
    {
        shared_runtime().spawn_blocking(f)
    }
}

impl ContextExt for EryxRuntime {
    fn scope<F, R>(locals: TaskLocals, fut: F) -> Pin<Box<dyn Future<Output = R> + Send>>
    where
        F: Future<Output = R> + Send + 'static,
    {
        let cell = OnceCell::new();
        let _ = cell.set(locals);
        Box::pin(TASK_LOCALS.scope(cell, fut))
    }

    fn get_task_locals() -> Option<TaskLocals> {
        TASK_LOCALS
            .try_with(|cell| cell.get().cloned())
            .unwrap_or_default()
    }
}

#[expect(
    clippy::expect_used,
    reason = "the runtime trait can't return an error; make_runtime only fails if the OS can't start a runtime's threads"
)]
fn shared_runtime() -> std::sync::Arc<tokio::runtime::Runtime> {
    crate::error::make_runtime().expect("failed to start the eryx runtime")
}
