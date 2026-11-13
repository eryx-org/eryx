//! SPIKE: `eryx:process`, child processes for the shell guest.
//!
//! Commands are WASI p2 command components (`wasi:cli/run`), registered via
//! `ERYX_SPIKE_COMMANDS=name=path,name=path,...` (a multicall binary such as
//! uutils coreutils is listed once per util; each distinct path compiles once).
//! Each spawn instantiates from a cached [`CommandPre`] on the shared engine,
//! so children get the same pooling allocator, fuel metering and epoch
//! interruption as the sandbox itself.
//!
//! Children run in batch, matching brush's driver: stdin is buffered until
//! `wait`, the child runs to completion inside `wait` (a fiber-async host call,
//! blocking to the guest), then its output is read.
//!
//! With `unstable-vfs`, a child gets a [fork](eryx_vfs::HybridVfsCtx::fork)
//! of the parent's hybrid VFS: the same files, mounts and permissions.
//! Without it, children have no filesystem.
//!
//! Not done: a child's fuel and memory are its own fixed budgets rather than
//! charged to the parent, and there is no network.

use std::collections::HashMap;
use std::sync::OnceLock;

use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p2::bindings::CommandPre;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

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

/// Most bytes kept from each of a child's stdout and stderr.
// ponytail: fixed caps; make them ResourceLimits fields if this graduates.
const CHILD_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
const CHILD_FUEL: u64 = 10_000_000_000;
const CHILD_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// Add the `eryx:process` interface to a sandbox linker.
pub(crate) fn add_to_linker(linker: &mut Linker<ExecutorState>) -> wasmtime::Result<()> {
    bindings::eryx::process::process::add_to_linker::<_, HasSelf<ExecutorState>>(linker, |s| s)
}

struct ChildState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    #[cfg(feature = "unstable-vfs")]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

#[cfg(feature = "unstable-vfs")]
impl eryx_vfs::HybridVfsView for ChildState {
    type Storage = eryx_vfs::ArcStorage;

    #[allow(clippy::expect_used)]
    fn hybrid_vfs(&mut self) -> eryx_vfs::HybridVfsState<'_, Self::Storage> {
        eryx_vfs::HybridVfsState::new(
            self.vfs.as_mut().expect("child VFS not configured"),
            &mut self.table,
        )
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

struct Registry {
    engine: Engine,
    commands: HashMap<String, CommandPre<ChildState>>,
}

/// The command registry from `ERYX_SPIKE_COMMANDS`, built on first spawn.
fn registry() -> Result<&'static Registry, String> {
    static REGISTRY: OnceLock<Result<Registry, String>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let engine =
                crate::PythonExecutor::shared_engine().map_err(|e| format!("engine: {e}"))?;
            let mut linker = Linker::<ChildState>::new(&engine);
            wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|e| e.to_string())?;
            #[cfg(feature = "unstable-vfs")]
            {
                linker.allow_shadowing(true);
                eryx_vfs::add_hybrid_vfs_to_linker(&mut linker).map_err(|e| e.to_string())?;
                linker.allow_shadowing(false);
            }
            let spec = std::env::var("ERYX_SPIKE_COMMANDS").unwrap_or_default();
            let mut by_path: HashMap<&str, CommandPre<ChildState>> = HashMap::new();
            let mut commands = HashMap::new();
            for entry in spec.split(',').filter(|e| !e.is_empty()) {
                let (name, path) = entry
                    .split_once('=')
                    .ok_or_else(|| format!("bad ERYX_SPIKE_COMMANDS entry {entry:?}"))?;
                let pre = match by_path.get(path) {
                    Some(pre) => pre.clone(),
                    None => {
                        let start = std::time::Instant::now();
                        let component = Component::from_file(&engine, path)
                            .map_err(|e| format!("{path}: {e}"))?;
                        let pre = CommandPre::new(
                            linker
                                .instantiate_pre(&component)
                                .map_err(|e| format!("{path}: {e}"))?,
                        )
                        .map_err(|e| format!("{path}: {e}"))?;
                        tracing::info!(path, elapsed = ?start.elapsed(), "compiled child command");
                        by_path.insert(path, pre.clone());
                        pre
                    }
                };
                commands.insert(name.to_owned(), pre);
            }
            Ok(Registry { engine, commands })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// A spawned child: its invocation, buffered stdin, and (after `wait`) its
/// exit code and unread output.
pub struct ChildProcess {
    pre: CommandPre<ChildState>,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    stdin: Vec<u8>,
    stdin_closed: bool,
    done: Option<(i32, Vec<u8>, Vec<u8>)>,
    #[cfg(feature = "unstable-vfs")]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

impl ChildProcess {
    async fn run(&mut self) -> Result<i32, String> {
        if let Some((code, ..)) = &self.done {
            return Ok(*code);
        }
        let engine = &registry()?.engine;
        let stdout = MemoryOutputPipe::new(CHILD_OUTPUT_LIMIT);
        let stderr = MemoryOutputPipe::new(CHILD_OUTPUT_LIMIT);
        let wasi = WasiCtxBuilder::new()
            .args(&self.argv)
            .envs(&self.env)
            .stdin(MemoryInputPipe::new(std::mem::take(&mut self.stdin)))
            .stdout(stdout.clone())
            .stderr(stderr.clone())
            .build();
        let mut store = Store::new(
            engine,
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

impl Host for ExecutorState {}

impl HostChild for ExecutorState {
    async fn spawn(
        &mut self,
        cmd: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        _cwd: String,
    ) -> Result<Resource<ChildProcess>, ProcessError> {
        if self.suspended.is_some() {
            return Err(ProcessError::SpawnFailed);
        }
        let registry = registry().map_err(|e| {
            tracing::warn!(error = %e, "child command registry unavailable");
            ProcessError::SpawnFailed
        })?;
        let pre = registry
            .commands
            .get(&cmd)
            .ok_or(ProcessError::CommandNotFound)?
            .clone();
        let mut argv = vec![cmd];
        argv.extend(args);
        let child = ChildProcess {
            pre,
            argv,
            env,
            stdin: Vec::new(),
            stdin_closed: false,
            done: None,
            #[cfg(feature = "unstable-vfs")]
            vfs: self
                .hybrid_vfs_ctx
                .as_ref()
                .map(eryx_vfs::HybridVfsCtx::fork),
        };
        self.table
            .push(child)
            .map_err(|_| ProcessError::SpawnFailed)
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
        Ok(child
            .done
            .as_mut()
            .map(|(_, out, _)| drain(out, max_bytes))
            .unwrap_or_default())
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
        Ok(child
            .done
            .as_mut()
            .map(|(_, _, err)| drain(err, max_bytes))
            .unwrap_or_default())
    }

    async fn wait(&mut self, child: Resource<ChildProcess>) -> Result<i32, ProcessError> {
        if self.suspended.is_some() {
            return Err(ProcessError::SpawnFailed);
        }
        let child = self
            .table
            .get_mut(&child)
            .map_err(|_| ProcessError::IoError)?;
        child.run().await.map_err(|e| {
            tracing::warn!(error = %e, "child command failed");
            ProcessError::SpawnFailed
        })
    }

    async fn drop(&mut self, child: Resource<ChildProcess>) -> wasmtime::Result<()> {
        self.table.delete(child)?;
        Ok(())
    }
}
