//! External commands for the shell guest: `eryx:process`.
//!
//! A [`Command`] is a WASI 0.2 command component (one that exports
//! `wasi:cli/run`), compiled once on the shared engine. A shell sandbox
//! registers commands by name
//! ([`SandboxBuilder::with_command`](crate::SandboxBuilder::with_command)), and
//! the shell spawns them as child processes through the `eryx:process`
//! interface. Each spawn instantiates from the command's cached
//! [`CommandPre`], so children get the engine's pooling allocator, fuel
//! metering and epoch interruption without recompiling.
//!
//! A child's stdin, stdout and stderr are component-model streams, piped
//! through small host buffers to its WASI stdio, so pipeline stages run
//! concurrently and a full pipe makes its writer wait. The child runs while
//! the guest awaits its `wait`.
//!
//! What a child can reach:
//! - **Files:** with `unstable-vfs`, a [fork](eryx_vfs::HybridVfsCtx::fork) of
//!   the parent's hybrid VFS, so the same mounts and permissions and nothing
//!   else. Without it, no filesystem.
//! - **Network:** none. Children get no sockets; the proxy is #812.
//! - **Limits:** children burn the parent execution's fuel as they run (at
//!   most `CHILD_FUEL` each), overshooting it by at most one epoch tick of
//!   work per running child, and stop at its timeout and cancellation. The
//!   running children share one `CHILD_MEMORY_LIMIT` of memory, on top of
//!   the parent's own limit, and the host memory their arguments hold is
//!   bounded by the execution's `ChildBudget`.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use wasmtime::component::{
    Access, Accessor, Component, Destination, HasSelf, Linker, Resource, ResourceTable, Source,
    StreamConsumer, StreamProducer, StreamReader, StreamResult, VecBuffer,
};
use wasmtime::{
    AsContext, AsContextMut, Engine, ResourceLimiter, Store, StoreContextMut, UpdateDeadline,
};
use wasmtime_wasi::cli::{AsyncStdinStream, AsyncStdoutStream};
use wasmtime_wasi::p2::bindings::CommandPre;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::error::{Error, ErrorKind};
use crate::wasm::ExecutorState;

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "eryx:process/spawner",
        // `spawn` needs the parent's store to pipe the streams, and `wait`
        // to charge it the child's fuel.
        imports: {
            "eryx:process/process.[static]child.spawn": store,
            default: async,
        },
        with: { "eryx:process/process.child": super::ChildProcess },
    });
}

use bindings::eryx::process::process::{
    Host, HostChild, HostChildWithStore, ProcessError, Spawned,
};

/// Bytes each of a child's stdin, stdout and stderr pipes holds before its
/// writer has to wait for the reader.
const PIPE_CAPACITY: usize = 64 * 1024;
/// Most fuel one child may burn, taken from the parent's.
///
/// A running child is charged to the parent each time it yields, once per
/// epoch tick, so children together overshoot the parent's fuel by at most
/// one tick of work each, for up to [`MAX_LIVE_CHILDREN`] of them.
const CHILD_FUEL: u64 = 10_000_000_000;
/// Memory an execution's running children may grow to together, on top of
/// the parent's own limit.
const CHILD_MEMORY_LIMIT: usize = 256 * 1024 * 1024;
/// Children spawned and not yet finished, at most, per execution.
const MAX_LIVE_CHILDREN: usize = 64;
/// Bytes of argv and env one spawn may pass, at most (like `ARG_MAX`).
const MAX_ARGS_BYTES: usize = 2 * 1024 * 1024;
/// Default for [`ResourceLimits::max_child_host_bytes`](crate::ResourceLimits::max_child_host_bytes).
pub(crate) const DEFAULT_CHILD_HOST_BYTES: u64 = 64 * 1024 * 1024;

/// Host memory held for an execution's children's argv and env, outside the
/// guest's memory limit. The shell spawns every pipeline stage before running
/// any, so without this a script could make the host hold many large
/// argument lists at once. (Their stdio pipes are `PIPE_CAPACITY` each.)
#[derive(Debug, Clone)]
pub(crate) struct ChildBudget {
    limit: u64,
    used: u64,
    /// Children spawned and not yet finished (or dropped), out of
    /// [`MAX_LIVE_CHILDREN`].
    live: Arc<AtomicUsize>,
    /// Linear memory the running children hold, out of `CHILD_MEMORY_LIMIT`.
    memory: Arc<AtomicUsize>,
}

impl Default for ChildBudget {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ChildBudget {
    /// A budget of `limit` bytes ([`DEFAULT_CHILD_HOST_BYTES`] for `None`).
    pub(crate) fn new(limit: Option<u64>) -> Self {
        Self {
            limit: limit.unwrap_or(DEFAULT_CHILD_HOST_BYTES),
            used: 0,
            live: Arc::default(),
            memory: Arc::default(),
        }
    }

    /// Take `bytes` from the budget, or fail if that would overdraw it.
    fn charge(&mut self, bytes: u64) -> Result<(), ProcessError> {
        let used = self.used.saturating_add(bytes);
        if used > self.limit {
            tracing::warn!(
                limit = self.limit,
                used = self.used,
                bytes,
                "child host-memory budget exhausted"
            );
            return Err(ProcessError::LimitExceeded);
        }
        self.used = used;
        Ok(())
    }

    fn release(&mut self, bytes: u64) {
        self.used = self.used.saturating_sub(bytes);
    }
}

/// Add the `eryx:process` interface to a sandbox linker.
pub(crate) fn add_to_linker(linker: &mut Linker<ExecutorState>) -> wasmtime::Result<()> {
    bindings::eryx::process::process::add_to_linker::<_, HasSelf<ExecutorState>>(linker, |s| s)
}

/// A WASI 0.2 command component that a shell sandbox can run as an external
/// command.
///
/// Cloning is cheap: clones share the compiled component, so registering one
/// multicall binary under several names (see
/// [`SandboxBuilder::with_commands`](crate::SandboxBuilder::with_commands))
/// compiles it once.
#[derive(Clone)]
pub struct Command {
    pre: CommandPre<ChildState>,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command").finish_non_exhaustive()
    }
}

impl Command {
    /// Compile a command component from a `.wasm` file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file can't be read or compiled, or isn't a
    /// WASI 0.2 command (it must export `wasi:cli/run` and import only WASI).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| {
            ErrorKind::Initialization(format!("failed to read command {}: {e}", path.display()))
        })?;
        Self::from_bytes(&bytes)
    }

    /// Compile a command component from its `.wasm` bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes don't compile, or aren't a WASI 0.2
    /// command.
    pub fn from_bytes(wasm: &[u8]) -> Result<Self, Error> {
        let engine = crate::Executor::shared_engine()?;
        let component = Component::from_binary(&engine, wasm).map_err(ErrorKind::wasm_component)?;
        Self::from_component(&engine, &component)
    }

    /// Load a command precompiled by
    /// [`Executor::precompile`](crate::Executor::precompile) (or
    /// `eryx-precompile compile`, without `--language`).
    ///
    /// # Safety
    ///
    /// This function is unsafe because Wasmtime cannot fully validate
    /// pre-compiled components for safety. Only use this with pre-compiled
    /// bytes you control and trust. Using untrusted bytes can lead to arbitrary
    /// code execution. The same contract as
    /// [`Executor::from_precompiled`](crate::Executor::from_precompiled).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::IncompatiblePrecompiled`] if another eryx version
    /// precompiled it, and an error if it isn't a WASI 0.2 command.
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    pub unsafe fn from_precompiled(precompiled: &[u8]) -> Result<Self, Error> {
        let engine = crate::Executor::shared_engine()?;
        // SAFETY: the caller guarantees the bytes are trusted eryx output.
        let component = unsafe { Component::deserialize(&engine, precompiled) }
            .map_err(crate::wasm::deserialize_error)?;
        Self::from_component(&engine, &component)
    }

    /// Load a precompiled command from a file; see
    /// [`from_precompiled`](Self::from_precompiled).
    ///
    /// # Safety
    ///
    /// This function is unsafe because Wasmtime cannot fully validate
    /// pre-compiled components for safety. Only use this with files you
    /// control and trust. Using untrusted files can lead to arbitrary code
    /// execution. The file is memory-mapped, so it must also not be modified
    /// while the command is loaded.
    ///
    /// # Errors
    ///
    /// As for [`from_precompiled`](Self::from_precompiled).
    #[cfg(any(feature = "embedded", feature = "preinit"))]
    #[allow(unsafe_code)]
    pub unsafe fn from_precompiled_file(path: impl AsRef<Path>) -> Result<Self, Error> {
        let engine = crate::Executor::shared_engine()?;
        // SAFETY: the caller guarantees the file is trusted and unchanging.
        let component = unsafe { Component::deserialize_file(&engine, path) }
            .map_err(crate::wasm::deserialize_error)?;
        Self::from_component(&engine, &component)
    }

    fn from_component(engine: &Engine, component: &Component) -> Result<Self, Error> {
        #[cfg(test)]
        tests::COMPILED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pre = child_linker(engine)?
            .instantiate_pre(component)
            .map_err(ErrorKind::wasm_component)?;
        let pre = CommandPre::new(pre).map_err(ErrorKind::wasm_component)?;
        Ok(Self { pre })
    }
}

/// The commands a sandbox can spawn, by name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Commands(Arc<HashMap<String, Command>>);

impl Commands {
    pub(crate) fn insert(&mut self, name: String, command: Command) {
        Arc::make_mut(&mut self.0).insert(name, command);
    }

    /// Register `command` as `name` unless something already is.
    #[cfg(all(feature = "embedded", feature = "coreutils"))]
    pub(crate) fn insert_if_absent(&mut self, name: &str, command: &Command) {
        if !self.0.contains_key(name) {
            Arc::make_mut(&mut self.0).insert(name.to_owned(), command.clone());
        }
    }
}

/// The linker children are instantiated with: WASI 0.2, plus the hybrid VFS
/// with `unstable-vfs`. Built once for the shared engine.
fn child_linker(engine: &Engine) -> Result<&'static Linker<ChildState>, Error> {
    static LINKER: OnceLock<Linker<ChildState>> = OnceLock::new();
    if let Some(linker) = LINKER.get() {
        return Ok(linker);
    }
    let wasm_err = |e: wasmtime::Error| ErrorKind::WasmEngine(e.to_string());
    let mut linker = Linker::<ChildState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(wasm_err)?;
    #[cfg(feature = "unstable-vfs")]
    {
        linker.allow_shadowing(true);
        eryx_vfs::add_hybrid_vfs_to_linker(&mut linker)
            .map_err(|e| ErrorKind::WasmEngine(e.to_string()))?;
        linker.allow_shadowing(false);
    }
    Ok(LINKER.get_or_init(|| linker))
}

/// A child's store data.
pub(crate) struct ChildState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: ChildMemory,
    #[cfg(feature = "unstable-vfs")]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

/// A child's share of its execution's `CHILD_MEMORY_LIMIT`, given back when
/// its store is dropped.
struct ChildMemory {
    pool: Arc<AtomicUsize>,
    held: usize,
}

impl ResourceLimiter for ChildMemory {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let grow = desired.saturating_sub(current);
        let reserved = self
            .pool
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(grow).filter(|&u| u <= CHILD_MEMORY_LIMIT)
            })
            .is_ok();
        if reserved {
            self.held += grow;
        }
        // A refused grow fails `memory.grow`, so the child sees an ordinary
        // allocation failure.
        Ok(reserved)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

impl Drop for ChildMemory {
    fn drop(&mut self) {
        self.pool.fetch_sub(self.held, Ordering::Relaxed);
    }
}

/// A child's place among the execution's [`MAX_LIVE_CHILDREN`], held from
/// spawn until its run ends or is dropped.
struct LiveSlot(Arc<AtomicUsize>);

impl LiveSlot {
    /// Take a slot, or `None` if all are taken.
    fn take(live: &Arc<AtomicUsize>) -> Option<Self> {
        live.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n < MAX_LIVE_CHILDREN).then_some(n + 1)
        })
        .ok()
        .map(|_| Self(Arc::clone(live)))
    }
}

impl Drop for LiveSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(feature = "unstable-vfs")]
impl eryx_vfs::HybridVfsView for ChildState {
    type Storage = eryx_vfs::ArcStorage;

    fn hybrid_vfs(&mut self) -> eryx_vfs::HybridVfsState<'_, Self::Storage> {
        // A child without the parent's VFS sees an empty one.
        let vfs = self.vfs.get_or_insert_with(|| {
            eryx_vfs::HybridVfsCtx::new(eryx_vfs::ArcStorage::new(Arc::new(
                eryx_vfs::InMemoryStorage::new(),
            )))
        });
        eryx_vfs::HybridVfsState::new(vfs, &mut self.table)
    }
}

impl WasiView for ChildState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// A spawned child, until `wait` runs it.
pub struct ChildProcess {
    /// What `wait` runs, until it does.
    run: Option<ChildRun>,
    /// What this child holds of the execution's [`ChildBudget`].
    charged: u64,
}

/// What `wait` runs a child with, taken out of its handle.
struct ChildRun {
    pre: CommandPre<ChildState>,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    /// The child's ends of its stdio pipes. (Duplex, not simplex: dropping
    /// one end of a simplex pipe's split halves doesn't end the other.)
    stdin: DuplexStream,
    stdout: DuplexStream,
    stderr: DuplexStream,
    /// Fuel the child may burn.
    fuel: u64,
    /// Fuel it has burned so far, as of its last epoch tick.
    fuel_used: Arc<AtomicU64>,
    /// The execution's pool of child memory.
    memory: Arc<AtomicUsize>,
    /// Its live slot, given back when the run ends or is dropped.
    _live: LiveSlot,
    #[cfg(feature = "unstable-vfs")]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

impl ChildRun {
    /// Run the child to completion, returning its exit code and the fuel it
    /// burned.
    async fn run(self) -> (Result<i32, String>, u64) {
        let wasi = WasiCtxBuilder::new()
            .args(&self.argv)
            .envs(&self.env)
            .stdin(AsyncStdinStream::new(self.stdin))
            .stdout(AsyncStdoutStream::new(PIPE_CAPACITY, self.stdout))
            .stderr(AsyncStdoutStream::new(PIPE_CAPACITY, self.stderr))
            .build();
        let mut store = Store::new(
            self.pre.engine(),
            ChildState {
                wasi,
                table: ResourceTable::new(),
                limits: ChildMemory {
                    pool: self.memory,
                    held: 0,
                },
                #[cfg(feature = "unstable-vfs")]
                vfs: self.vfs,
            },
        );
        store.limiter(|s| &mut s.limits);
        // Without this, a loop with no calls keeps its fuel count in a
        // register, and the epoch callback below reads a stale count.
        if let Err(e) = crate::wasm::sync_fuel_periodically(&mut store) {
            return (Err(e.to_string()), 0);
        }
        // Yield on every epoch tick, publishing the fuel burned so far for
        // `wait` to charge the parent. The parent's timeout and cancellation
        // race its whole execution, so yielding also lets them drop a
        // CPU-bound child as they would a guest blocked in `time.sleep`.
        let fuel = self.fuel;
        let fuel_used = self.fuel_used;
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(move |store| {
            let left = store.as_context().get_fuel().unwrap_or(0);
            fuel_used.store(fuel.saturating_sub(left), Ordering::Relaxed);
            Ok(UpdateDeadline::Yield(1))
        });

        let code = async {
            store.set_fuel(self.fuel).map_err(|e| e.to_string())?;
            let command = self
                .pre
                .instantiate_async(&mut store)
                .await
                .map_err(|e| format!("instantiate: {e}"))?;
            match command.wasi_cli_run().call_run(&mut store).await {
                Ok(Ok(())) => Ok(0),
                Ok(Err(())) => Ok(1),
                Err(e) => match e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                    Some(exit) => Ok(exit.0),
                    None => Err(format!("{}: {e}", self.argv[0])),
                },
            }
        }
        .await;
        let fuel_used = self.fuel.saturating_sub(store.get_fuel().unwrap_or(0));
        // Dropping the store closes the child's ends of its pipes, which ends
        // its output streams.
        drop(store);
        (code, fuel_used)
    }
}

/// Feeds a guest's `stream<u8>` into a child's stdin pipe.
struct PipeConsumer(Pin<Box<DuplexStream>>);

impl<D> StreamConsumer<D> for PipeConsumer {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'_, D>,
        src: Source<'_, Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut src = src.as_direct(store);
        let buf = src.remaining();
        // A zero-length write asks whether the pipe is writable; say yes.
        if buf.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        match self.0.as_mut().poll_write(cx, buf) {
            Poll::Ready(Ok(n)) if n > 0 => {
                src.mark_read(n);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            // The child exited or closed its stdin.
            Poll::Ready(_) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Feeds a child's stdout or stderr pipe into a `stream<u8>` for the guest.
struct PipeProducer(Pin<Box<DuplexStream>>);

impl<D> StreamProducer<D> for PipeProducer {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // A zero-length read asks whether output is ready, which a pipe can't
        // tell without reading; say yes.
        if dst.remaining(store.as_context_mut()) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let mut dst = dst.as_direct(store, PIPE_CAPACITY);
        let mut buf = ReadBuf::new(dst.remaining());
        match self.0.as_mut().poll_read(cx, &mut buf) {
            // End of file: the child exited, or closed the stream.
            Poll::Ready(Ok(())) if buf.filled().is_empty() => {
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len();
                dst.mark_written(n);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(Err(_)) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl ExecutorState {
    /// Start waiting on `child`: what to run it with, given `fuel` left in
    /// the parent.
    fn take_child_run(
        &mut self,
        child: &Resource<ChildProcess>,
        fuel: u64,
    ) -> Result<ChildRun, ProcessError> {
        if self.suspended.is_some() {
            return Err(ProcessError::SpawnFailed);
        }
        let child = self
            .table
            .get_mut(child)
            .map_err(|_| ProcessError::IoError)?;
        // Its argv and env move into the run and are freed with it, even if
        // the parent's timeout drops it midway.
        self.child_budget
            .release(std::mem::take(&mut child.charged));
        let mut run = child.run.take().ok_or(ProcessError::IoError)?;
        run.fuel = fuel.min(CHILD_FUEL);
        run.memory = Arc::clone(&self.child_budget.memory);
        Ok(run)
    }
}

impl Host for ExecutorState {
    async fn list_commands(&mut self) -> Vec<String> {
        self.commands.0.keys().cloned().collect()
    }
}

impl HostChild for ExecutorState {
    async fn drop(&mut self, child: Resource<ChildProcess>) -> wasmtime::Result<()> {
        let child = self.table.delete(child)?;
        self.child_budget.release(child.charged);
        Ok(())
    }
}

impl<T: Send> HostChildWithStore<T> for HasSelf<ExecutorState> {
    fn spawn(
        mut host: Access<'_, T, Self>,
        cmd: String,
        args: Vec<String>,
        mut env: Vec<(String, String)>,
        cwd: String,
        stdin: StreamReader<u8>,
    ) -> Result<Spawned, ProcessError> {
        let state = host.get();
        if state.suspended.is_some() {
            return Err(ProcessError::SpawnFailed);
        }
        let command = state
            .commands
            .0
            .get(&cmd)
            .ok_or(ProcessError::CommandNotFound)?
            .clone();
        // WASI 0.2 has no working directory; tell the child where it is. The
        // rest of a POSIX-ish environment (HOME, TMPDIR, /dev/null) is #832.
        if !env.iter().any(|(k, _)| k == "PWD") {
            env.push(("PWD".to_owned(), cwd));
        }
        let mut argv = vec![cmd];
        argv.extend(args);
        let args_bytes = argv.iter().map(String::len).sum::<usize>()
            + env.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
        let live = (args_bytes <= MAX_ARGS_BYTES)
            .then(|| LiveSlot::take(&state.child_budget.live))
            .flatten();
        let Some(live) = live else {
            tracing::warn!(
                args_bytes,
                live = state.child_budget.live.load(Ordering::Relaxed),
                "child spawn over the argument or live-child limit"
            );
            return Err(ProcessError::LimitExceeded);
        };
        let charged = args_bytes as u64;
        state.child_budget.charge(charged)?;

        let (stdin_rx, stdin_tx) = tokio::io::duplex(PIPE_CAPACITY);
        let (stdout_rx, stdout_tx) = tokio::io::duplex(PIPE_CAPACITY);
        let (stderr_rx, stderr_tx) = tokio::io::duplex(PIPE_CAPACITY);
        let child = ChildProcess {
            run: Some(ChildRun {
                pre: command.pre,
                argv,
                env,
                stdin: stdin_rx,
                stdout: stdout_tx,
                stderr: stderr_tx,
                fuel: 0,
                fuel_used: Arc::default(),
                memory: Arc::default(),
                _live: live,
                #[cfg(feature = "unstable-vfs")]
                vfs: state
                    .hybrid_vfs_ctx
                    .as_ref()
                    .map(eryx_vfs::HybridVfsCtx::fork),
            }),
            charged,
        };
        let spawned = (|| -> wasmtime::Result<Spawned> {
            stdin.pipe(&mut host, PipeConsumer(Box::pin(stdin_tx)))?;
            let stdout = StreamReader::new(&mut host, PipeProducer(Box::pin(stdout_rx)))?;
            let stderr = StreamReader::new(&mut host, PipeProducer(Box::pin(stderr_rx)))?;
            let child = host.get().table.push(child)?;
            Ok(Spawned {
                child,
                stdout,
                stderr,
            })
        })();
        let state = host.get();
        match spawned {
            Ok(spawned) => Ok(spawned),
            Err(e) => {
                tracing::warn!(error = %e, "connecting a child's stdio failed");
                state.child_budget.release(charged);
                Err(ProcessError::SpawnFailed)
            }
        }
    }

    /// Run the child, charging the parent the fuel it burns as it runs.
    ///
    /// If the parent runs out, the child is killed and the parent's fuel left
    /// at 0, so the guest traps (`FuelExhausted`) as soon as it resumes,
    /// before it can see this call's result.
    async fn wait(
        accessor: &Accessor<T, Self>,
        child: Resource<ChildProcess>,
    ) -> Result<i32, ProcessError> {
        let run = accessor.with(|mut access| {
            let fuel = access.as_context_mut().get_fuel().unwrap_or(0);
            access.get().take_child_run(&child, fuel)
        })?;
        // The child is charged on its epoch ticks, so they must run even in
        // an execution with no timeout or cancellation.
        if let Err(e) = crate::wasm::ensure_epoch_ticker(run.pre.engine()) {
            tracing::warn!(error = %e, "no epoch ticker; the child is charged only when it exits");
        }
        let fuel_used = Arc::clone(&run.fuel_used);
        let mut charged = 0;
        // Move what the child has burned since the last charge to the parent;
        // false once the parent has none left.
        let mut charge = |used: u64| {
            let delta = used.saturating_sub(charged);
            charged = charged.max(used);
            accessor.with(|mut access| {
                let mut store = access.as_context_mut();
                let left = store.get_fuel().unwrap_or(0).saturating_sub(delta);
                // Fuel is always enabled on the shared engine.
                let _ = store.set_fuel(left);
                left > 0
            })
        };
        let mut running = std::pin::pin!(run.run());
        let finished = std::future::poll_fn(|cx| match running.as_mut().poll(cx) {
            Poll::Ready(done) => Poll::Ready(Some(done)),
            // The child yielded (an epoch tick) or is waiting on a pipe.
            Poll::Pending if charge(fuel_used.load(Ordering::Relaxed)) => Poll::Pending,
            Poll::Pending => Poll::Ready(None),
        })
        .await;
        let Some((code, used)) = finished else {
            tracing::warn!("parent ran out of fuel; killed its child");
            return Err(ProcessError::SpawnFailed);
        };
        if !charge(used) {
            return Err(ProcessError::SpawnFailed);
        }
        code.map_err(|e| {
            tracing::warn!(error = %e, "child command failed");
            ProcessError::SpawnFailed
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// How many commands have been linked into a `CommandPre`.
    pub(super) static COMPILED: AtomicUsize = AtomicUsize::new(0);

    fn test_cmd_wasm() -> Vec<u8> {
        let manifest =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-cmd/Cargo.toml");
        let target_dir = std::env::temp_dir().join("eryx-unit-test-cmd");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--release", "--locked"])
            .args(["--target", "wasm32-wasip2", "--manifest-path"])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", &target_dir)
            .status()
            .unwrap();
        assert!(status.success(), "building tests/fixtures/test-cmd failed");
        std::fs::read(target_dir.join("wasm32-wasip2/release/eryx-test-cmd.wasm")).unwrap()
    }

    /// Running reuses the command's `CommandPre`: nothing is compiled or
    /// linked after `Command::from_bytes`, however many children run.
    #[tokio::test]
    async fn runs_reuse_the_compiled_command() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let command = Command::from_bytes(&test_cmd_wasm()).unwrap();
        let compiled = COMPILED.load(Ordering::SeqCst);
        for input in ["one", "two"] {
            let (stdin, mut stdin_tx) = tokio::io::duplex(PIPE_CAPACITY);
            let (mut stdout_rx, stdout) = tokio::io::duplex(PIPE_CAPACITY);
            let (_stderr_rx, stderr) = tokio::io::duplex(PIPE_CAPACITY);
            stdin_tx.write_all(input.as_bytes()).await.unwrap();
            drop(stdin_tx);
            let run = ChildRun {
                pre: command.pre.clone(),
                argv: vec!["upper".into()],
                env: vec![],
                stdin,
                stdout,
                stderr,
                fuel: CHILD_FUEL,
                fuel_used: Arc::default(),
                memory: Arc::default(),
                _live: LiveSlot::take(&Arc::default()).unwrap(),
                #[cfg(feature = "unstable-vfs")]
                vfs: None,
            };
            let (code, fuel_used) = run.run().await;
            assert_eq!(code.unwrap(), 0);
            assert!(fuel_used > 0);
            let mut out = String::new();
            stdout_rx.read_to_string(&mut out).await.unwrap();
            assert_eq!(out, input.to_uppercase());
        }
        assert_eq!(COMPILED.load(Ordering::SeqCst), compiled);
    }
}
