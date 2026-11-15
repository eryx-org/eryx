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
//! - **Limits:** a fixed fuel and memory budget of its own per child. Charging
//!   children to the parent execution is #810.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p2::bindings::CommandPre;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::error::{Error, ErrorKind};
use crate::wasm::ExecutorState;

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "eryx:process/spawner",
        imports: { default: async },
        with: { "eryx:process/process.child": super::ChildProcess },
    });
}

use bindings::eryx::process::process::{Host, HostChild, ProcessError};

/// Most bytes buffered for each of a child's stdin, stdout and stderr.
// ponytail: batch pipes hold whole streams; streaming them is #815.
const CHILD_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
/// Fuel each child gets.
// ponytail: fixed per-child budgets, not the parent's; charging the parent
// execution its children's fuel, memory and time is #810.
const CHILD_FUEL: u64 = 10_000_000_000;
/// Memory each child may grow to (see `CHILD_FUEL`, #810).
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
    done: Option<(i32, Vec<u8>, Vec<u8>)>,
    /// What this child holds of the execution's [`ChildBudget`].
    charged: u64,
    /// Not yet waited on, so it counts as live.
    live: bool,
    #[cfg(feature = "unstable-vfs")]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

impl ChildProcess {
    /// Run the child to completion, keeping at most `output_room` bytes of its
    /// stdout and stderr each.
    async fn run(&mut self, output_room: usize) -> Result<i32, String> {
        if let Some((code, ..)) = &self.done {
            return Ok(*code);
        }
        let engine = self.command.pre.engine().clone();
        let stdout = MemoryOutputPipe::new(output_room);
        let stderr = MemoryOutputPipe::new(output_room);
        let wasi = WasiCtxBuilder::new()
            .args(&self.argv)
            .envs(&self.env)
            .stdin(MemoryInputPipe::new(std::mem::take(&mut self.stdin)))
            .stdout(stdout.clone())
            .stderr(stderr.clone())
            .build();
        let mut store = Store::new(
            &engine,
            ChildState {
                wasi,
                table: ResourceTable::new(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(CHILD_MEMORY_LIMIT)
                    .build(),
                #[cfg(feature = "unstable-vfs")]
                vfs: self.vfs.take(),
            },
        );
        store.limiter(|s| &mut s.limits);
        store.set_fuel(CHILD_FUEL).map_err(|e| e.to_string())?;
        store.set_epoch_deadline(u64::MAX / 2);

        let command = self
            .command
            .pre
            .instantiate_async(&mut store)
            .await
            .map_err(|e| format!("instantiate: {e}"))?;
        let code = match command.wasi_cli_run().call_run(&mut store).await {
            Ok(Ok(())) => 0,
            Ok(Err(())) => 1,
            Err(e) => match e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                Some(exit) => exit.0,
                None => return Err(format!("{}: {e}", self.argv[0])),
            },
        };
        drop(store);
        self.done = Some((code, stdout.contents().to_vec(), stderr.contents().to_vec()));
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

    async fn wait(&mut self, child: Resource<ChildProcess>) -> Result<i32, ProcessError> {
        if self.suspended.is_some() {
            return Err(ProcessError::SpawnFailed);
        }
        let child = self
            .table
            .get_mut(&child)
            .map_err(|_| ProcessError::IoError)?;
        if child.live {
            child.live = false;
            self.child_budget.live -= 1;
        }
        if child.done.is_none() {
            // Its stdin goes into the child and is freed with it; its output
            // is charged instead, each stream capped at what is left.
            let stdin = child.stdin.len() as u64;
            let room = (self.child_budget.remaining() / 2).min(CHILD_OUTPUT_LIMIT as u64);
            let result = child.run(room as usize).await;
            child.charged -= stdin;
            self.child_budget.release(stdin);
            let code = result.map_err(|e| {
                tracing::warn!(error = %e, "child command failed");
                ProcessError::SpawnFailed
            })?;
            let output = child
                .done
                .as_ref()
                .map_or(0, |(_, out, err)| (out.len() + err.len()) as u64);
            // Fits: each stream was capped at half of what was left.
            self.child_budget.used += output;
            child.charged += output;
            return Ok(code);
        }
        Ok(child.done.as_ref().map_or(0, |(code, ..)| *code))
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
            assert_eq!(HostChild::wait(&mut state, rep).await.unwrap(), 0);
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

    #[tokio::test]
    async fn unregistered_names_are_not_found() {
        let mut state = ExecutorState::placeholder();
        let err = HostChild::spawn(&mut state, "nope".into(), vec![], vec![], "/".into())
            .await
            .err();
        assert!(matches!(err, Some(ProcessError::CommandNotFound)));
    }
}
