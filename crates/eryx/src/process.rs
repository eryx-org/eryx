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
//! Children run in batch: stdin is buffered until `wait`, the child runs to
//! completion inside `wait`, and then its output can be read. Streaming
//! between pipeline stages is #815.
//!
//! What a child can reach:
//! - **Files:** with `unstable-vfs`, a [fork](eryx_vfs::HybridVfsCtx::fork) of
//!   the parent's hybrid VFS, so the same mounts and permissions and nothing
//!   else. Without it, no filesystem.
//! - **Network:** none. Children get no sockets; the proxy is #812.
//! - **Limits:** a child burns the parent execution's fuel (at most
//!   `CHILD_FUEL` per child) and stops at its timeout and cancellation.
//!   Memory is per child: each may grow to `CHILD_MEMORY_LIMIT`, on top of
//!   the parent's own limit, and the host memory children hold is bounded by
//!   the execution's `ChildBudget`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use wasmtime::component::{Access, Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{AsContextMut, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::p2::bindings::CommandPre;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::error::{Error, ErrorKind};
use crate::wasm::ExecutorState;

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "eryx:process/spawner",
        // `wait` gets the parent's store, to charge it the child's fuel.
        imports: {
            "eryx:process/process.[method]child.wait": async | store,
            default: async,
        },
        with: { "eryx:process/process.child": super::ChildProcess },
    });
}

use bindings::eryx::process::process::{Host, HostChild, HostChildWithStore, ProcessError};

/// Most bytes buffered for each of a child's stdin, stdout and stderr.
// ponytail: batch pipes hold whole streams; streaming them is #815.
const CHILD_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
/// Most fuel one child may burn, taken from the parent's.
const CHILD_FUEL: u64 = 10_000_000_000;
/// Memory each child may grow to, on top of the parent's limit.
// ponytail: per child, not charged to the parent's max_memory_bytes; one
// child runs at a time, so a script holds at most this much more.
const CHILD_MEMORY_LIMIT: usize = 256 * 1024 * 1024;
/// Children spawned but not yet waited on, at most, per execution.
const MAX_LIVE_CHILDREN: usize = 64;
/// Bytes of argv and env one spawn may pass, at most (like `ARG_MAX`).
const MAX_ARGS_BYTES: usize = 2 * 1024 * 1024;
/// Default for [`ResourceLimits::max_child_host_bytes`](crate::ResourceLimits::max_child_host_bytes).
pub(crate) const DEFAULT_CHILD_HOST_BYTES: u64 = 64 * 1024 * 1024;

/// Host memory held for an execution's children, outside the guest's memory
/// limit: their argv and env, buffered stdin, and unread output. The shell
/// spawns every pipeline stage before running any, so without this a script
/// could make the host hold many large buffers at once.
#[derive(Debug, Clone)]
pub(crate) struct ChildBudget {
    limit: u64,
    used: u64,
    live: usize,
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
            live: 0,
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

    /// Bytes left to charge.
    fn remaining(&self) -> u64 {
        self.limit - self.used
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
    #[cfg(any(feature = "embedded", feature = "preinit", feature = "packages"))]
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
    #[cfg(any(feature = "embedded", feature = "preinit", feature = "packages"))]
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

/// A host directory an isolated command sees, at `guest`.
#[cfg(feature = "packages")]
pub(crate) struct Mount<'a> {
    pub(crate) host: &'a Path,
    pub(crate) guest: &'a str,
    pub(crate) writable: bool,
}

/// What an isolated command may use.
#[cfg(feature = "packages")]
pub(crate) struct IsolatedLimits {
    /// Bytes of linear memory.
    pub(crate) memory: usize,
    /// Fuel (roughly, wasm instructions).
    pub(crate) fuel: u64,
    /// Wall-clock time.
    pub(crate) timeout: std::time::Duration,
    /// Bytes kept of each of stdout and stderr.
    pub(crate) output: usize,
}

#[cfg(feature = "packages")]
impl Command {
    /// Run the command on its own, outside any sandbox, with `argv`, no
    /// environment variables, empty stdin, no network (no sockets or name
    /// lookups), and only `mounts` as its filesystem, within `limits`.
    /// Returns its exit code, stdout and stderr.
    pub(crate) async fn run_isolated(
        &self,
        argv: &[String],
        mounts: &[Mount<'_>],
        limits: &IsolatedLimits,
    ) -> Result<ChildOutput, Error> {
        let fail = |message: String| Error::from(ErrorKind::Execution(message));
        let stdout = MemoryOutputPipe::new(limits.output);
        let stderr = MemoryOutputPipe::new(limits.output);
        let mut wasi = WasiCtxBuilder::new();
        wasi.args(argv)
            .stdout(stdout.clone())
            .stderr(stderr.clone())
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .socket_addr_check(|_, _| Box::pin(async { false }));
        for mount in mounts {
            let perms = if mount.writable {
                wasmtime_wasi::FsPerms::ReadWrite
            } else {
                wasmtime_wasi::FsPerms::ReadOnly
            };
            wasi.preopened_dir(mount.host, mount.guest, perms)
                .map_err(|e| fail(format!("mounting {}: {e}", mount.host.display())))?;
        }
        let engine = self.pre.engine();
        crate::wasm::ensure_epoch_ticker(engine)?;
        let mut store = Store::new(
            engine,
            ChildState {
                wasi: wasi.build(),
                table: ResourceTable::new(),
                limits: StoreLimitsBuilder::new().memory_size(limits.memory).build(),
                #[cfg(feature = "unstable-vfs")]
                vfs: None,
            },
        );
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| fail(format!("setting fuel: {e}")))?;
        // Yield on every tick, so the timeout below can drop a CPU-bound run.
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Yield(1)));

        let run = async {
            let command = self
                .pre
                .instantiate_async(&mut store)
                .await
                .map_err(|e| fail(format!("instantiating {}: {e}", argv[0])))?;
            match command.wasi_cli_run().call_run(&mut store).await {
                Ok(Ok(())) => Ok(0),
                Ok(Err(())) => Ok(1),
                Err(e) => match e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                    Some(exit) => Ok(exit.0),
                    None => Err(fail(format!("{} failed: {e}", argv[0]))),
                },
            }
        };
        let code = tokio::time::timeout(limits.timeout, run)
            .await
            .map_err(|_| fail(format!("{} timed out after {:?}", argv[0], limits.timeout)))??;
        drop(store);
        Ok((code, stdout.contents().to_vec(), stderr.contents().to_vec()))
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
    limits: StoreLimits,
    #[cfg(feature = "unstable-vfs")]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
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

/// A spawned child: its invocation, buffered stdin, and (after `wait`) its
/// exit code and unread output.
pub struct ChildProcess {
    command: Command,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    stdin: Vec<u8>,
    stdin_closed: bool,
    done: Option<ChildOutput>,
    /// What this child holds of the execution's [`ChildBudget`].
    charged: u64,
    /// Not yet waited on, so it counts as live.
    live: bool,
    #[cfg(feature = "unstable-vfs")]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

/// What `wait` runs a child with, taken out of its handle.
struct ChildRun {
    pre: CommandPre<ChildState>,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    stdin: Vec<u8>,
    /// Most bytes kept of each of stdout and stderr.
    output_room: usize,
    /// Fuel the child may burn.
    fuel: u64,
    #[cfg(feature = "unstable-vfs")]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

/// A child's exit code, stdout and stderr.
type ChildOutput = (i32, Vec<u8>, Vec<u8>);

impl ChildRun {
    /// Run the child to completion, returning its output and the fuel it
    /// burned.
    async fn run(self) -> (Result<ChildOutput, String>, u64) {
        let stdout = MemoryOutputPipe::new(self.output_room);
        let stderr = MemoryOutputPipe::new(self.output_room);
        let wasi = WasiCtxBuilder::new()
            .args(&self.argv)
            .envs(&self.env)
            .stdin(MemoryInputPipe::new(self.stdin))
            .stdout(stdout.clone())
            .stderr(stderr.clone())
            .build();
        let mut store = Store::new(
            self.pre.engine(),
            ChildState {
                wasi,
                table: ResourceTable::new(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(CHILD_MEMORY_LIMIT)
                    .build(),
                #[cfg(feature = "unstable-vfs")]
                vfs: self.vfs,
            },
        );
        store.limiter(|s| &mut s.limits);
        // Yield on every epoch tick. The parent's timeout and cancellation
        // race its whole execution, so yielding lets them drop a CPU-bound
        // child as they would a guest blocked in `time.sleep`. Without a
        // timeout or cancellation the epoch clock may not run at all.
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Yield(1)));

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
        drop(store);
        let output =
            code.map(|code| (code, stdout.contents().to_vec(), stderr.contents().to_vec()));
        (output, fuel_used)
    }
}

impl ExecutorState {
    /// Start waiting on `child`: what to run it with, given `fuel` left in
    /// the parent, or its exit code if it has already run.
    fn take_child_run(
        &mut self,
        child: &Resource<ChildProcess>,
        fuel: u64,
    ) -> Result<Result<ChildRun, i32>, ProcessError> {
        if self.suspended.is_some() {
            return Err(ProcessError::SpawnFailed);
        }
        let child = self
            .table
            .get_mut(child)
            .map_err(|_| ProcessError::IoError)?;
        if child.live {
            child.live = false;
            self.child_budget.live -= 1;
        }
        if let Some((code, ..)) = &child.done {
            return Ok(Err(*code));
        }
        if child.argv.is_empty() {
            // It already ran, and failed.
            return Err(ProcessError::SpawnFailed);
        }
        // Each output stream gets half of what is left while the argv, env
        // and stdin are still held. Those move into the run and are freed
        // with it, even if the parent's timeout drops it midway, so only the
        // output is charged from here on.
        let output_room = (self.child_budget.remaining() / 2).min(CHILD_OUTPUT_LIMIT as u64);
        self.child_budget
            .release(std::mem::take(&mut child.charged));
        Ok(Ok(ChildRun {
            pre: child.command.pre.clone(),
            argv: std::mem::take(&mut child.argv),
            env: std::mem::take(&mut child.env),
            stdin: std::mem::take(&mut child.stdin),
            output_room: output_room as usize,
            fuel: fuel.min(CHILD_FUEL),
            #[cfg(feature = "unstable-vfs")]
            vfs: child.vfs.take(),
        }))
    }

    /// Record how `child`'s run ended.
    fn finish_child(
        &mut self,
        child: &Resource<ChildProcess>,
        output: Result<ChildOutput, String>,
    ) -> Result<i32, ProcessError> {
        let child = self
            .table
            .get_mut(child)
            .map_err(|_| ProcessError::IoError)?;
        let (code, out, err) = output.map_err(|e| {
            tracing::warn!(error = %e, "child command failed");
            ProcessError::SpawnFailed
        })?;
        // Fits: each stream was capped at half of what was left.
        let held = (out.len() + err.len()) as u64;
        self.child_budget.used += held;
        child.charged += held;
        child.done = Some((code, out, err));
        Ok(code)
    }
}

fn drain(buf: &mut Vec<u8>, max: u32) -> Vec<u8> {
    let n = buf.len().min(max as usize);
    buf.drain(..n).collect()
}

impl Host for ExecutorState {
    async fn list_commands(&mut self) -> Vec<String> {
        self.commands.0.keys().cloned().collect()
    }
}

impl HostChild for ExecutorState {
    async fn spawn(
        &mut self,
        cmd: String,
        args: Vec<String>,
        mut env: Vec<(String, String)>,
        cwd: String,
    ) -> Result<Resource<ChildProcess>, ProcessError> {
        if self.suspended.is_some() {
            return Err(ProcessError::SpawnFailed);
        }
        let command = self
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
        if args_bytes > MAX_ARGS_BYTES || self.child_budget.live >= MAX_LIVE_CHILDREN {
            tracing::warn!(
                args_bytes,
                live = self.child_budget.live,
                "child spawn over the argument or live-child limit"
            );
            return Err(ProcessError::LimitExceeded);
        }
        let charged = args_bytes as u64;
        self.child_budget.charge(charged)?;
        self.child_budget.live += 1;
        let child = ChildProcess {
            command,
            argv,
            env,
            stdin: Vec::new(),
            stdin_closed: false,
            done: None,
            charged,
            live: true,
            #[cfg(feature = "unstable-vfs")]
            vfs: self
                .hybrid_vfs_ctx
                .as_ref()
                .map(eryx_vfs::HybridVfsCtx::fork),
        };
        self.table.push(child).map_err(|_| {
            self.child_budget.release(charged);
            self.child_budget.live -= 1;
            ProcessError::SpawnFailed
        })
    }

    async fn write_stdin(
        &mut self,
        child: Resource<ChildProcess>,
        data: Vec<u8>,
    ) -> Result<u64, ProcessError> {
        let child = self
            .table
            .get_mut(&child)
            .map_err(|_| ProcessError::IoError)?;
        if child.stdin_closed {
            return Err(ProcessError::IoError);
        }
        // Stdin is buffered until `wait`, so cap it like the output.
        if child.stdin.len() + data.len() > CHILD_OUTPUT_LIMIT {
            return Err(ProcessError::LimitExceeded);
        }
        self.child_budget.charge(data.len() as u64)?;
        child.charged += data.len() as u64;
        child.stdin.extend_from_slice(&data);
        Ok(data.len() as u64)
    }

    async fn close_stdin(&mut self, child: Resource<ChildProcess>) {
        if let Ok(child) = self.table.get_mut(&child) {
            child.stdin_closed = true;
        }
    }

    async fn read_stdout(
        &mut self,
        child: Resource<ChildProcess>,
        max_bytes: u32,
    ) -> Result<Vec<u8>, ProcessError> {
        let child = self
            .table
            .get_mut(&child)
            .map_err(|_| ProcessError::IoError)?;
        let out = child
            .done
            .as_mut()
            .map(|(_, out, _)| drain(out, max_bytes))
            .unwrap_or_default();
        child.charged -= out.len() as u64;
        self.child_budget.release(out.len() as u64);
        Ok(out)
    }

    async fn read_stderr(
        &mut self,
        child: Resource<ChildProcess>,
        max_bytes: u32,
    ) -> Result<Vec<u8>, ProcessError> {
        let child = self
            .table
            .get_mut(&child)
            .map_err(|_| ProcessError::IoError)?;
        let err = child
            .done
            .as_mut()
            .map(|(_, _, err)| drain(err, max_bytes))
            .unwrap_or_default();
        child.charged -= err.len() as u64;
        self.child_budget.release(err.len() as u64);
        Ok(err)
    }

    async fn drop(&mut self, child: Resource<ChildProcess>) -> wasmtime::Result<()> {
        let child = self.table.delete(child)?;
        self.child_budget.release(child.charged);
        if child.live {
            self.child_budget.live -= 1;
        }
        Ok(())
    }
}

impl<T: Send> HostChildWithStore<T> for HasSelf<ExecutorState> {
    /// Run the child, charging the parent the fuel it burns.
    async fn wait(
        mut host: Access<'_, T, Self>,
        child: Resource<ChildProcess>,
    ) -> Result<i32, ProcessError> {
        let fuel = host.as_context_mut().get_fuel().unwrap_or(0);
        let run = match host.get().take_child_run(&child, fuel)? {
            Ok(run) => run,
            Err(code) => return Ok(code),
        };
        let (output, fuel_used) = run.run().await;
        let mut store = host.as_context_mut();
        let left = store.get_fuel().unwrap_or(0).saturating_sub(fuel_used);
        // Fuel is always enabled on the shared engine.
        let _ = store.set_fuel(left);
        host.get().finish_child(&child, output)
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

    /// Spawning reuses the command's `CommandPre`: nothing is compiled or
    /// linked after `Command::from_bytes`, however many children run.
    #[tokio::test]
    async fn spawns_reuse_the_compiled_command() {
        let command = Command::from_bytes(&test_cmd_wasm()).unwrap();
        let compiled = COMPILED.load(Ordering::SeqCst);
        let mut commands = Commands::default();
        commands.insert("upper".into(), command);
        let mut state = ExecutorState::placeholder();
        state.commands = commands;

        for input in ["one", "two"] {
            let child = HostChild::spawn(&mut state, "upper".into(), vec![], vec![], "/".into())
                .await
                .unwrap();
            let rep = Resource::new_borrow(child.rep());
            HostChild::write_stdin(&mut state, Resource::new_borrow(child.rep()), input.into())
                .await
                .unwrap();
            HostChild::close_stdin(&mut state, Resource::new_borrow(child.rep())).await;
            // `wait`, without the parent store it charges.
            let Ok(run) = state.take_child_run(&rep, u64::MAX).unwrap() else {
                panic!("not run yet");
            };
            let (output, fuel_used) = run.run().await;
            assert!(fuel_used > 0);
            assert_eq!(state.finish_child(&rep, output).unwrap(), 0);
            let out = HostChild::read_stdout(&mut state, Resource::new_borrow(child.rep()), 64)
                .await
                .unwrap();
            assert_eq!(out, input.to_uppercase().into_bytes());
            HostChild::drop(&mut state, child).await.unwrap();
        }
        assert_eq!(COMPILED.load(Ordering::SeqCst), compiled);
        // Dropped children give back everything they held.
        assert_eq!((state.child_budget.used, state.child_budget.live), (0, 0));
    }

    /// Output room is sized with the child's own inputs still charged, so
    /// inputs plus output stay within the budget; starting the run then
    /// releases the inputs.
    #[tokio::test]
    async fn output_room_counts_the_childs_inputs() {
        let mut commands = Commands::default();
        commands.insert(
            "upper".into(),
            Command::from_bytes(&test_cmd_wasm()).unwrap(),
        );
        let mut state = ExecutorState::placeholder();
        state.commands = commands;
        state.child_budget = ChildBudget::new(Some(4 << 20));

        let env = vec![("X".to_owned(), "x".repeat(1 << 20))];
        let child = HostChild::spawn(&mut state, "upper".into(), vec![], env, "/".into())
            .await
            .unwrap();
        let rep = Resource::new_borrow(child.rep());
        HostChild::write_stdin(
            &mut state,
            Resource::new_borrow(child.rep()),
            vec![0; 1 << 20],
        )
        .await
        .unwrap();
        let held = state.child_budget.used;
        assert!(held > 2 << 20);
        let Ok(run) = state.take_child_run(&rep, u64::MAX).unwrap() else {
            panic!("not run yet");
        };
        assert_eq!(run.output_room as u64, ((4 << 20) - held) / 2);
        assert_eq!(state.child_budget.used, 0);
    }

    #[tokio::test]
    async fn unregistered_names_are_not_found() {
        let mut state = ExecutorState::placeholder();
        let err = HostChild::spawn(&mut state, "nope".into(), vec![], vec![], "/".into())
            .await
            .err();
        assert!(matches!(err, Some(ProcessError::CommandNotFound)));
    }
}
