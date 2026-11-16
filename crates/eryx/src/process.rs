//! External commands for the shell guest: `eryx:process`.
//!
//! A [`Command`] is a WASI 0.2 or 0.3 command component (one that exports
//! `wasi:cli/run`), compiled once on the shared engine. A shell sandbox
//! registers commands by name
//! ([`SandboxBuilder::with_command`](crate::SandboxBuilder::with_command)), and
//! the shell spawns them as child processes through the `eryx:process`
//! interface. Each spawn instantiates from the command's cached
//! pre-instance, so children get the engine's pooling allocator, fuel
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
//! - **Network:** through the child proxy only, when the sandbox has one
//!   (#812); otherwise none.
//! - **Commands:** a child whose command was registered with
//!   [`Command::allow_spawn`] runs commands itself through
//!   `eryx:process/run`, to completion, with the same commands, filesystem,
//!   network and execution limits, at most `MAX_SPAWN_DEPTH` deep.
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

mod pipe;

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

    /// Compile a command component from its `.wasm` bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes don't compile, or aren't a WASI 0.2 or
    /// 0.3 command.
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
    /// precompiled it, and an error if it isn't a WASI 0.2 or 0.3 command.
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
        let pre = Pre::new(child_linker(engine)?, engine, component)?;
        Ok(Self {
            pre,
            tunnels: false,
            spawns: false,
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

    /// Let this command's children run commands themselves, through
    /// `eryx:process/run`, as git runs `git` for `stash` or `worktree add`.
    ///
    /// They resolve names against the same commands, see the same filesystem
    /// and get the same child network as the shell's children, and share
    /// their execution's limits: its fuel, child memory, live children and
    /// host memory. Children nest at most 4 deep (the shell's are the
    /// first), and each nested run's stdout and stderr are capped at 16 MiB.
    /// A command spawns only if it was registered with this too.
    #[must_use]
    pub fn allow_spawn(mut self) -> Self {
        self.spawns = true;
        self
    }
}

/// An execution's network for its children: the sandbox's policy and secrets,
/// and the proxies started for them, at most one inspecting and one
/// tunnelling. They live as long as the execution (see
/// [`ExecutorState::disconnect`]), well inside their CAs' validity.
pub(crate) struct ChildNet {
    config: crate::net::NetConfig,
    secrets: HashMap<String, crate::secrets::SecretConfig>,
    network: crate::net::ChildNetwork,
    proxies: [Option<ProxyLink>; 2],
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
    /// `None` for [`ChildNetwork::None`](crate::net::ChildNetwork::None):
    /// children stay offline.
    pub(crate) fn new(
        config: crate::net::NetConfig,
        secrets: HashMap<String, crate::secrets::SecretConfig>,
        network: crate::net::ChildNetwork,
    ) -> Option<Self> {
        (!matches!(network, crate::net::ChildNetwork::None)).then_some(Self {
            config,
            secrets,
            network,
            proxies: [None, None],
            sockets: Arc::default(),
        })
    }

    /// The proxy for a child that may (`tunnels`) or may not open blind
    /// tunnels, started on first use.
    fn link(&mut self, tunnels: bool) -> std::io::Result<ChildLink> {
        let slot = &mut self.proxies[usize::from(tunnels)];
        let link = match slot {
            Some(link) => link,
            None => {
                let Some(proxy) = crate::net::ChildProxy::start(
                    self.config.clone(),
                    self.secrets.clone(),
                    self.network.clone(),
                    tunnels,
                )?
                else {
                    return Err(std::io::Error::other("children have no network"));
                };
                let ca_dir = tempfile::tempdir()?;
                std::fs::write(ca_dir.path().join(CA_FILE), proxy.ca_pem())?;
                slot.insert(ProxyLink { proxy, ca_dir })
            }
        };
        Ok(ChildLink {
            proxy: link.proxy.addr(),
            ca_dir: link.ca_dir.path().to_path_buf(),
            sockets: Arc::clone(&self.sockets),
        })
    }
}

/// What a child needs to reach its proxy.
#[derive(Clone)]
struct ChildLink {
    proxy: std::net::SocketAddr,
    ca_dir: std::path::PathBuf,
    /// Its execution's count of children's sockets.
    sockets: Arc<AtomicUsize>,
}

impl ChildLink {
    /// The env vars pointing a child's HTTP clients at the proxy and its CA.
    fn env(&self) -> Vec<(String, String)> {
        let proxy = format!("http://{}", self.proxy);
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
    },
    End {
        id: u64,
        outcome: Result<i32, String>,
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
        NestedTraces::push(&traces, NestedEvent::Start { id, argv });
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
                },
            );
        }
    }
}

/// A child ready to run, with what it holds of the execution's budget and
/// the parent's ends of its pipes.
struct Prepared {
    run: ChildRun,
    charged: u64,
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
        #[cfg(feature = "unstable-vfs")] vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
    ) -> Result<Prepared, ProcessError> {
        let command = self
            .commands
            .0
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
                        .link(command.tunnels)
                }) {
                Ok(link) => Some(link),
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
        let charged = args_bytes as u64;
        self.budget.charge(charged)?;

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
            memory: Arc::clone(&self.budget.memory),
            _live: live,
            network,
            spawner: command.spawns.then(|| Spawner {
                depth: self.depth + 1,
                traces: Some(self.traces.clone().unwrap_or_default()),
                ..self.clone()
            }),
            #[cfg(feature = "unstable-vfs")]
            vfs,
        };
        Ok(Prepared {
            run,
            charged,
            stdin: stdin_tx,
            stdout: stdout_rx,
            stderr: stderr_rx,
        })
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
    /// Load a precompiled command to run with
    /// [`run_isolated`](Self::run_isolated): it's linked with plain WASI 0.2
    /// (a WASI 0.3 command fails to link), so its filesystem is the host
    /// directories it's given, never a sandbox's VFS (which `unstable-vfs`
    /// puts in place of WASI's filesystem for children, empty for one
    /// without a parent).
    ///
    /// # Safety
    ///
    /// As for [`from_precompiled`](Self::from_precompiled): only bytes eryx
    /// precompiled and you trust.
    #[allow(unsafe_code)]
    pub(crate) unsafe fn from_precompiled_isolated(precompiled: &[u8]) -> Result<Self, Error> {
        let engine = crate::Executor::shared_engine()?;
        // SAFETY: the caller guarantees the bytes are trusted eryx output.
        let component = unsafe { Component::deserialize(&engine, precompiled) }
            .map_err(crate::wasm::deserialize_error)?;
        let pre = Pre::new(isolated_linker(&engine)?, &engine, &component)?;
        Ok(Self {
            pre,
            tunnels: false,
            spawns: false,
        })
    }

    /// Run the command on its own, outside any sandbox, with `argv`, no
    /// environment variables, empty stdin, no network (no sockets or name
    /// lookups), and only `mounts` as its filesystem, within `limits`.
    /// Returns its exit code, stdout and stderr. Load it with
    /// [`from_precompiled_isolated`](Self::from_precompiled_isolated), or
    /// with `unstable-vfs` it sees an empty filesystem instead of `mounts`.
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
                limits: ChildMemory {
                    pool: Arc::new(AtomicUsize::new(0)),
                    held: 0,
                    limit: limits.memory,
                },
                // No network: TCP is off.
                sockets: ChildSockets::default(),
                fuel: limits.fuel,
                fuel_used: Arc::default(),
                // Isolated commands run nothing else.
                spawner: None,
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
            .map_err(ErrorKind::wasm_component)?;
        Ok(if p3 {
            Self::P3(
                wasmtime_wasi::p3::bindings::CommandPre::new(pre)
                    .map_err(ErrorKind::wasm_component)?,
            )
        } else {
            Self::P2(
                wasmtime_wasi::p2::bindings::CommandPre::new(pre)
                    .map_err(ErrorKind::wasm_component)?,
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

/// The commands a sandbox can spawn, by name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Commands(Arc<HashMap<String, Command>>);

impl Commands {
    pub(crate) fn insert(&mut self, name: String, command: Command) {
        Arc::make_mut(&mut self.0).insert(name, command);
    }

    /// Just the commands named in `allowed`, for one execution; `None`
    /// keeps them all, and `Some` of an empty list leaves none. Names that
    /// aren't registered are ignored.
    pub(crate) fn only(&self, allowed: Option<&[String]>) -> Self {
        let Some(allowed) = allowed else {
            return self.clone();
        };
        let mut kept = HashMap::new();
        for name in allowed {
            match self.0.get(name) {
                Some(command) => {
                    kept.insert(name.clone(), command.clone());
                }
                None => tracing::debug!(command = %name, "allowed command isn't registered"),
            }
        }
        Self(Arc::new(kept))
    }

    /// Register `command` as `name` unless something already is.
    #[cfg(all(feature = "embedded", feature = "coreutils"))]
    pub(crate) fn insert_if_absent(&mut self, name: &str, command: &Command) {
        if !self.0.contains_key(name) {
            Arc::make_mut(&mut self.0).insert(name.to_owned(), command.clone());
        }
    }
}

/// The linker children are instantiated with: WASI 0.2 and 0.3, plus the
/// hybrid VFS with `unstable-vfs`. Built once for the shared engine.
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
    #[cfg(feature = "unstable-vfs")]
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
#[cfg(feature = "packages")]
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
    #[cfg(feature = "unstable-vfs")]
    vfs: Option<eryx_vfs::HybridVfsCtx<eryx_vfs::ArcStorage>>,
}

/// A child's share of `pool`, which may grow to `limit` (its execution's
/// `CHILD_MEMORY_LIMIT`), given back when its store is dropped.
struct ChildMemory {
    pool: Arc<AtomicUsize>,
    held: usize,
    limit: usize,
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
                used.checked_add(grow).filter(|&u| u <= self.limit)
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
    /// The execution's pool of child memory.
    memory: Arc<AtomicUsize>,
    /// Its live slot, given back when the run ends or is dropped.
    _live: LiveSlot,
    /// Its proxy, if children have network.
    network: Option<ChildLink>,
    /// How it runs commands of its own, if its command may.
    spawner: Option<Spawner>,
    #[cfg(feature = "unstable-vfs")]
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
            .stderr(self.stderr)
            // The proxy resolves names; children reach it by address.
            .allow_ip_name_lookup(false)
            .allow_udp(false);
        #[cfg(feature = "unstable-vfs")]
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
            #[cfg(feature = "unstable-vfs")]
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
            #[cfg(not(feature = "unstable-vfs"))]
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
                limits: ChildMemory {
                    pool: self.memory,
                    held: 0,
                    limit: CHILD_MEMORY_LIMIT,
                },
                sockets: ChildSockets::new(
                    self.network
                        .as_ref()
                        .map(|link| Arc::clone(&link.sockets))
                        .unwrap_or_default(),
                ),
                fuel: self.fuel,
                fuel_used: Arc::clone(&self.fuel_used),
                spawner: self.spawner,
                #[cfg(feature = "unstable-vfs")]
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
            charged,
            stdin: stdin_tx,
            stdout: stdout_rx,
            stderr: stderr_rx,
        } = state.spawner().prepare(
            cmd,
            args,
            env,
            cwd,
            #[cfg(feature = "unstable-vfs")]
            state
                .hybrid_vfs_ctx
                .as_ref()
                .map(eryx_vfs::HybridVfsCtx::fork),
        )?;
        let child = ChildProcess {
            run: Some(run),
            charged,
        };
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
                        NestedEvent::Start { id, argv } => {
                            if let Some(trace) = state.trace_process_start(&argv) {
                                reported.insert(id, trace);
                            }
                        }
                        NestedEvent::End { id, outcome } => {
                            if let Some(trace) = reported.remove(&id) {
                                trace.end(state, outcome.as_ref().copied().map_err(String::as_str));
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
                // Fuel is always enabled on the shared engine.
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
        charged,
        stdin,
        stdout,
        stderr,
    } = spawner.prepare(
        cmd,
        args,
        env,
        cwd,
        #[cfg(feature = "unstable-vfs")]
        state.vfs.as_ref().map(eryx_vfs::HybridVfsCtx::fork),
    )?;
    let trace = spawner
        .traces
        .clone()
        .map(|traces| NestedTrace::start(traces, run.argv.clone()));
    let mut held = Held {
        budget: spawner.budget,
        bytes: charged,
    };
    held.charge(input.len() as u64)?;
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
        let delta = used.saturating_sub(charged_fuel);
        charged_fuel = charged_fuel.max(used);
        let left = store.get_fuel().unwrap_or(0).saturating_sub(delta);
        // Fuel is always enabled on the shared engine.
        let _ = store.set_fuel(left);
        let state = store.data();
        state
            .fuel_used
            .store(state.fuel.saturating_sub(left), Ordering::Relaxed);
        left > 0
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// How many commands have been linked into a pre-instance.
    pub(super) static COMPILED: AtomicUsize = AtomicUsize::new(0);

    fn test_cmd_wasm() -> Vec<u8> {
        test_cmd_wasm_for("wasm32-wasip2")
    }

    fn test_cmd_wasm_for(target: &str) -> Vec<u8> {
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
        let engine = crate::Executor::shared_engine().unwrap();
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

    /// A command is linked for the WASI version whose `wasi:cli/run` it
    /// exports; a component exporting neither isn't a command.
    #[test]
    fn commands_are_linked_for_their_wasi_version() {
        let p2 = Command::from_bytes(&test_cmd_wasm()).unwrap();
        assert!(matches!(p2.pre, Pre::P2(_)));
        let p3 = Command::from_bytes(&test_cmd_wasm_for("wasm32-wasip3")).unwrap();
        assert!(matches!(p3.pre, Pre::P3(_)));

        let engine = crate::Executor::shared_engine().unwrap();
        let empty = Component::new(&engine, "(component)").unwrap();
        assert!(Command::from_component(&engine, &empty).is_err());
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
                memory: Arc::default(),
                _live: LiveSlot::take(&Arc::default()).unwrap(),
                network: None,
                spawner: None,
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
