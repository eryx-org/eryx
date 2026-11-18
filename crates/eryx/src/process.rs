//! External commands for the shell guest: `eryx:process`.
//!
//! A [`Command`] is a WASI 0.2 or 0.3 command component (one that exports
//! `wasi:cli/run`), compiled once on the child engine (the shared engine's
//! configuration with on-demand allocation, so children never take the
//! sandboxes' pooling slots; #833). A shell sandbox
//! registers commands by name
//! ([`SandboxBuilder::with_command`](crate::SandboxBuilder::with_command)), and
//! the shell spawns them as child processes through the `eryx:process`
//! interface. Each spawn instantiates from the command's cached
//! pre-instance, so children get the engine's fuel metering and epoch
//! interruption without recompiling.
//!
//! A child's stdin, stdout and stderr are component-model streams, piped
//! through small host buffers to its WASI stdio, so pipeline stages run
//! concurrently and a full pipe makes its writer wait. The child runs while
//! the guest awaits its `wait`.
//!
//! What a child can reach:
//! - **Files:** with `unstable-vfs` or `shell`, a
//!   [fork](eryx_vfs::HybridVfsCtx::fork) of the parent's hybrid VFS, so the
//!   same mounts and permissions and nothing else. Without either, no
//!   filesystem.
//! - **Network:** through the child proxy only, when the sandbox has one
//!   (#812); otherwise none.
//! - **Commands:** a child whose command was registered with
//!   [`Command::allow_spawn`] runs commands itself through
//!   `eryx:process/run`, to completion, or `eryx:process/pipe`, with live
//!   pipes (see [`piped`]), with the same commands, filesystem, network and
//!   execution limits, at most `MAX_SPAWN_DEPTH` deep.
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

use wasmtime::component::{
    Access, Accessor, Component, Destination, HasSelf, Linker, Resource, ResourceTable, Source,
    StreamConsumer, StreamProducer, StreamReader, StreamResult, VecBuffer,
};
use wasmtime::{
    AsContext, AsContextMut, Engine, ResourceLimiter, Store, StoreContextMut, UpdateDeadline,
};
use wasmtime_wasi::p3::bindings::sockets::types as p3_sockets;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::error::{Error, ErrorKind};
use crate::wasm::ExecutorState;

mod cached;
mod imports;
mod pipe;
mod piped;
mod preview1;
mod vmctx;

pub(crate) use imports::missing_message;
pub use imports::{CommandImport, CommandInfo, ImportSupport};

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

mod child_bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "eryx:process/child",
        with: { "eryx:process/pipe.piped": super::piped::Piped },
    });
}

use bindings::eryx::process::process::{
    Host, HostChild, HostChildWithStore, ProcessError, Spawned,
};
use child_bindings::eryx::process::run::{Output, RunError};

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
/// How deep children may spawn children: the shell's are depth 1, so a
/// command it runs may start commands three levels down (git running `git`
/// running a hook, say).
const MAX_SPAWN_DEPTH: usize = 4;
/// Most of each of stdout and stderr a nested `run` keeps.
const MAX_RUN_OUTPUT: usize = 16 * 1024 * 1024;
/// TCP sockets one child may hold open at once. Creating one opens a host
/// socket before any address is checked, so this, with
/// [`MAX_EXECUTION_SOCKETS`] and the process-wide limit, keeps a script from
/// exhausting the host's file descriptors.
const MAX_CHILD_SOCKETS: usize = 32;
/// TCP sockets an execution's children may hold open at once, together.
const MAX_EXECUTION_SOCKETS: usize = 128;
/// TCP sockets children may hold open at once across the process.
static PROCESS_CHILD_SOCKETS: AtomicUsize = AtomicUsize::new(0);

/// The process-wide limit on children's sockets: a quarter of the soft
/// `RLIMIT_NOFILE` (at least 64, at most 4096), or 1024 where there's none.
fn max_process_child_sockets() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        #[cfg(unix)]
        let soft = rustix::process::getrlimit(rustix::process::Resource::Nofile).current;
        #[cfg(not(unix))]
        let soft: Option<u64> = None;
        soft.map_or(1024, |n| {
            usize::try_from(n / 4).unwrap_or(4096).clamp(64, 4096)
        })
    })
}

/// A child's open TCP sockets, counted against the child's, its execution's
/// and the process's limits, and given back as they close (or the child's
/// store is dropped).
#[derive(Default)]
struct ChildSockets {
    /// Table indices of the sockets it created, and whether through WASI 0.3
    /// (whose sockets are another resource type); dropped ones are pruned.
    live: Vec<(u32, bool)>,
    /// Sockets its execution's children hold.
    execution: Arc<AtomicUsize>,
}

impl ChildSockets {
    fn new(execution: Arc<AtomicUsize>) -> Self {
        Self {
            live: Vec::new(),
            execution,
        }
    }

    fn release(&self, n: usize) {
        self.execution.fetch_sub(n, Ordering::Relaxed);
        PROCESS_CHILD_SOCKETS.fetch_sub(n, Ordering::Relaxed);
    }

    /// Forget sockets the child has dropped.
    fn prune(&mut self, table: &ResourceTable) {
        let before = self.live.len();
        self.live.retain(|&(rep, p3)| {
            if p3 {
                table
                    .get(&Resource::<wasmtime_wasi::sockets::TcpSocket>::new_borrow(
                        rep,
                    ))
                    .is_ok()
            } else {
                table
                    .get(&Resource::<wasmtime_wasi::p2::TcpSocket>::new_borrow(rep))
                    .is_ok()
            }
        });
        self.release(before - self.live.len());
    }

    /// Reserve one more socket, if every limit allows it.
    fn admit(&self) -> bool {
        if self.live.len() >= MAX_CHILD_SOCKETS {
            return false;
        }
        if self.execution.fetch_add(1, Ordering::Relaxed) >= MAX_EXECUTION_SOCKETS {
            self.execution.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        if PROCESS_CHILD_SOCKETS.fetch_add(1, Ordering::Relaxed) >= max_process_child_sockets() {
            self.release(1);
            return false;
        }
        true
    }
}

impl Drop for ChildSockets {
    fn drop(&mut self) {
        self.release(self.live.len());
    }
}

/// `wasi:sockets/tcp-create-socket` for children: wasmtime-wasi's, counted
/// against [`ChildSockets`].
struct CountedSockets;

impl wasmtime::component::HasData for CountedSockets {
    type Data<'a> = CountedSocketsView<'a>;
}

struct CountedSocketsView<'a> {
    sockets: wasmtime_wasi::sockets::WasiSocketsCtxView<'a>,
    budget: &'a mut ChildSockets,
}

// The interface's error conversions, as wasmtime-wasi's.
impl wasmtime_wasi::p2::bindings::sockets::network::Host for CountedSocketsView<'_> {
    fn convert_error_code(
        &mut self,
        error: wasmtime_wasi::p2::SocketError,
    ) -> wasmtime::Result<wasmtime_wasi::p2::bindings::sockets::network::ErrorCode> {
        self.sockets.convert_error_code(error)
    }

    fn network_error_code(
        &mut self,
        err: Resource<wasmtime::Error>,
    ) -> wasmtime::Result<Option<wasmtime_wasi::p2::bindings::sockets::network::ErrorCode>> {
        self.sockets.network_error_code(err)
    }
}

impl wasmtime_wasi::p2::bindings::sockets::network::HostNetwork for CountedSocketsView<'_> {
    fn drop(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::network::Network>,
    ) -> wasmtime::Result<()> {
        wasmtime_wasi::p2::bindings::sockets::network::HostNetwork::drop(&mut self.sockets, this)
    }
}

impl wasmtime_wasi::p2::bindings::sockets::tcp_create_socket::Host for CountedSocketsView<'_> {
    fn create_tcp_socket(
        &mut self,
        family: wasmtime_wasi::p2::bindings::sockets::network::IpAddressFamily,
    ) -> wasmtime_wasi::p2::SocketResult<Resource<wasmtime_wasi::p2::TcpSocket>> {
        self.budget.prune(self.sockets.table);
        if !self.budget.admit() {
            return Err(
                wasmtime_wasi::p2::bindings::sockets::network::ErrorCode::NewSocketLimit.into(),
            );
        }
        let created =
            wasmtime_wasi::p2::bindings::sockets::tcp_create_socket::Host::create_tcp_socket(
                &mut self.sockets,
                family,
            );
        match created {
            Ok(socket) => {
                self.budget.live.push((socket.rep(), false));
                Ok(socket)
            }
            Err(e) => {
                self.budget.release(1);
                Err(e)
            }
        }
    }
}

/// WASI 0.3's `[static]tcp-socket.create`, counted against [`ChildSockets`]
/// as [`CountedSockets`] counts WASI 0.2's.
fn create_p3_tcp_socket(
    mut store: StoreContextMut<'_, ChildState>,
    (family,): (p3_sockets::IpAddressFamily,),
) -> wasmtime::Result<(Result<Resource<wasmtime_wasi::sockets::TcpSocket>, p3_sockets::ErrorCode>,)>
{
    let state = store.data_mut();
    state.sockets.prune(&state.table);
    if !state.sockets.admit() {
        // WASI 0.3 has no `new-socket-limit`.
        return Ok((Err(p3_sockets::ErrorCode::Other(Some(
            "socket limit reached".into(),
        ))),));
    }
    let mut sockets = wasmtime_wasi::sockets::WasiSocketsCtxView {
        ctx: state.wasi.sockets(),
        table: &mut state.table,
    };
    match p3_sockets::HostTcpSocket::create(&mut sockets, family) {
        Ok(socket) => {
            state.sockets.live.push((socket.rep(), true));
            Ok((Ok(socket),))
        }
        Err(e) => {
            state.sockets.release(1);
            Ok((Err(e.downcast()?),))
        }
    }
}

/// The interface holding WASI 0.3's `tcp-socket.create`, as wasmtime-wasi
/// links it.
const P3_SOCKETS: &str = "wasi:sockets/types@0.3.0";
/// That function's name in it.
const P3_TCP_CREATE: &str = "[static]tcp-socket.create";

/// Replace wasmtime-wasi's WASI 0.3 `tcp-socket.create` in `linker` with
/// [`create_p3_tcp_socket`].
///
/// Fails unless wasmtime-wasi defined it under [`P3_SOCKETS`]: after a
/// wasmtime-wasi upgrade moves the interface to another version, children
/// would otherwise create sockets through it uncounted.
fn count_p3_sockets(linker: &mut Linker<ChildState>) -> wasmtime::Result<()> {
    linker.allow_shadowing(false);
    let defined = linker
        .instance(P3_SOCKETS)?
        .func_wrap(P3_TCP_CREATE, create_p3_tcp_socket)
        .is_err();
    linker.allow_shadowing(true);
    if !defined {
        wasmtime::bail!("wasmtime-wasi no longer links `{P3_TCP_CREATE}` in `{P3_SOCKETS}`");
    }
    linker
        .instance(P3_SOCKETS)?
        .func_wrap(P3_TCP_CREATE, create_p3_tcp_socket)
}

/// Where a child finds the proxy's CA certificate (see [`ChildNet`]).
const CA_DIR: &str = "/etc/ssl/certs";
/// The CA bundle's file name in [`CA_DIR`].
const CA_FILE: &str = "ca-certificates.crt";
/// The env vars naming the proxy.
const PROXY_ENV: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];
/// The env vars naming the CA bundle.
const CA_ENV: [&str; 4] = [
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
];
/// Default for [`ResourceLimits::max_child_host_bytes`](crate::ResourceLimits::max_child_host_bytes).
pub(crate) const DEFAULT_CHILD_HOST_BYTES: u64 = 64 * 1024 * 1024;

/// Host memory held for an execution's children's argv and env, outside the
/// guest's memory limit. The shell spawns every pipeline stage before running
/// any, so without this a script could make the host hold many large
/// argument lists at once. (Their stdio pipes are `PIPE_CAPACITY` each.)
///
/// Clones share it: nested children (see [`Spawner`]) draw on their
/// execution's.
#[derive(Debug, Clone)]
pub(crate) struct ChildBudget {
    limit: u64,
    used: Arc<AtomicU64>,
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
            used: Arc::default(),
            live: Arc::default(),
            memory: Arc::default(),
        }
    }

    /// Take `bytes` from the budget, or fail if that would overdraw it.
    fn charge(&self, bytes: u64) -> Result<(), ProcessError> {
        let charged = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&u| u <= self.limit)
            });
        if let Err(used) = charged {
            tracing::warn!(
                limit = self.limit,
                used,
                bytes,
                "child host-memory budget exhausted"
            );
            return Err(ProcessError::LimitExceeded);
        }
        Ok(())
    }

    fn release(&self, bytes: u64) {
        let _ = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_sub(bytes))
            });
    }
}

/// Add the `eryx:process` interface to a sandbox linker.
pub(crate) fn add_to_linker(linker: &mut Linker<ExecutorState>) -> wasmtime::Result<()> {
    bindings::eryx::process::process::add_to_linker::<_, HasSelf<ExecutorState>>(linker, |s| s)
}

/// A WASI 0.2 or 0.3 command component that a shell sandbox can run as an
/// external command.
///
/// Cloning is cheap: clones share the compiled component, so registering one
/// multicall binary under several names (see
/// [`SandboxBuilder::with_commands`](crate::SandboxBuilder::with_commands))
/// compiles it once.
#[derive(Clone)]
pub struct Command {
    pre: Pre,
    /// Whether its children's `CONNECT`s are blind tunnels.
    tunnels: bool,
    /// Whether its children may run commands themselves.
    spawns: bool,
    /// Host memory its instances' metadata takes, charged to the children's
    /// memory pool while one runs (see `vmctx`).
    vmctx: usize,
    /// Its network, if it's been narrowed from the execution's: at
    /// registration (see [`Command::with_network`]), or for one execution
    /// (see [`CommandOptions::with_network`]), which replaces that.
    network: Option<crate::net::ChildNetwork>,
    /// The credentials its requests get in this execution (see
    /// [`Credential`](crate::net::Credential)).
    credentials: Vec<Arc<crate::net::Credential>>,
}

/// Per-execution options for one registered command, which can only take
/// away from what it has: see
/// [`ExecuteOptions::with_command_options`](crate::ExecuteOptions::with_command_options).
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct CommandOptions {
    network: Option<crate::net::ChildNetwork>,
    no_tunnels: bool,
    no_spawn: bool,
    no_credentials: bool,
}

impl CommandOptions {
    /// No changes: the command as registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Give this command's children `network` instead of the execution's
    /// child network, within it: [`ChildNetwork::None`](crate::ChildNetwork::None)
    /// takes their network away, [`Inherit`](crate::ChildNetwork::Inherit)
    /// keeps the execution's, and [`Narrow`](crate::ChildNetwork::Narrow)
    /// allows only what both it and the execution's allow. Their connections
    /// and bytes count against the narrowed config's limits and the
    /// execution's. An execution with no child network can't give one
    /// command any: the execution fails to start.
    #[must_use]
    pub fn with_network(mut self, network: crate::net::ChildNetwork) -> Self {
        self.network = Some(network);
        self
    }

    /// No blind tunnels for this command's children in this execution, even
    /// if it was registered with [`Command::allow_tunnels`].
    #[must_use]
    pub fn without_tunnels(mut self) -> Self {
        self.no_tunnels = true;
        self
    }

    /// Don't let this command run commands in this execution, even if it was
    /// registered with [`Command::allow_spawn`].
    #[must_use]
    pub fn without_spawn(mut self) -> Self {
        self.no_spawn = true;
        self
    }

    /// No [`Credential`](crate::net::Credential)s for this command's
    /// requests in this execution, even ones given for every child.
    #[must_use]
    pub fn without_credentials(mut self) -> Self {
        self.no_credentials = true;
        self
    }
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
    /// WASI 0.2 or 0.3 command (it must export `wasi:cli/run` and import only
    /// WASI).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| {
            ErrorKind::Initialization(format!("failed to read command {}: {e}", path.display()))
        })?;
        Self::from_bytes(&bytes)
    }

    /// Compile a command from its `.wasm` bytes: a WASI 0.2 or 0.3 command
    /// component, or a WASI preview 1 command module (built for
    /// `wasm32-wasip1`, say), which is adapted into a 0.2 component first
    /// (see [`to_component`](Self::to_component)).
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes don't compile, or aren't a WASI command.
    pub fn from_bytes(wasm: &[u8]) -> Result<Self, Error> {
        let wasm = preview1::to_component(wasm)?;
        let engine = crate::Executor::child_engine()?;
        let component =
            Component::from_binary(&engine, &wasm).map_err(ErrorKind::wasm_component)?;
        Self::from_component(&engine, &component, vmctx::estimate(&wasm))
    }

    /// `wasm` as a command component: unchanged if it's a component, and
    /// adapted into a WASI 0.2 command component if it's a WASI preview 1
    /// command module, with the `wasi_snapshot_preview1` adapter of the
    /// wasmtime eryx links (as `wasm-tools component new --adapt` does).
    /// [`from_bytes`](Self::from_bytes) does this itself; it's for compiling
    /// a preview 1 command ahead of time, as `eryx-precompile compile` does.
    ///
    /// # Errors
    ///
    /// Returns an error for a core module that isn't a preview 1 command: a
    /// reactor (no `_start`), or one that doesn't import
    /// `wasi_snapshot_preview1`.
    pub fn to_component(wasm: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>, Error> {
        preview1::to_component(wasm)
    }

    /// Inspect a command's `.wasm` bytes before registering it: which WASI
    /// version it runs as, each interface it imports and whether children get
    /// it, what it exports, and why it can't run, if it can't
    /// (`eryx-precompile commands inspect` prints this). It reads the
    /// component without compiling it.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are neither a component nor a WASI
    /// preview 1 command module. One that can't run as a command is reported
    /// in [`CommandInfo::problem`] instead.
    pub fn inspect(wasm: &[u8]) -> Result<CommandInfo, Error> {
        let component = match preview1::to_component(wasm) {
            Ok(component) => component,
            // A preview 1 module eryx can't adapt (a reactor, say) is one it
            // can't run, not one it can't inspect.
            Err(e) if preview1::is_preview1(wasm) => {
                let why = match e.kind() {
                    ErrorKind::Initialization(why) => why.clone(),
                    _ => e.to_string(),
                };
                return Ok(CommandInfo::unadaptable(why));
            }
            Err(e) => return Err(e),
        };
        let mut info = imports::inspect(&component)?;
        info.preview1 = matches!(component, std::borrow::Cow::Owned(_));
        Ok(info)
    }

    /// Stamp `precompiled`, the output of
    /// [`Executor::precompile`](crate::Executor::precompile) (or its
    /// variants) for `wasm`, with what its instances need of the host's
    /// memory, so a sandbox can charge each running child for it.
    /// `eryx-precompile compile` does this for commands.
    ///
    /// A precompiled command without the stamp is charged 4 MiB per child,
    /// usually enough (gh, the largest measured, needs 1.3 MB), but it fits
    /// fewer children in their memory pool.
    pub fn stamp_precompiled(precompiled: &mut Vec<u8>, wasm: &[u8]) {
        vmctx::stamp(precompiled, vmctx::estimate(wasm));
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
    /// precompiled it, and an error if it isn't a WASI 0.2 or 0.3 command.
    #[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
    #[allow(unsafe_code)]
    pub unsafe fn from_precompiled(precompiled: &[u8]) -> Result<Self, Error> {
        let engine = crate::Executor::child_engine()?;
        // SAFETY: the caller guarantees the bytes are trusted eryx output.
        let component = unsafe { Component::deserialize(&engine, precompiled) }
            .map_err(crate::wasm::deserialize_error)?;
        Self::from_component(&engine, &component, precompiled_vmctx(precompiled))
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
    #[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
    #[allow(unsafe_code)]
    pub unsafe fn from_precompiled_file(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let engine = crate::Executor::child_engine()?;
        // SAFETY: the caller guarantees the file is trusted and unchanging.
        let component = unsafe { Component::deserialize_file(&engine, path) }
            .map_err(crate::wasm::deserialize_error)?;
        let vmctx = precompiled_vmctx(&file_tail(path).unwrap_or_default());
        Self::from_component(&engine, &component, vmctx)
    }

    fn from_component(engine: &Engine, component: &Component, vmctx: u64) -> Result<Self, Error> {
        #[cfg(test)]
        tests::COMPILED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pre = Pre::new(child_linker(engine)?, engine, component)?;
        Ok(Self {
            pre,
            tunnels: false,
            spawns: false,
            vmctx: usize::try_from(vmctx).unwrap_or(usize::MAX),
            network: None,
            credentials: Vec::new(),
        })
    }

    /// Let this command's children open blind `CONNECT` tunnels through the
    /// child proxy, for protocols the proxy can't inspect (e.g. git over
    /// SSH-in-TLS, or a client pinning certificates), when the sandbox gives
    /// children network (see
    /// [`SandboxBuilder::with_child_network`](crate::SandboxBuilder::with_child_network)).
    ///
    /// A tunnel skips everything the proxy does with requests: **no** secret
    /// substitution (placeholders go out as they are), **no** HTTP rules (a
    /// host with rules is refused a tunnel), **no** scrubbing of responses,
    /// and nothing is checked beyond the `CONNECT` host and the addresses it
    /// resolves to, so the child can reach other virtual hosts served from
    /// the same address (e.g. behind a CDN). Every `CONNECT` from such a
    /// child is a tunnel.
    #[must_use]
    pub fn allow_tunnels(mut self) -> Self {
        self.tunnels = true;
        self
    }

    /// Give this command's children `network` instead of the sandbox's
    /// child network (see
    /// [`SandboxBuilder::with_child_network`](crate::SandboxBuilder::with_child_network)),
    /// within it, in every execution:
    /// [`ChildNetwork::None`](crate::ChildNetwork::None) takes their network
    /// away, [`Inherit`](crate::ChildNetwork::Inherit) keeps the sandbox's,
    /// and [`Narrow`](crate::ChildNetwork::Narrow) allows only what both it
    /// and the sandbox's allow, with its limits counted per name the command
    /// is registered under, on top of the execution's.
    ///
    /// An execution's [`CommandOptions::with_network`] replaces it for that
    /// execution, within the sandbox's child network too.
    ///
    /// [`build`](crate::SandboxBuilder::build) fails if the sandbox gives
    /// children no network and this gives the command some, or if a
    /// [`Narrow`](crate::ChildNetwork::Narrow) config is invalid (see
    /// [`NetConfig::validate`](crate::NetConfig::validate)). A pool lease
    /// without a network (see
    /// [`SandboxBuilder::with_network_per_lease`](crate::SandboxBuilder::with_network_per_lease))
    /// leaves the command's children offline, whatever this says.
    #[must_use]
    pub fn with_network(mut self, network: crate::net::ChildNetwork) -> Self {
        self.network = Some(network);
        self
    }

    /// Let this command's children run commands themselves, through
    /// `eryx:process/run`, as git runs `git` for `stash` or `worktree add`,
    /// or `eryx:process/pipe`, with live pipes, as git runs its
    /// `remote-https` helper.
    ///
    /// They resolve names against the same commands, see the same filesystem
    /// and get the same child network as the shell's children, and share
    /// their execution's limits: its fuel, child memory, live children and
    /// host memory. Children nest at most 4 deep (the shell's are the
    /// first), and each nested run's stdout and stderr are capped at 16 MiB
    /// (a piped command's flow through pipes instead).
    /// A command spawns only if it was registered with this too.
    #[must_use]
    pub fn allow_spawn(mut self) -> Self {
        self.spawns = true;
        self
    }
}

/// What a precompiled command's instances need of the host's memory, as
/// [`Command::stamp_precompiled`] recorded it.
#[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
fn precompiled_vmctx(precompiled: &[u8]) -> u64 {
    vmctx::stamped(precompiled).unwrap_or(vmctx::UNSTAMPED)
}

/// The last bytes of `path`, where a stamp would be.
#[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
fn file_tail(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(vmctx::STAMP_LEN as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    Ok(tail)
}

/// An execution's network for its children: the sandbox's policy, secrets
/// and budget (shared with the guest's connections), and the proxies started
/// for them, one inspecting and one tunnelling per network (the execution's,
/// and each command's narrowed one). Nested children use the same one. They
/// live as long as the execution (see [`ExecutorState::disconnect`]), well
/// inside their CAs' validity.
pub(crate) struct ChildNet {
    net: crate::net::ChildNetPolicy,
    /// Commands' narrowed networks (see [`CommandOptions::with_network`]),
    /// by command name, made on first use.
    narrowed: HashMap<String, crate::net::ChildNetPolicy>,
    /// Started proxies, by the command whose narrowed network or credentials
    /// they serve (`None` for the execution's) and whether they tunnel.
    proxies: HashMap<(Option<String>, bool), ProxyLink>,
    /// TCP sockets its children hold, together.
    sockets: Arc<AtomicUsize>,
}

/// A started proxy, and the host directory holding its CA certificate for
/// children to mount read-only at [`CA_DIR`].
struct ProxyLink {
    proxy: crate::net::ChildProxy,
    ca_dir: tempfile::TempDir,
}

impl ChildNet {
    /// From [`ConnectionManager::child_network`](crate::ConnectionManager::child_network).
    pub(crate) fn new(net: crate::net::ChildNetPolicy) -> Self {
        Self {
            net,
            narrowed: HashMap::new(),
            proxies: HashMap::new(),
            sockets: Arc::default(),
        }
    }

    /// The proxy for a child of the command `name` (`command`) that may or
    /// may not open blind tunnels, started on first use; `None` if the
    /// command's network is [`ChildNetwork::None`](crate::ChildNetwork::None).
    fn link(&mut self, name: &str, command: &Command) -> std::io::Result<Option<ChildLink>> {
        use crate::net::ChildNetwork;
        // A command with credentials gets a proxy of its own, so no other
        // command's children can use them.
        let own = (!command.credentials.is_empty()).then(|| name.to_owned());
        let (key, policy) = match &command.network {
            None | Some(ChildNetwork::Inherit) => (own, &self.net),
            Some(ChildNetwork::None) => return Ok(None),
            Some(ChildNetwork::Narrow(config)) => {
                let policy = match self.narrowed.entry(name.to_owned()) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        // Checked at build and per execution already, so
                        // this is only the policy's own guarantee.
                        let narrowed = self.net.narrowed((**config).clone()).map_err(|e| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                format!("command {name}'s network: {e}"),
                            )
                        })?;
                        entry.insert(narrowed)
                    }
                };
                (Some(name.to_owned()), &*policy)
            }
        };
        let link = match self.proxies.entry((key, command.tunnels)) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let proxy = policy.start_proxy_with(command.tunnels, &command.credentials, name)?;
                let ca_dir = tempfile::tempdir()?;
                std::fs::write(ca_dir.path().join(CA_FILE), proxy.ca_pem())?;
                entry.insert(ProxyLink { proxy, ca_dir })
            }
        };
        Ok(Some(ChildLink {
            proxy: link.proxy.addr(),
            proxy_auth: link.proxy.userinfo().map(str::to_owned),
            ca_dir: link.ca_dir.path().to_path_buf(),
            sockets: Arc::clone(&self.sockets),
        }))
    }
}

/// What a child needs to reach its proxy.
#[derive(Clone)]
struct ChildLink {
    proxy: std::net::SocketAddr,
    /// `user:password` the proxy requires, if it holds credentials.
    proxy_auth: Option<String>,
    ca_dir: std::path::PathBuf,
    /// Its execution's count of children's sockets.
    sockets: Arc<AtomicUsize>,
}

impl ChildLink {
    /// The env vars pointing a child's HTTP clients at the proxy and its CA.
    fn env(&self) -> Vec<(String, String)> {
        let proxy = match &self.proxy_auth {
            Some(auth) => format!("http://{auth}@{}", self.proxy),
            None => format!("http://{}", self.proxy),
        };
        let ca = format!("{CA_DIR}/{CA_FILE}");
        let named =
            |names: [&str; 4], value: String| names.map(|name| (name.to_owned(), value.clone()));
        named(PROXY_ENV, proxy)
            .into_iter()
            .chain(named(CA_ENV, ca))
            .collect()
    }
}

/// What a child needs to run commands of its own (see
/// [`Command::allow_spawn`]): its execution's commands, budget and network.
#[derive(Clone)]
struct Spawner {
    commands: Commands,
    budget: ChildBudget,
    /// Weak, so a child left unwaited can't keep the execution's proxies
    /// (and its secrets) alive after it ends.
    net: Option<std::sync::Weak<std::sync::Mutex<ChildNet>>>,
    /// How deep the children it spawns are (the shell's are 1).
    depth: usize,
    /// Where its children's process events go, for the shell's `wait` to
    /// report: shared by every descendant of one of the shell's children.
    traces: Option<Arc<std::sync::Mutex<NestedTraces>>>,
}

/// Nested children's process events, queued for the `wait` of the shell's
/// child they descend from: only it can reach the execution's trace.
#[derive(Default)]
struct NestedTraces {
    next: u64,
    events: Vec<NestedEvent>,
}

enum NestedEvent {
    Start {
        id: u64,
        argv: Vec<String>,
        at: std::time::Instant,
    },
    End {
        id: u64,
        outcome: Result<i32, String>,
        at: std::time::Instant,
    },
}

impl NestedTraces {
    fn push(traces: &std::sync::Mutex<NestedTraces>, event: NestedEvent) {
        traces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .push(event);
    }
}

/// A nested child's process events: its start when made, its end when
/// [`end`](Self::end)ed, or `killed` if it's dropped first (its caller was
/// killed, or the execution ended, midway).
struct NestedTrace {
    traces: Arc<std::sync::Mutex<NestedTraces>>,
    id: u64,
    ended: bool,
}

impl NestedTrace {
    fn start(traces: Arc<std::sync::Mutex<NestedTraces>>, argv: Vec<String>) -> Self {
        let id = {
            let mut queue = traces
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.next += 1;
            queue.next
        };
        let at = std::time::Instant::now();
        NestedTraces::push(&traces, NestedEvent::Start { id, argv, at });
        Self {
            traces,
            id,
            ended: false,
        }
    }

    fn end(mut self, outcome: Result<i32, String>) {
        self.ended = true;
        NestedTraces::push(
            &self.traces,
            NestedEvent::End {
                id: self.id,
                outcome,
                at: std::time::Instant::now(),
            },
        );
    }
}

impl Drop for NestedTrace {
    fn drop(&mut self) {
        if !self.ended {
            NestedTraces::push(
                &self.traces,
                NestedEvent::End {
                    id: self.id,
                    outcome: Err("killed".to_owned()),
                    at: std::time::Instant::now(),
                },
            );
        }
    }
}

/// A child ready to run, with the parent's ends of its pipes.
struct Prepared {
    run: ChildRun,
    stdin: pipe::Writer,
    stdout: pipe::Reader,
    stderr: pipe::Reader,
}

impl Spawner {
    /// Set up a child running `cmd`: its command, environment, proxy, live
    /// slot and pipes, and its argv and env charged to the budget.
    fn prepare(
        &self,
        cmd: String,
        args: Vec<String>,
        mut env: Vec<(String, String)>,
        cwd: String,
        #[cfg(any(feature = "unstable-vfs", feature = "shell"))] vfs: Option<
            eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>,
        >,
    ) -> Result<Prepared, ProcessError> {
        let command = self
            .commands
            .get(&cmd)
            .ok_or(ProcessError::CommandNotFound)?
            .clone();
        // WASI 0.2 has no working directory; tell the child where it is.
        if !env.iter().any(|(k, _)| k == "PWD") {
            env.push(("PWD".to_owned(), cwd));
        }
        // With network, children get the proxy. (Secrets' placeholders come
        // from the shell, which exports them.)
        let network = match &self.net {
            Some(net) => match net
                .upgrade()
                .ok_or_else(|| std::io::Error::other("the execution has ended"))
                .and_then(|net| {
                    net.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .link(&cmd, &command)
                }) {
                Ok(link) => link,
                Err(e) => {
                    tracing::warn!(error = %e, "starting the child proxy failed");
                    return Err(ProcessError::SpawnFailed);
                }
            },
            None => None,
        };
        let mut argv = vec![cmd];
        argv.extend(args);
        let args_bytes = argv.iter().map(String::len).sum::<usize>()
            + env.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
        let live = (args_bytes <= MAX_ARGS_BYTES)
            .then(|| LiveSlot::take(&self.budget.live))
            .flatten();
        let Some(live) = live else {
            tracing::warn!(
                args_bytes,
                live = self.budget.live.load(Ordering::Relaxed),
                "child spawn over the argument or live-child limit"
            );
            return Err(ProcessError::LimitExceeded);
        };
        let Some(memory) = ChildMemory::reserve(
            Arc::clone(&self.budget.memory),
            CHILD_MEMORY_LIMIT,
            command.vmctx,
        ) else {
            tracing::warn!(
                needs = command.vmctx,
                used = self.budget.memory.load(Ordering::Relaxed),
                "no room in the child memory pool for a child's instance metadata"
            );
            return Err(ProcessError::LimitExceeded);
        };
        let mut args = Held {
            budget: self.budget.clone(),
            bytes: 0,
        };
        args.charge(args_bytes as u64)?;

        let (stdin_tx, stdin_rx) = pipe::pipe(PIPE_CAPACITY);
        let (stdout_tx, stdout_rx) = pipe::pipe(PIPE_CAPACITY);
        let (stderr_tx, stderr_rx) = pipe::pipe(PIPE_CAPACITY);
        let run = ChildRun {
            pre: command.pre,
            argv,
            env,
            stdin: stdin_rx,
            stdout: stdout_tx,
            stderr: stderr_tx,
            fuel: 0,
            fuel_used: Arc::default(),
            memory,
            _live: Some(live),
            _args: args,
            network,
            spawner: command.spawns.then(|| Spawner {
                depth: self.depth + 1,
                traces: Some(self.traces.clone().unwrap_or_default()),
                ..self.clone()
            }),
            #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
            vfs,
        };
        Ok(Prepared {
            run,
            stdin: stdin_tx,
            stdout: stdout_rx,
            stderr: stderr_rx,
        })
    }
}

/// A host directory an isolated command sees, at `guest`.
#[cfg(feature = "env-pack")]
pub(crate) struct Mount<'a> {
    pub(crate) host: &'a Path,
    pub(crate) guest: &'a str,
    pub(crate) writable: bool,
}

/// What an isolated command may use.
#[cfg(feature = "env-pack")]
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

#[cfg(feature = "env-pack")]
impl Command {
    /// Load a precompiled command to run with
    /// [`run_isolated`](Self::run_isolated): it's linked with plain WASI 0.2
    /// (a WASI 0.3 command fails to link), so its filesystem is the host
    /// directories it's given, never a sandbox's VFS (which `unstable-vfs`
    /// and `shell` put in place of WASI's filesystem for children, empty for
    /// one without a parent).
    ///
    /// # Safety
    ///
    /// As for [`from_precompiled`](Self::from_precompiled): only bytes eryx
    /// precompiled and you trust.
    #[allow(unsafe_code)]
    pub(crate) unsafe fn from_precompiled_isolated(precompiled: &[u8]) -> Result<Self, Error> {
        let engine = crate::Executor::child_engine()?;
        // SAFETY: the caller guarantees the bytes are trusted eryx output.
        let component = unsafe { Component::deserialize(&engine, precompiled) }
            .map_err(crate::wasm::deserialize_error)?;
        let pre = Pre::new(isolated_linker(&engine)?, &engine, &component)?;
        Ok(Self {
            pre,
            tunnels: false,
            spawns: false,
            vmctx: usize::try_from(precompiled_vmctx(precompiled)).unwrap_or(usize::MAX),
            network: None,
            credentials: Vec::new(),
        })
    }

    /// Run the command on its own, outside any sandbox, with `argv`, no
    /// environment variables, empty stdin, no network (no sockets or name
    /// lookups), and only `mounts` as its filesystem, within `limits`.
    /// Returns its exit code, stdout and stderr. Load it with
    /// [`from_precompiled_isolated`](Self::from_precompiled_isolated), or
    /// with `unstable-vfs` or `shell` it sees an empty filesystem instead of
    /// `mounts`.
    pub(crate) async fn run_isolated(
        &self,
        argv: &[String],
        mounts: &[Mount<'_>],
        limits: &IsolatedLimits,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), Error> {
        use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

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
                // A pool of its own: an isolated run shares no execution's.
                limits: ChildMemory::reserve(Arc::default(), limits.memory, self.vmctx)
                    .ok_or_else(|| fail(format!("{} needs more memory than its limit", argv[0])))?,
                // No network: TCP is off.
                sockets: ChildSockets::default(),
                fuel: limits.fuel,
                fuel_used: Arc::default(),
                // Isolated commands run nothing else.
                spawner: None,
                stderr: None,
                piped: piped::Runs::default(),
                #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
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
            match self.pre.run(&mut store).await {
                Err(e) => Err(fail(format!("instantiating {}: {e}", argv[0]))),
                Ok(Err(e)) => Err(fail(format!("{} failed: {e}", argv[0]))),
                Ok(Ok(code)) => Ok(code),
            }
        };
        let code = tokio::time::timeout(limits.timeout, run)
            .await
            .map_err(|_| fail(format!("{} timed out after {:?}", argv[0], limits.timeout)))??;
        drop(store);
        Ok((code, stdout.contents().to_vec(), stderr.contents().to_vec()))
    }
}

/// A command pre-instantiated for the WASI version it targets.
#[derive(Clone)]
enum Pre {
    P2(wasmtime_wasi::p2::bindings::CommandPre<ChildState>),
    P3(wasmtime_wasi::p3::bindings::CommandPre<ChildState>),
}

impl Pre {
    /// Pre-instantiate `component` with `linker`: as WASI 0.3 if it exports
    /// 0.3's `wasi:cli/run`, else as WASI 0.2.
    fn new(
        linker: &Linker<ChildState>,
        engine: &Engine,
        component: &Component,
    ) -> Result<Self, Error> {
        let p3 = component
            .component_type()
            .exports(engine)
            .any(|(name, _)| name.starts_with("wasi:cli/run@0.3."));
        let pre = linker
            .instantiate_pre(component)
            .map_err(|e| imports::explain(engine, component, e))?;
        Ok(if p3 {
            Self::P3(
                wasmtime_wasi::p3::bindings::CommandPre::new(pre)
                    .map_err(|e| imports::explain(engine, component, e))?,
            )
        } else {
            Self::P2(
                wasmtime_wasi::p2::bindings::CommandPre::new(pre)
                    .map_err(|e| imports::explain(engine, component, e))?,
            )
        })
    }

    fn engine(&self) -> &Engine {
        match self {
            Self::P2(pre) => pre.engine(),
            Self::P3(pre) => pre.engine(),
        }
    }

    /// Instantiate the command in `store` and run it. The outer error is a
    /// failed instantiation; the inner one a trap, other than an exit.
    async fn run(&self, store: &mut Store<ChildState>) -> wasmtime::Result<wasmtime::Result<i32>> {
        let ran = match self {
            Self::P2(pre) => {
                let command = pre.instantiate_async(&mut *store).await?;
                command.wasi_cli_run().call_run(&mut *store).await
            }
            Self::P3(pre) => {
                let command = pre.instantiate_async(&mut *store).await?;
                store
                    .run_concurrent(async move |accessor| {
                        command.wasi_cli_run().call_run(accessor).await
                    })
                    .await
                    .and_then(|ran| ran)
            }
        };
        Ok(match ran {
            Ok(Ok(())) => Ok(0),
            Ok(Err(())) => Ok(1),
            Err(e) => match e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                Some(exit) => Ok(exit.0),
                None => Err(e),
            },
        })
    }
}

/// In-guest commands that count as commands for an execution's
/// allow-list: the shell's own `grep`, and `sh`/`bash` (nested shells).
/// `list-commands` names each one the execution allows, and the guest
/// refuses those it doesn't list.
const GUEST_COMMANDS: [&str; 3] = ["grep", "sh", "bash"];

/// The commands a sandbox can spawn, by name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Commands {
    registered: Arc<HashMap<String, Command>>,
    /// Which of [`GUEST_COMMANDS`] this execution allows; `None` allows all.
    guest: Option<Vec<&'static str>>,
}

impl Commands {
    /// The registered commands' names.
    pub(crate) fn registered_names(&self) -> impl Iterator<Item = &str> {
        self.registered.keys().map(String::as_str)
    }

    pub(crate) fn insert(&mut self, name: String, command: Command) {
        Arc::make_mut(&mut self.registered).insert(name, command);
    }

    /// Just the commands named in `allowed`, for one execution; `None`
    /// keeps them all, and `Some` of an empty list leaves none. Names that
    /// aren't registered or [`GUEST_COMMANDS`] are ignored.
    pub(crate) fn only(&self, allowed: Option<&[String]>) -> Self {
        let Some(allowed) = allowed else {
            return self.clone();
        };
        let mut registered = HashMap::new();
        let mut guest = Vec::new();
        for name in allowed {
            if let Some(command) = self.registered.get(name) {
                registered.insert(name.clone(), command.clone());
            } else if let Some(builtin) = GUEST_COMMANDS.iter().find(|b| **b == name) {
                guest.push(*builtin);
            } else {
                tracing::debug!(command = %name, "allowed command isn't registered");
            }
        }
        Self {
            registered: Arc::new(registered),
            guest: Some(guest),
        }
    }

    /// Check the networks commands were registered with (see
    /// [`Command::with_network`]) against the sandbox's child network:
    /// whether it gives children any (`child_net`).
    ///
    /// # Errors
    ///
    /// Fails if a command has network the sandbox doesn't give children, or
    /// an invalid narrowed config.
    pub(crate) fn check_networks(&self, child_net: bool) -> Result<(), Error> {
        use crate::net::ChildNetwork;
        for (name, command) in self.registered.iter() {
            if !child_net
                && matches!(
                    command.network,
                    Some(ChildNetwork::Inherit | ChildNetwork::Narrow(_))
                )
            {
                return Err(ErrorKind::Initialization(format!(
                    "command {name} can't have network: the sandbox gives children none"
                ))
                .into());
            }
            if let Some(ChildNetwork::Narrow(config)) = &command.network {
                config.validate().map_err(|e| {
                    ErrorKind::Initialization(format!("command {name}'s network: {e}"))
                })?;
            }
        }
        Ok(())
    }

    /// The commands one execution gets: just those in `allowed` (see
    /// [`only`](Self::only)), with `options` applied.
    ///
    /// # Errors
    ///
    /// Fails if an option names a command that isn't registered, or gives a
    /// command network when the execution has no child network
    /// (`child_net`), since an option can only take away; or if a credential
    /// is invalid, the execution has no child network, or a credential names
    /// a command that isn't registered, has no network or tunnels (so could
    /// never use it).
    pub(crate) fn for_execution(
        &self,
        allowed: Option<&[String]>,
        options: &HashMap<String, CommandOptions>,
        child_net: bool,
        credentials: &[Arc<crate::net::Credential>],
    ) -> Result<Self, Error> {
        use crate::net::ChildNetwork;
        let mut commands = self.only(allowed);
        for (name, option) in options {
            if !self.registered.contains_key(name) {
                return Err(ErrorKind::Initialization(format!(
                    "command options for {name}, which isn't a registered command"
                ))
                .into());
            }
            if !child_net
                && matches!(
                    option.network,
                    Some(ChildNetwork::Inherit | ChildNetwork::Narrow(_))
                )
            {
                return Err(ErrorKind::Initialization(format!(
                    "command {name} can't have network: the execution gives children none"
                ))
                .into());
            }
            if let Some(ChildNetwork::Narrow(config)) = &option.network {
                config.validate().map_err(|e| {
                    ErrorKind::Initialization(format!("command {name}'s network: {e}"))
                })?;
            }
            let registered = Arc::make_mut(&mut commands.registered);
            if let Some(command) = registered.get_mut(name) {
                command.tunnels &= !option.no_tunnels;
                command.spawns &= !option.no_spawn;
                if option.network.is_some() {
                    command.network.clone_from(&option.network);
                }
            }
        }
        self.attach_credentials(&mut commands, options, child_net, credentials)?;
        Ok(commands)
    }

    /// The registered commands one execution (`allowed`, `options`, as for
    /// [`Self::for_execution`]) may give one of `credentials` to, and so a
    /// proxy with a password: those it allows that have a network and that
    /// a credential is for.
    pub(crate) fn credentialed_names(
        &self,
        allowed: Option<&[String]>,
        options: &HashMap<String, CommandOptions>,
        credentials: &[&crate::net::Credential],
    ) -> Vec<String> {
        use crate::net::ChildNetwork;
        if credentials.is_empty() {
            return Vec::new();
        }
        self.only(allowed)
            .registered
            .iter()
            .filter(|(name, command)| {
                let option = options.get(*name);
                let network = option
                    .and_then(|o| o.network.as_ref())
                    .or(command.network.as_ref());
                !option.is_some_and(|o| o.no_credentials)
                    && !matches!(network, Some(ChildNetwork::None))
                    && credentials.iter().any(|c| c.for_command(name))
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Give each of `commands` the `credentials` that are for it, unless its
    /// options take them away; see [`Self::for_execution`] for the errors.
    fn attach_credentials(
        &self,
        commands: &mut Self,
        options: &HashMap<String, CommandOptions>,
        child_net: bool,
        credentials: &[Arc<crate::net::Credential>],
    ) -> Result<(), Error> {
        use crate::net::ChildNetwork;
        let fail = |message: String| Err(ErrorKind::Initialization(message).into());
        if credentials.is_empty() {
            return Ok(());
        }
        if !child_net {
            return fail(
                "credentials need a child network: the execution gives children none".into(),
            );
        }
        for credential in credentials {
            credential.validate().map_err(ErrorKind::Initialization)?;
            for name in credential.commands().unwrap_or_default() {
                if !self.registered.contains_key(name) {
                    return fail(format!(
                        "a credential for {name}, which isn't a registered command"
                    ));
                }
            }
        }
        let registered = Arc::make_mut(&mut commands.registered);
        for (name, command) in registered.iter_mut() {
            if options.get(name).is_some_and(|o| o.no_credentials) {
                continue;
            }
            let named = || {
                credentials
                    .iter()
                    .any(|c| c.commands().is_some_and(|cs| cs.contains(name)))
            };
            if matches!(command.network, Some(ChildNetwork::None)) {
                if named() {
                    return fail(format!(
                        "command {name} has a credential but no network in this execution"
                    ));
                }
                continue;
            }
            if command.tunnels && named() {
                return fail(format!(
                    "command {name} has a credential but opens blind tunnels, which can't \
                     carry one: take its tunnels away with CommandOptions::without_tunnels"
                ));
            }
            command.credentials = credentials
                .iter()
                .filter(|c| c.for_command(name))
                .cloned()
                .collect();
        }
        Ok(())
    }

    /// What `list-commands` reports: the registered commands, and the
    /// guest's own the execution allows.
    fn names(&self) -> Vec<String> {
        let guest = GUEST_COMMANDS
            .iter()
            .filter(|b| {
                self.guest
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(b))
            })
            .filter(|b| !self.registered.contains_key(**b));
        self.registered
            .keys()
            .cloned()
            .chain(guest.map(|b| (*b).to_owned()))
            .collect()
    }

    /// The registered command `name`, if this execution may spawn it.
    fn get(&self, name: &str) -> Option<&Command> {
        self.registered.get(name)
    }

    /// Register `command` as `name` unless something already is.
    #[cfg(all(feature = "embedded", feature = "coreutils"))]
    pub(crate) fn insert_if_absent(&mut self, name: &str, command: &Command) {
        if !self.registered.contains_key(name) {
            Arc::make_mut(&mut self.registered).insert(name.to_owned(), command.clone());
        }
    }
}

/// The linker children are instantiated with: WASI 0.2 and 0.3, plus the
/// hybrid VFS with `unstable-vfs` or `shell`. Built once, for the child engine.
fn child_linker(engine: &Engine) -> Result<&'static Linker<ChildState>, Error> {
    static LINKER: OnceLock<Linker<ChildState>> = OnceLock::new();
    if let Some(linker) = LINKER.get() {
        return Ok(linker);
    }
    let wasm_err = |e: wasmtime::Error| ErrorKind::WasmEngine(e.to_string());
    let mut linker = Linker::<ChildState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(wasm_err)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker).map_err(wasm_err)?;
    linker.allow_shadowing(true);
    // Count children's sockets (see ChildSockets).
    wasmtime_wasi::p2::bindings::sockets::tcp_create_socket::add_to_linker::<_, CountedSockets>(
        &mut linker,
        |state: &mut ChildState| CountedSocketsView {
            sockets: wasmtime_wasi::sockets::WasiSocketsCtxView {
                ctx: state.wasi.sockets(),
                table: &mut state.table,
            },
            budget: &mut state.sockets,
        },
    )
    .map_err(wasm_err)?;
    count_p3_sockets(&mut linker).map_err(wasm_err)?;
    add_run_to_linker(&mut linker).map_err(wasm_err)?;
    piped::add_to_linker(&mut linker).map_err(wasm_err)?;
    #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
    {
        eryx_vfs::add_hybrid_vfs_to_linker(&mut linker)
            .map_err(|e| ErrorKind::WasmEngine(e.to_string()))?;
        eryx_vfs::add_hybrid_vfs_to_linker_p3(&mut linker)
            .map_err(|e| ErrorKind::WasmEngine(e.to_string()))?;
    }
    linker.allow_shadowing(false);
    Ok(LINKER.get_or_init(|| linker))
}

/// The linker isolated commands are instantiated with: WASI 0.2 only.
#[cfg(feature = "env-pack")]
fn isolated_linker(engine: &Engine) -> Result<&'static Linker<ChildState>, Error> {
    static LINKER: OnceLock<Linker<ChildState>> = OnceLock::new();
    if let Some(linker) = LINKER.get() {
        return Ok(linker);
    }
    let mut linker = Linker::<ChildState>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|e| ErrorKind::WasmEngine(e.to_string()))?;
    Ok(LINKER.get_or_init(|| linker))
}

/// A child's store data.
pub(crate) struct ChildState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: ChildMemory,
    sockets: ChildSockets,
    /// Fuel it may burn, and has burned as of its last epoch tick or
    /// nested run (which charges it its own children's).
    fuel: u64,
    fuel_used: Arc<AtomicU64>,
    /// How it runs commands of its own, if its command may.
    spawner: Option<Spawner>,
    /// Its stderr, which its piped children's stderr goes to (see
    /// [`piped`]); `None` when it's captured instead.
    stderr: Option<pipe::Writer>,
    /// Its piped commands (see [`piped`]), run by its pipe calls.
    piped: piped::Runs,
    #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

/// A child's share of `pool`, which may grow to `limit` (its execution's
/// `CHILD_MEMORY_LIMIT`), given back when its store is dropped.
struct ChildMemory {
    pool: Arc<AtomicUsize>,
    held: usize,
    limit: usize,
}

impl ChildMemory {
    /// A share of `pool` holding `bytes` already (for the child's instance
    /// metadata), or `None` if that doesn't fit `limit`.
    fn reserve(pool: Arc<AtomicUsize>, limit: usize, bytes: usize) -> Option<Self> {
        pool.try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes).filter(|&u| u <= limit)
        })
        .ok()?;
        Some(Self {
            pool,
            held: bytes,
            limit,
        })
    }
}

impl ChildMemory {
    /// Take `bytes` more of the pool, if they fit.
    fn charge(&mut self, bytes: usize) -> bool {
        let reserved = self
            .pool
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&u| u <= self.limit)
            })
            .is_ok();
        if reserved {
            self.held += bytes;
        }
        reserved
    }
}

impl ResourceLimiter for ChildMemory {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // A refused grow fails `memory.grow`, so the child sees an ordinary
        // allocation failure.
        Ok(self.charge(desired.saturating_sub(current)))
    }

    /// Table elements are host memory too: a pointer each. Children run on
    /// the on-demand child engine, where nothing else bounds a table (the
    /// pooling allocator's `table_elements` doesn't apply), so they're charged
    /// to the same pool, initial size included. A refused grow fails
    /// `table.grow` (returns -1).
    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let elements = desired.saturating_sub(current);
        Ok(self.charge(elements.saturating_mul(std::mem::size_of::<usize>())))
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

#[cfg(any(feature = "unstable-vfs", feature = "shell"))]
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
}

/// What `wait` runs a child with, taken out of its handle.
struct ChildRun {
    pre: Pre,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    /// The child's ends of its stdio pipes.
    stdin: pipe::Reader,
    stdout: pipe::Writer,
    stderr: pipe::Writer,
    /// Fuel the child may burn.
    fuel: u64,
    /// Fuel it has burned so far, as of its last epoch tick.
    fuel_used: Arc<AtomicU64>,
    /// Its share of the execution's pool of child memory, holding its
    /// instances' metadata from spawn.
    memory: ChildMemory,
    /// Its live slot, given back when the run ends or is dropped (a piped
    /// command's handle holds it instead, until it's dropped; see
    /// [`piped`]).
    _live: Option<LiveSlot>,
    /// Its argv and env, charged to the execution's budget for as long as
    /// the run (which holds copies of them) lasts.
    _args: Held,
    /// Its proxy, if children have network.
    network: Option<ChildLink>,
    /// How it runs commands of its own, if its command may.
    spawner: Option<Spawner>,
    #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

impl ChildRun {
    /// Run the child to completion, returning its exit code and the fuel it
    /// burned.
    async fn run(self) -> (Result<i32, String>, u64) {
        // With a proxy, its env vars replace any of the same names the script
        // passed (libc takes a name's first value).
        let env: Vec<(String, String)> = match &self.network {
            Some(link) => self
                .env
                .iter()
                .filter(|(k, _)| !PROXY_ENV.contains(&k.as_str()) && !CA_ENV.contains(&k.as_str()))
                .cloned()
                .chain(link.env())
                .collect(),
            None => self.env.clone(),
        };
        let mut wasi = WasiCtxBuilder::new();
        wasi.args(&self.argv)
            .envs(&env)
            .stdin(self.stdin)
            .stdout(self.stdout)
            .stderr(self.stderr.clone())
            // The proxy resolves names; children reach it by address.
            .allow_ip_name_lookup(false)
            .allow_udp(false);
        // Report the shell's directory through `wasi:cli/environment`'s
        // initial-cwd too. wasi-libc ignores it (WebAssembly/wasi-libc#915);
        // other guest runtimes may not.
        if let Some((_, pwd)) = env.iter().find(|(k, _)| k == "PWD") {
            wasi.initial_cwd(pwd);
        }
        #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
        let mut vfs = self.vfs;
        // Without a proxy TCP stays off (wasmtime-wasi's default) and every
        // address is refused, so the child is offline. With one, it may only
        // connect to the proxy.
        if let Some(link) = &self.network {
            let proxy = link.proxy;
            wasi.allow_tcp(true).socket_addr_check(move |addr, use_| {
                use wasmtime_wasi::sockets::SocketAddrUse;
                let ok = match use_ {
                    SocketAddrUse::TcpConnect => addr == proxy,
                    // A connect checks the implicit bind to an ephemeral
                    // port first; nothing else may bind.
                    SocketAddrUse::TcpBind => addr.ip().is_unspecified() && addr.port() == 0,
                    _ => false,
                };
                Box::pin(async move { ok })
            });
            // The CA certificate only, read-only, at CA_DIR.
            #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
            let mounted = match vfs.as_mut() {
                Some(vfs) => vfs.add_real_preopen_path(
                    CA_DIR,
                    &link.ca_dir,
                    eryx_vfs::DirPerms::READ,
                    eryx_vfs::FilePerms::READ,
                ),
                None => wasi
                    .preopened_dir(&link.ca_dir, CA_DIR, wasmtime_wasi::FsPerms::ReadOnly)
                    .map(|_| ())
                    .map_err(std::io::Error::other),
            };
            #[cfg(not(any(feature = "unstable-vfs", feature = "shell")))]
            let mounted = wasi
                .preopened_dir(&link.ca_dir, CA_DIR, wasmtime_wasi::FsPerms::ReadOnly)
                .map(|_| ())
                .map_err(std::io::Error::other);
            if let Err(e) = mounted {
                return (Err(format!("mounting the proxy CA: {e}")), 0);
            }
        }
        let wasi = wasi.build();
        let mut store = Store::new(
            self.pre.engine(),
            ChildState {
                wasi,
                table: ResourceTable::new(),
                limits: self.memory,
                sockets: ChildSockets::new(
                    self.network
                        .as_ref()
                        .map(|link| Arc::clone(&link.sockets))
                        .unwrap_or_default(),
                ),
                fuel: self.fuel,
                fuel_used: Arc::clone(&self.fuel_used),
                spawner: self.spawner,
                stderr: Some(self.stderr),
                piped: piped::Runs::default(),
                #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
                vfs,
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
            match self.pre.run(&mut store).await {
                Err(e) => Err(format!("instantiate: {e}")),
                Ok(Err(e)) => Err(format!("{}: {e}", self.argv[0])),
                Ok(Ok(code)) => Ok(code),
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
struct PipeConsumer(pipe::Writer);

impl<D> StreamConsumer<D> for PipeConsumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
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
        match self.0.poll_write(cx, buf) {
            Poll::Ready(Some(n)) => {
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
struct PipeProducer(pipe::Reader);

impl<D> StreamProducer<D> for PipeProducer {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
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
        match self.0.poll_read(cx, dst.remaining()) {
            // End of file: the child exited, or closed the stream.
            Poll::Ready(0) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Ready(n) => {
                dst.mark_written(n);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl ExecutorState {
    /// How the guest spawns children: the shell's are depth 1.
    fn spawner(&self) -> Spawner {
        Spawner {
            commands: self.commands.clone(),
            budget: self.child_budget.clone(),
            net: self.child_net.as_ref().map(Arc::downgrade),
            depth: 1,
            traces: None,
        }
    }

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
        let mut run = child.run.take().ok_or(ProcessError::IoError)?;
        run.fuel = fuel.min(CHILD_FUEL);
        Ok(run)
    }
}

impl Host for ExecutorState {
    async fn list_commands(&mut self) -> Vec<String> {
        self.commands.names()
    }
}

impl HostChild for ExecutorState {
    async fn drop(&mut self, child: Resource<ChildProcess>) -> wasmtime::Result<()> {
        // A run not yet waited on gives back its budget as it's dropped.
        self.table.delete(child)?;
        Ok(())
    }
}

impl<T: Send> HostChildWithStore<T> for HasSelf<ExecutorState> {
    fn spawn(
        mut host: Access<'_, T, Self>,
        cmd: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: String,
        stdin: StreamReader<u8>,
    ) -> Result<Spawned, ProcessError> {
        let state = host.get();
        if state.suspended.is_some() {
            return Err(ProcessError::SpawnFailed);
        }
        let Prepared {
            run,
            stdin: stdin_tx,
            stdout: stdout_rx,
            stderr: stderr_rx,
        } = state.spawner().prepare(
            cmd,
            args,
            env,
            cwd,
            #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
            state
                .hybrid_vfs_ctx
                .as_ref()
                .map(eryx_vfs::HybridVfsCtx::fork),
        )?;
        let child = ChildProcess { run: Some(run) };
        let spawned = (|| -> wasmtime::Result<Spawned> {
            stdin.pipe(&mut host, PipeConsumer(stdin_tx))?;
            let stdout = StreamReader::new(&mut host, PipeProducer(stdout_rx))?;
            let stderr = StreamReader::new(&mut host, PipeProducer(stderr_rx))?;
            let child = host.get().table.push(child)?;
            Ok(Spawned {
                child,
                stdout,
                stderr,
            })
        })();
        spawned.map_err(|e| {
            tracing::warn!(error = %e, "connecting a child's stdio failed");
            ProcessError::SpawnFailed
        })
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
        let trace = accessor.with(|mut access| access.get().trace_process_start(&run.argv));
        // Its descendants' process events, reported as they come.
        let nested = run.spawner.as_ref().and_then(|s| s.traces.clone());
        let mut reported = HashMap::new();
        let mut report_nested = || {
            let Some(nested) = &nested else { return };
            let events = std::mem::take(
                &mut nested
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .events,
            );
            if events.is_empty() {
                return;
            }
            accessor.with(|mut access| {
                let state = access.get();
                for event in events {
                    match event {
                        NestedEvent::Start { id, argv, at } => {
                            if let Some(trace) = state.trace_process_start_at(&argv, at) {
                                reported.insert(id, trace);
                            }
                        }
                        NestedEvent::End { id, outcome, at } => {
                            if let Some(trace) = reported.remove(&id) {
                                let outcome = outcome.as_ref().copied().map_err(String::as_str);
                                trace.end_at(state, outcome, at);
                            }
                        }
                    }
                }
            });
        };
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
                // Fuel is always enabled on both engines.
                let _ = store.set_fuel(left);
                left > 0
            })
        };
        // Boxed, so it can be dropped (killing its descendants) before the
        // last report.
        let mut running = Box::pin(run.run());
        let finished = std::future::poll_fn(|cx| {
            let polled = running.as_mut().poll(cx);
            report_nested();
            match polled {
                Poll::Ready(done) => Poll::Ready(Some(done)),
                // The child yielded (an epoch tick) or is waiting on a pipe.
                Poll::Pending if charge(fuel_used.load(Ordering::Relaxed)) => Poll::Pending,
                Poll::Pending => Poll::Ready(None),
            }
        })
        .await;
        // A child killed midway left its descendants' ends queued.
        drop(running);
        report_nested();
        const OUT_OF_FUEL: &str = "the parent ran out of fuel";
        let outcome = match finished {
            Some((code, used)) if charge(used) => code,
            _ => {
                tracing::warn!("parent ran out of fuel; killed its child");
                Err(OUT_OF_FUEL.to_owned())
            }
        };
        if let Err(e) = &outcome
            && e != OUT_OF_FUEL
        {
            tracing::warn!(error = %e, "child command failed");
        }
        if let Some(trace) = trace {
            accessor.with(|mut access| {
                trace.end(
                    access.get(),
                    outcome.as_ref().copied().map_err(String::as_str),
                )
            });
        }
        outcome.map_err(|_| ProcessError::SpawnFailed)
    }
}

/// Link `eryx:process/run` for children (see [`run_nested`]).
fn add_run_to_linker(linker: &mut Linker<ChildState>) -> wasmtime::Result<()> {
    type Params = (String, Vec<String>, Vec<(String, String)>, String, Vec<u8>);
    linker.instance("eryx:process/run@0.1.0")?.func_wrap_async(
        "run",
        |store, (cmd, args, env, cwd, stdin): Params| {
            Box::new(async move {
                let ran = run_nested(store, cmd, args, env, cwd, stdin).await;
                Ok((ran.map_err(|e| match e {
                    ProcessError::CommandNotFound => RunError::CommandNotFound,
                    ProcessError::LimitExceeded => RunError::LimitExceeded,
                    ProcessError::SpawnFailed | ProcessError::IoError => RunError::SpawnFailed,
                }),))
            })
        },
    )
}

/// What a nested run holds of its execution's [`ChildBudget`], given back
/// when it's dropped: when `run` returns, or when its caller is killed
/// midway.
struct Held {
    budget: ChildBudget,
    bytes: u64,
}

impl Held {
    fn charge(&mut self, bytes: u64) -> Result<(), ProcessError> {
        self.budget.charge(bytes)?;
        self.bytes += bytes;
        Ok(())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

/// One of a nested run's output streams, read into memory as it's written.
struct Capture {
    reader: Option<pipe::Reader>,
    data: Vec<u8>,
}

impl Capture {
    /// Read what's there into `data`, charging it to `held`: `Ready(Ok)` at
    /// end of file, an error past `MAX_RUN_OUTPUT` or the budget.
    fn poll(&mut self, cx: &Context<'_>, held: &mut Held) -> Poll<Result<(), ProcessError>> {
        let Some(reader) = &self.reader else {
            return Poll::Ready(Ok(()));
        };
        let mut buf = [0; 16 * 1024];
        loop {
            match reader.poll_read(cx, &mut buf) {
                Poll::Ready(0) => {
                    self.reader = None;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(n) => {
                    if self.data.len() + n > MAX_RUN_OUTPUT {
                        tracing::warn!("nested command's output over the limit");
                        return Poll::Ready(Err(ProcessError::LimitExceeded));
                    }
                    held.charge(n as u64)?;
                    self.data.extend_from_slice(&buf[..n]);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Move the fuel a nested child has burned since the last charge (`used`
/// in all, `charged` of it already moved) to its caller, and publish the
/// caller's total, for its own parent to charge. False once the caller has
/// none left.
fn charge_caller(
    store: &mut StoreContextMut<'_, ChildState>,
    charged: &mut u64,
    used: u64,
) -> bool {
    let delta = used.saturating_sub(*charged);
    *charged = (*charged).max(used);
    let left = store.get_fuel().unwrap_or(0).saturating_sub(delta);
    // Fuel is always enabled on both engines.
    let _ = store.set_fuel(left);
    let state = store.data();
    state
        .fuel_used
        .store(state.fuel.saturating_sub(left), Ordering::Relaxed);
    left > 0
}

/// `eryx:process/run`: run `cmd` to completion for the child whose store
/// this is, as `wait` does for the guest's children.
///
/// The nested child gets the caller's fuel left (at most `CHILD_FUEL`), and
/// what it burns is charged to the caller as it runs, so it rolls up to the
/// execution. Its argv, env, stdin and captured output are held against
/// the execution's [`ChildBudget`] until this returns (or is dropped).
async fn run_nested(
    mut store: StoreContextMut<'_, ChildState>,
    cmd: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: String,
    input: Vec<u8>,
) -> Result<Output, ProcessError> {
    let state = store.data_mut();
    let Some(spawner) = state.spawner.clone() else {
        tracing::warn!(
            cmd,
            "a child whose command may not spawn tried to run a command"
        );
        return Err(ProcessError::SpawnFailed);
    };
    if spawner.depth > MAX_SPAWN_DEPTH {
        tracing::warn!(cmd, depth = spawner.depth, "nested spawns too deep");
        return Err(ProcessError::LimitExceeded);
    }
    let Prepared {
        mut run,
        stdin,
        stdout,
        stderr,
    } = spawner.prepare(
        cmd,
        args,
        env,
        cwd,
        #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
        state.vfs.as_ref().map(eryx_vfs::HybridVfsCtx::fork),
    )?;
    // Its input and output; the run holds its argv and env.
    let mut held = Held {
        budget: spawner.budget,
        bytes: 0,
    };
    held.charge(input.len() as u64)?;
    // Only once it can't be refused, so a refused child reports nothing.
    let trace = spawner
        .traces
        .clone()
        .map(|traces| NestedTrace::start(traces, run.argv.clone()));
    run.fuel = store.get_fuel().unwrap_or(0).min(CHILD_FUEL);
    let fuel_used = Arc::clone(&run.fuel_used);
    let mut running = std::pin::pin!(run.run());
    let mut done = None;
    let mut stdin = Some(stdin);
    let mut fed = 0;
    let mut out = Capture {
        reader: Some(stdout),
        data: Vec::new(),
    };
    let mut err = Capture {
        reader: Some(stderr),
        data: Vec::new(),
    };
    let mut charged_fuel = 0;
    // Move what the child has burned since the last charge to the caller
    // (and publish the caller's total, for its own parent to charge);
    // false once the caller has none left.
    let mut charge = |store: &mut StoreContextMut<'_, ChildState>, used: u64| {
        charge_caller(store, &mut charged_fuel, used)
    };
    let result = std::future::poll_fn(|cx| {
        while let Some(writer) = &stdin {
            if fed == input.len() {
                // Closing its stdin ends the child's input.
                stdin = None;
                break;
            }
            match writer.poll_write(cx, &input[fed..]) {
                Poll::Ready(Some(n)) => fed += n,
                // It exited, or closed its stdin.
                Poll::Ready(None) => stdin = None,
                Poll::Pending => break,
            }
        }
        if let Poll::Ready(Err(e)) = out.poll(cx, &mut held) {
            return Poll::Ready(Err(e));
        }
        if let Poll::Ready(Err(e)) = err.poll(cx, &mut held) {
            return Poll::Ready(Err(e));
        }
        if done.is_none()
            && let Poll::Ready(finished) = running.as_mut().poll(cx)
        {
            done = Some(finished);
        }
        match &done {
            Some(_) if out.reader.is_none() && err.reader.is_none() => Poll::Ready(Ok(done.take())),
            // Output is still draining; the child's exit closed its pipes.
            Some(_) => Poll::Pending,
            None if charge(&mut store, fuel_used.load(Ordering::Relaxed)) => Poll::Pending,
            None => Poll::Ready(Ok(None)),
        }
    })
    .await;
    drop(held);
    // Charge what it burned however it ended, before any error returns.
    let used = match &result {
        Ok(Some((_, used))) => *used,
        _ => fuel_used.load(Ordering::Relaxed),
    };
    let fuel_left = charge(&mut store, used);
    if let Some(trace) = trace {
        trace.end(match &result {
            Ok(Some((code, _))) => code.clone(),
            Ok(None) => Err("its caller ran out of fuel".to_owned()),
            Err(e) => Err(format!("{e:?}")),
        });
    }
    let Some((code, _)) = result? else {
        tracing::warn!("caller ran out of fuel; killed its nested child");
        return Err(ProcessError::SpawnFailed);
    };
    if !fuel_left {
        return Err(ProcessError::SpawnFailed);
    }
    let status = code.map_err(|e| {
        tracing::warn!(error = %e, "nested command failed");
        ProcessError::SpawnFailed
    })?;
    Ok(Output {
        status,
        stdout: out.data,
        stderr: err.data,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// How many commands have been linked into a pre-instance.
    pub(super) static COMPILED: AtomicUsize = AtomicUsize::new(0);

    pub(crate) fn test_cmd_wasm() -> Vec<u8> {
        test_cmd_wasm_for("wasm32-wasip2")
    }

    pub(crate) fn test_cmd_wasm_for(target: &str) -> Vec<u8> {
        let manifest =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-cmd/Cargo.toml");
        let target_dir = std::env::temp_dir().join("eryx-unit-test-cmd");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--quiet", "--release", "--locked"])
            .args(["--target", target, "--manifest-path"])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", &target_dir)
            .status()
            .unwrap();
        assert!(status.success(), "building tests/fixtures/test-cmd failed");
        std::fs::read(target_dir.join(format!("{target}/release/eryx-test-cmd.wasm"))).unwrap()
    }

    /// The socket counter only goes in where wasmtime-wasi linked WASI 0.3's
    /// `tcp-socket.create`, so a version it no longer links fails loudly.
    #[test]
    fn p3_sockets_are_counted_only_where_wasmtime_wasi_links_them() {
        let engine = crate::Executor::child_engine().unwrap();
        let mut linker = Linker::<ChildState>::new(&engine);
        assert!(count_p3_sockets(&mut linker).is_err());
        wasmtime_wasi::p3::add_to_linker(&mut linker).unwrap();
        count_p3_sockets(&mut linker).unwrap();
    }

    /// What a nested run holds of the budget comes back however it ends,
    /// including when it's dropped midway (a timeout, or its caller killed).
    #[test]
    fn nested_runs_give_back_their_budget() {
        let budget = ChildBudget::new(Some(100));
        let mut held = Held {
            budget: budget.clone(),
            bytes: 0,
        };
        held.charge(60).unwrap();
        assert!(held.charge(60).is_err());
        assert_eq!(budget.used.load(Ordering::Relaxed), 60);
        drop(held);
        assert_eq!(budget.used.load(Ordering::Relaxed), 0);
    }

    /// The largest core instance `wasm` creates, as wasmtime sizes it: the
    /// pooling allocator refuses an instance over its limit, saying how many
    /// bytes it needed, so raise the limit to that until every one fits.
    async fn largest_instance(wasm: &[u8]) -> u64 {
        let mut limit = 1;
        loop {
            let mut pooling = wasmtime::PoolingAllocationConfig::new();
            pooling.max_core_instance_size(limit);
            match pooling_refusal(wasm, pooling).await {
                Some(needed) => {
                    assert!(needed > limit);
                    limit = needed;
                }
                None => return limit as u64,
            }
        }
    }

    /// What wasmtime needs for all of `wasm`'s instance metadata, core
    /// instances and `VMComponentContext` together: its pooling limit on a
    /// component instance reports the total.
    async fn component_total(wasm: &[u8]) -> u64 {
        let mut pooling = wasmtime::PoolingAllocationConfig::new();
        pooling.max_core_instance_size(64 << 20);
        pooling.max_component_instance_size(1);
        pooling_refusal(wasm, pooling).await.expect("refused") as u64
    }

    /// The bytes a pooling allocator under `pooling` says `wasm` needs, if it
    /// refuses to compile or instantiate it.
    async fn pooling_refusal(
        wasm: &[u8],
        pooling: wasmtime::PoolingAllocationConfig,
    ) -> Option<usize> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model_async(true);
        config.wasm_component_model_more_async_builtins(true);
        config.allocation_strategy(wasmtime::InstanceAllocationStrategy::Pooling(pooling));
        let engine = Engine::new(&config).unwrap();
        // wasmtime checks when compiling, and again when instantiating.
        let tried = async {
            let component = Component::new(&engine, wasm)?;
            let mut linker = Linker::<()>::new(&engine);
            linker.define_unknown_imports_as_traps(&component)?;
            let mut store = Store::new(&engine, ());
            linker.instantiate_async(&mut store, &component).await
        };
        let e = tried.await.err()?;
        let message = format!("{e:#}");
        let needed = message
            .split_once("requires ")
            .and_then(|(_, rest)| rest.split_once(' '))
            .and_then(|(n, _)| n.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("unexpected error: {message}"));
        Some(needed)
    }

    /// The estimate a command is charged for its instances' metadata covers
    /// what wasmtime allocates for its largest instance, closely.
    #[tokio::test]
    async fn instance_metadata_estimates_match_wasmtime() {
        for target in ["wasm32-wasip2", "wasm32-wasip3"] {
            let wasm = test_cmd_wasm_for(target);
            let actual = largest_instance(&wasm).await;
            let estimated = vmctx::largest(&wasm);
            assert!(
                estimated >= actual && estimated <= actual + actual / 20,
                "{target}: estimated {estimated}, wasmtime {actual}"
            );
            // The pool is charged the total: every core instance (none
            // missed or counted twice) and the component's own context.
            let actual = component_total(&wasm).await;
            let estimated = vmctx::estimate(&wasm);
            assert!(
                estimated >= actual && estimated <= actual + actual / 20 + 16 * 1024,
                "{target}: estimated total {estimated}, wasmtime {actual}"
            );
        }
    }

    /// A precompiled command carries its estimate in its stamp; one without
    /// is charged [`vmctx::UNSTAMPED`]. Commands precompiled on the shared
    /// engine's configuration ([`Executor::precompile`](crate::Executor::precompile))
    /// load on the child engine.
    #[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
    #[test]
    #[allow(unsafe_code)]
    fn precompiled_commands_carry_their_estimate() {
        let wasm = test_cmd_wasm();
        let mut precompiled = crate::Executor::precompile(&wasm).unwrap();
        // SAFETY: built just above.
        let plain = unsafe { Command::from_precompiled(&precompiled) }.unwrap();
        assert_eq!(plain.vmctx as u64, vmctx::UNSTAMPED);
        Command::stamp_precompiled(&mut precompiled, &wasm);
        // SAFETY: as above, with a trailer wasmtime ignores.
        let stamped = unsafe { Command::from_precompiled(&precompiled) }.unwrap();
        assert_eq!(stamped.vmctx as u64, vmctx::estimate(&wasm));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tc.cwasm");
        std::fs::write(&path, &precompiled).unwrap();
        // SAFETY: as above.
        let from_file = unsafe { Command::from_precompiled_file(&path) }.unwrap();
        assert_eq!(from_file.vmctx, stamped.vmctx);
    }

    /// Growing a child's tables is charged to its memory pool, a pointer per
    /// element, and refused once the pool is full; the charge comes back
    /// when the child ends.
    #[test]
    fn table_growth_is_charged_to_the_pool() {
        let pool = Arc::new(AtomicUsize::new(0));
        let ptr = std::mem::size_of::<usize>();
        let mut child = ChildMemory::reserve(Arc::clone(&pool), 100 * ptr, 0).unwrap();
        assert!(child.table_growing(0, 60, None).unwrap());
        assert_eq!(pool.load(Ordering::Relaxed), 60 * ptr);
        assert!(!child.table_growing(60, 120, None).unwrap());
        assert!(child.memory_growing(0, 40 * ptr, None).unwrap());
        assert!(!child.memory_growing(40 * ptr, 41 * ptr, None).unwrap());
        drop(child);
        assert_eq!(pool.load(Ordering::Relaxed), 0);
    }

    /// Commands run on the child engine: with the pooling allocator on the
    /// shared engine, that's another engine, allocating on demand, so
    /// children take none of the sandboxes' pool slots.
    #[test]
    fn commands_run_on_the_on_demand_child_engine() {
        let shared = crate::Executor::shared_engine().unwrap();
        let command = Command::from_bytes(&test_cmd_wasm()).unwrap();
        let engine = command.pre.engine();
        assert!(engine.pooling_allocator_metrics().is_none());
        assert_eq!(
            Engine::same(engine, &shared),
            shared.pooling_allocator_metrics().is_none(),
            "the child engine is the shared one exactly when that is on-demand"
        );
        assert!(Engine::same(
            engine,
            &crate::Executor::child_engine().unwrap()
        ));
    }

    /// The child engine has no per-instance limit of its own, so the charge
    /// for instance metadata is what bounds a child there: one whose estimate
    /// doesn't fit the child memory pool isn't started.
    #[cfg(all(feature = "embedded", feature = "shell"))]
    #[tokio::test]
    async fn the_child_engine_refuses_an_estimate_over_the_limit() {
        let fits = Command::from_bytes(&test_cmd_wasm()).unwrap();
        let engine = fits.pre.engine().clone();
        assert!(engine.pooling_allocator_metrics().is_none());
        let component = Component::from_binary(&engine, &test_cmd_wasm()).unwrap();
        let too_big =
            Command::from_component(&engine, &component, CHILD_MEMORY_LIMIT as u64 + 1).unwrap();
        let sandbox = crate::Sandbox::embedded()
            .shell()
            .with_command("fits", fits)
            .with_command("too-big", too_big)
            .build()
            .unwrap();
        sandbox.execute("echo hi | fits > /dev/null").await.unwrap();
        let err = sandbox.execute("echo hi | too-big").await.unwrap_err();
        assert!(err.to_string().contains("too-big: limit-exceeded"), "{err}");
    }

    /// A child whose instances' metadata doesn't fit the memory pool isn't
    /// started; its share is given back once it ends.
    #[test]
    fn instance_metadata_is_charged_to_the_pool() {
        let pool = Arc::new(AtomicUsize::new(0));
        let held = ChildMemory::reserve(Arc::clone(&pool), 100, 60).unwrap();
        assert_eq!(pool.load(Ordering::Relaxed), 60);
        assert!(ChildMemory::reserve(Arc::clone(&pool), 100, 60).is_none());
        drop(held);
        assert_eq!(pool.load(Ordering::Relaxed), 0);
    }

    /// A command is linked for the WASI version whose `wasi:cli/run` it
    /// exports; a component exporting neither isn't a command.
    #[test]
    fn commands_are_linked_for_their_wasi_version() {
        let p2 = Command::from_bytes(&test_cmd_wasm()).unwrap();
        assert!(matches!(p2.pre, Pre::P2(_)));
        let p3 = Command::from_bytes(&test_cmd_wasm_for("wasm32-wasip3")).unwrap();
        assert!(matches!(p3.pre, Pre::P3(_)));

        let engine = crate::Executor::child_engine().unwrap();
        let empty = Component::new(&engine, "(component)").unwrap();
        assert!(Command::from_component(&engine, &empty, 0).is_err());
    }

    /// A command importing what children don't get is refused at load with
    /// each such import named, and one exporting no `wasi:cli/run` is told
    /// it isn't a command; `inspect` reports every import.
    #[test]
    fn unsupported_imports_are_named_at_load() {
        let wasm = wat::parse_str(
            r#"(component
                (import "wasi:http/outgoing-handler@0.2.0" (instance))
                (import "wasi:cli/environment@0.2.0" (instance))
                (import "wasi:cli/stdout@0.1.0" (instance))
            )"#,
        )
        .unwrap();
        let err = Command::from_bytes(&wasm).unwrap_err();
        let ErrorKind::CommandImports { missing } = err.kind() else {
            panic!("{err}");
        };
        let names: Vec<_> = missing.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            ["wasi:http/outgoing-handler@0.2.0", "wasi:cli/stdout@0.1.0"]
        );
        assert!(
            err.to_string().contains("children get no wasi:http"),
            "{err}"
        );

        let info = Command::inspect(&wasm).unwrap();
        assert_eq!(info.problem.as_deref(), Some(err.to_string().as_str()));
        assert_eq!(info.wasi, None);
        assert_eq!(info.imports.len(), 3);
        assert_eq!(info.imports[1].support, ImportSupport::Provided);

        let empty = wat::parse_str("(component)").unwrap();
        let err = Command::from_bytes(&empty).unwrap_err();
        assert!(err.to_string().contains("not a WASI command"), "{err}");
        let info = Command::inspect(&empty).unwrap();
        assert!(info.problem.unwrap().contains("not a WASI command"));
        let module = wat::parse_str("(module)").unwrap();
        assert!(Command::inspect(&module).is_err());

        let info = Command::inspect(&test_cmd_wasm()).unwrap();
        assert!(info.runnable(), "{:?}", info.problem);
        assert_eq!(info.wasi, Some("0.2"));
        assert!(
            info.imports
                .iter()
                .all(|i| !matches!(i.support, ImportSupport::Missing(_))),
            "{:?}",
            info.imports
        );
    }

    /// The table of what children get agrees with the real child linker:
    /// a probe component importing each interface of the WIT wasmtime-wasi
    /// links (with its real types) links exactly when `support` says
    /// children get it, and so do probes children don't get.
    #[test]
    fn import_support_matches_the_child_linker() {
        use wit_component_wasmtime::{ComponentEncoder, StringEncoding};
        use wit_parser_wasmtime::{LiftLowerAbi, ManglingAndAbi, Resolve};

        // The WIT of the wasmtime-wasi in Cargo.lock, from cargo's registry
        // sources (where the build got it).
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock = std::fs::read_to_string(manifest.join("../../Cargo.lock"))
            .expect("reading the workspace Cargo.lock");
        let version = lock
            .split("\n[[package]]\n")
            .find_map(|p| p.strip_prefix("name = \"wasmtime-wasi\"\nversion = \""))
            .and_then(|v| v.split('"').next())
            .expect("wasmtime-wasi in Cargo.lock");
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")))
            .expect("CARGO_HOME or HOME");
        let registry = cargo_home.join("registry/src");
        let src = std::fs::read_dir(&registry)
            .unwrap_or_else(|e| panic!("reading {}: {e}", registry.display()))
            .flatten()
            .map(|index| index.path().join(format!("wasmtime-wasi-{version}/src")))
            .find(|src| src.join("p2/wit").is_dir())
            .unwrap_or_else(|| {
                panic!(
                    "wasmtime-wasi {version}'s sources aren't in {}",
                    registry.display()
                )
            });

        // A component importing `interface` from the WIT in `dir`, plus
        // `extra` WIT defining packages of its own.
        let probe = |dir: &Path, interface: &str, extra: Option<&str>, abi| {
            let mut resolve = Resolve::default();
            resolve.push_dir(dir).unwrap();
            if let Some(extra) = extra {
                resolve.push_str("extra.wit", extra).unwrap();
            }
            let world = format!("package eryx:probe;\nworld probe {{ import {interface}; }}\n");
            let package = resolve.push_str("probe.wit", &world).unwrap();
            let world = resolve.select_world(&[package], Some("probe")).unwrap();
            let mut module = wit_component_wasmtime::dummy_module(&resolve, world, abi);
            wit_component_wasmtime::embed_component_metadata(
                &mut module,
                &resolve,
                world,
                StringEncoding::UTF8,
                false,
            )
            .unwrap();
            ComponentEncoder::default()
                .module(&module)
                .unwrap()
                .encode()
                .unwrap()
        };

        let mut probes = Vec::new();
        for (dir, abi) in [
            ("p2/wit", ManglingAndAbi::Legacy(LiftLowerAbi::Sync)),
            (
                "p3/wit",
                ManglingAndAbi::Legacy(LiftLowerAbi::AsyncCallback),
            ),
        ] {
            let dir = src.join(dir);
            let mut resolve = Resolve::default();
            resolve.push_dir(&dir).unwrap();
            for (_, package) in &resolve.packages {
                if package.name.namespace != "wasi" {
                    continue;
                }
                let version = package.name.version.as_ref().unwrap();
                for name in package.interfaces.keys() {
                    let interface = format!("wasi:{}/{name}@{version}", package.name.name);
                    probes.push((interface.clone(), probe(&dir, &interface, None, abi)));
                }
            }
        }
        assert!(probes.len() > 40, "{}", probes.len());
        // What children don't get, with real types: another version of an
        // interface they do get, a prerelease, and a package they don't.
        let p2 = src.join("p2/wit");
        let sync = ManglingAndAbi::Legacy(LiftLowerAbi::Sync);
        for (interface, wit) in [
            (
                "wasi:cli/environment@0.1.0",
                "package wasi:cli@0.1.0;\ninterface environment { get-arguments: func() -> list<string>; }",
            ),
            (
                "wasi:random/random@0.3.0-rc-2025-09-16",
                "package wasi:random@0.3.0-rc-2025-09-16;\ninterface random { get-random-u64: func() -> u64; }",
            ),
            (
                "wasi:http/outgoing-handler@0.2.0",
                "package wasi:http@0.2.0;\ninterface outgoing-handler { handle: func(); }",
            ),
        ] {
            probes.push((interface.to_owned(), probe(&p2, interface, Some(wit), sync)));
        }

        let engine = crate::Executor::child_engine().unwrap();
        let linker = child_linker(&engine).unwrap();
        let mut disagree = Vec::new();
        for (name, wasm) in &probes {
            let component = Component::new(&engine, wasm).unwrap();
            let links = linker
                .instantiate_pre(&component)
                .map(drop)
                .map_err(|e| format!("{e:#}"));
            let provided = !matches!(imports::support(name), ImportSupport::Missing(_));
            if links.is_ok() != provided {
                disagree.push(format!(
                    "{name}: support says {provided}, linking: {links:?}"
                ));
            }
        }
        assert!(disagree.is_empty(), "{disagree:#?}");
    }

    /// A WASI preview 1 module is adapted only if it's a command: a reactor
    /// (no `_start`) and a module not importing `wasi_snapshot_preview1` are
    /// refused, and components pass through unchanged.
    #[test]
    fn only_preview1_commands_are_adapted() {
        let reactor = wat::parse_str(
            r#"(module
                (import "wasi_snapshot_preview1" "proc_exit" (func (param i32)))
                (memory (export "memory") 1)
                (func (export "_initialize")))"#,
        )
        .unwrap();
        let err = Command::to_component(&reactor).unwrap_err();
        assert!(err.to_string().contains("reactor"), "{err}");
        let plain = wat::parse_str("(module (func (export \"_start\")))").unwrap();
        let err = Command::to_component(&plain).unwrap_err();
        assert!(err.to_string().contains("wasi_snapshot_preview1"), "{err}");
        // Inspecting says a preview 1 module can't run, rather than failing.
        let info = Command::inspect(&reactor).unwrap();
        assert!(info.preview1 && !info.runnable(), "{info:?}");
        assert!(info.problem.unwrap().contains("reactor"));
        assert!(Command::inspect(&plain).is_err());

        let wasm = test_cmd_wasm();
        assert!(matches!(
            Command::to_component(&wasm).unwrap(),
            std::borrow::Cow::Borrowed(_)
        ));
        let p1 = test_cmd_wasm_for("wasm32-wasip1");
        let adapted = Command::to_component(&p1).unwrap();
        assert!(wasmparser::Parser::is_component(&adapted));
        let info = Command::inspect(&p1).unwrap();
        assert!(info.preview1 && info.runnable(), "{info:?}");
        assert_eq!(info.wasi, Some("0.2"));
        assert!(matches!(Command::from_bytes(&p1).unwrap().pre, Pre::P2(_)));
    }

    /// Only commands that can get a credential (allowed, with a network, not
    /// opted out, and one is for them) have their proxy's password scrubbed.
    #[test]
    fn credentialed_names_are_those_a_credential_can_reach() {
        use crate::net::{ChildNetwork, Credential};
        let command = Command::from_bytes(&test_cmd_wasm()).unwrap();
        let mut commands = Commands::default();
        for name in ["gh", "git", "cat", "ls"] {
            commands.insert(name.into(), command.clone());
        }
        let names = |allowed: Option<&[String]>,
                     options: &HashMap<String, CommandOptions>,
                     credentials: &[&Credential]| {
            let mut names = commands.credentialed_names(allowed, options, credentials);
            names.sort();
            names
        };
        let none = HashMap::new();
        let gh = Credential::bearer("s")
            .for_hosts(["api.github.com"])
            .for_commands(["gh"]);
        let any = Credential::bearer("s").for_hosts(["github.com"]);
        assert!(names(None, &none, &[]).is_empty());
        assert_eq!(names(None, &none, &[&gh]), ["gh"]);
        assert_eq!(names(None, &none, &[&any]), ["cat", "gh", "git", "ls"]);
        // Not allowed, no network, or opted out: no password to scrub.
        let allowed = ["gh".to_owned(), "git".to_owned(), "cat".to_owned()];
        let options = HashMap::from([
            (
                "git".to_owned(),
                CommandOptions::new().with_network(ChildNetwork::None),
            ),
            (
                "cat".to_owned(),
                CommandOptions::new().without_credentials(),
            ),
        ]);
        assert_eq!(names(Some(&allowed), &options, &[&any]), ["gh"]);
    }

    /// Running reuses the command's `CommandPre`: nothing is compiled or
    /// linked after `Command::from_bytes`, however many children run.
    #[tokio::test]
    async fn runs_reuse_the_compiled_command() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let command = Command::from_bytes(&test_cmd_wasm()).unwrap();
        let compiled = COMPILED.load(Ordering::SeqCst);
        for input in ["one", "two"] {
            let (mut stdin_tx, stdin) = pipe::pipe(PIPE_CAPACITY);
            let (stdout, mut stdout_rx) = pipe::pipe(PIPE_CAPACITY);
            let (stderr, _stderr_rx) = pipe::pipe(PIPE_CAPACITY);
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
                memory: ChildMemory::reserve(Arc::default(), CHILD_MEMORY_LIMIT, 0).unwrap(),
                _live: LiveSlot::take(&Arc::default()),
                _args: Held {
                    budget: ChildBudget::default(),
                    bytes: 0,
                },
                network: None,
                spawner: None,
                #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
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
