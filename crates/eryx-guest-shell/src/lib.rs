//! A bash-compatible shell (brush) as an eryx guest.
//!
//! Exports the same `eryx:sandbox/sandbox` world as the CPython and QuickJS
//! guests, and imports `eryx:process` for external commands (unregistered
//! ones exit 127):
//!
//! - `execute` runs a script on a persistent brush `Shell`. Output goes to the
//!   guest's WASI fd 1/2, which the host already captures.
//! - Host callbacks are shell commands: `get_time`, `add 2 3`,
//!   `add --a 2 --b 3`, `add --json '{"a":2}'`, or `invoke add '{"a":2}'`,
//!   with arguments typed by the callback's schema (see [`call`]). The result
//!   goes to stdout, so `$(add 2 3 | ...)` composes like any other command.
//!   Each call reports `callback_start` and `callback_end` trace events.
//! - A non-zero exit status becomes an `execute-error` ("exit status N").
//! - `snapshot-state` is brush's own serde form of the `Shell`, after a line
//!   naming that format; restoring refuses any other format.
//! - Anything that isn't a builtin, function or callback is spawned through
//!   `eryx:process` (host-registered WASI commands, e.g. uutils coreutils).

#![allow(clippy::expect_used)] // single-threaded guest: a broken invariant traps either way

mod call;
mod grep;
mod input;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Arc;

use brush_core::extensions::{
    DefaultErrorFormatter, ProcessLauncher, ShellExtensionsImpl, TaskSpawner,
};
use brush_core::openfiles::OpenFiles;
use brush_core::processes::{ChildProcess, LaunchRequest};
use brush_core::tasks::TaskHandle;
use brush_core::{
    ExecutionContext, ExecutionParameters, ExecutionResult, ShellExtensions, SourceInfo, builtins,
    error,
};

/// brush with external commands launched as eryx child components, and its
/// concurrent tasks on a `LocalSet` the component's executor drives.
type Ext = ShellExtensionsImpl<DefaultErrorFormatter, EryxLauncher, LocalSpawner>;
type Shell = brush_core::Shell<Ext>;

/// Where host commands appear to live. Nothing is there: the shell finds them
/// through its command table, which `register_builtins` seeds.
const COMMAND_DIR: &str = "/eryx/bin";

wit_bindgen::generate!({
    path: "wit",
    world: "eryx:shell-guest/shell-sandbox",
    generate_all,
});

// State that lasts across calls into the guest is a plain static, not a
// `thread_local!`: on wasm32-wasip3 each call into an export runs as a task of
// its own, with thread-local storage of its own, so a thread-local set by one
// call (`set-result-variable`, or the shell an `execute` left behind) is gone
// by the next. Thread-locals below only live within one `execute`.
static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("failed to create tokio runtime")
});
static SHELL: std::sync::Mutex<Option<Shell>> = std::sync::Mutex::new(None);
/// The result variable's name; empty for the default, `result`.
static RESULT_VAR: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The guest is single-threaded, so a lock is never contended or poisoned.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

thread_local! {
    /// The host's callbacks by name, refreshed every execution.
    static CALLBACKS: RefCell<HashMap<String, call::CallbackDef>> = RefCell::new(HashMap::new());
    /// Prefix callbacks (`ns.*`), as `ns.`.
    static PREFIXES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Children's output pumps still running, and a signal each time one
    /// writes; see [`wait_for_input`].
    static PUMPS: Cell<usize> = const { Cell::new(0) };
    static PUMPED: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());
    /// A signal each time an input pump or builtin reads from a pipe; see
    /// [`drain`].
    static READ: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());
}

/// Most bytes a child's output pump leaves unread in an in-guest pipe before
/// it waits for the reader. brush's wasm pipes are unbounded, so without this
/// a fast child would fill the guest's memory; with it, the child waits on its
/// host pipe instead.
const PIPE_HIGH_WATER: usize = 64 * 1024;

/// Note that a pipe was read from, for a pump waiting in [`drain`].
pub(crate) fn note_read() {
    READ.with(|r| r.notify_waiters());
}

/// Wait for `signal`, or 1 ms, whichever comes first.
async fn signalled_or_soon(signal: &tokio::sync::Notify) {
    let mut notified = std::pin::pin!(signal.notified());
    let mut timer = std::pin::pin!(sleep(1_000_000));
    std::future::poll_fn(|cx| {
        if notified.as_mut().poll(cx).is_ready() || timer.as_mut().poll(cx).is_ready() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
}

/// Wait until an empty in-guest pipe may have data.
///
/// brush's wasm pipes have no waker: a reader that finds one empty while its
/// writer runs gets `WouldBlock`. Yielding straight back is right when the
/// writer is a builtin, which runs in this guest. When a child's output pump
/// may be the writer, yielding would spin through a host round trip per
/// poll, so park until a pump writes, with a short timer for the case where a
/// builtin turns out to be the writer after all.
pub(crate) async fn wait_for_input() {
    if PUMPS.get() == 0 {
        tokio::task::yield_now().await;
        return;
    }
    signalled_or_soon(&PUMPED.with(Arc::clone)).await;
}

fn block_on<F: Future>(f: F) -> F::Output {
    RUNTIME.block_on(f)
}

/// Run `f`, and the tasks it spawns, under the component's executor.
///
/// Child processes' streams only make progress there, so pipeline stages run
/// as `spawn_local` tasks on a `LocalSet` polled from the async export, not
/// under tokio's `block_on`. A stage waiting on a pipe yields back to the
/// host, which runs the children meanwhile.
async fn run_local<F: Future>(f: F) -> F::Output {
    let handle = RUNTIME.handle().clone();
    let _enter = handle.enter();
    tokio::task::LocalSet::new().run_until(f).await
}

/// Starts the shell's concurrent tasks on the current `LocalSet`.
#[derive(Clone, Default)]
struct LocalSpawner;

impl TaskSpawner for LocalSpawner {
    fn spawn<F>(&self, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let task = tokio::task::spawn_local(future);
        TaskHandle::from_future(async move {
            task.await
                .map_err(|e| brush_core::ErrorKind::TaskError(Box::new(e)).into())
        })
    }

    fn spawn_blocking<F>(
        &self,
        future: F,
    ) -> impl Future<Output = TaskHandle<F::Output>> + Send + use<F>
    where
        F: Future + Send + 'static,
        F::Output: Send + Sync + 'static,
    {
        // It runs on the LocalSet alongside the caller, so the caller can go
        // on right away, as it would with a thread of its own. Waiting for it
        // instead would run a pipeline stage or command substitution to the
        // end before its reader started, and a child writing more than a pipe
        // holds into it would wait forever for that reader.
        std::future::ready(self.spawn(future))
    }
}

/// Register brush's bash builtins, one `invoke` builtin per host callback, and
/// the host's commands in the command table.
// ponytail: commands live only in the hash table, so `hash -r`, `type` and
// `command -v` don't see them; a read-only /eryx/bin in the VFS fixes that (#809).
fn register_builtins(shell: &mut Shell) {
    for (name, reg) in brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode) {
        shell.builtins_mut().register(name, reg);
    }
    shell
        .builtins_mut()
        .register("invoke", builtins::Registration::of::<InvokeCommand>());
    shell
        .builtins_mut()
        .register("grep", builtins::Registration::of::<grep::GrepCommand>());
    let mut defs = HashMap::new();
    let mut prefixes = Vec::new();
    for cb in list_callbacks() {
        if let Some(ns) = cb.name.strip_suffix('*') {
            // A prefix callback has no single command name; `invoke ns.x`
            // reaches it.
            prefixes.push(ns.to_owned());
            continue;
        }
        let schema = serde_json::from_str(&cb.parameters_schema_json).ok();
        defs.insert(
            cb.name.clone(),
            call::CallbackDef {
                description: cb.description,
                schema,
            },
        );
        // Builtins (registered above) win over a callback of the same name,
        // as functions win over both; `invoke NAME` still reaches it.
        shell
            .builtins_mut()
            .register_if_absent(cb.name, builtins::Registration::of::<InvokeCommand>());
    }
    CALLBACKS.with(|c| *c.borrow_mut() = defs);
    PREFIXES.with(|p| *p.borrow_mut() = prefixes);
    for name in eryx::process::process::list_commands() {
        let path = std::path::Path::new(COMMAND_DIR).join(&name);
        shell.program_location_cache_mut().set(name, path);
    }
}

fn new_shell() -> Shell {
    let mut shell = block_on(brush_core::Shell::builder_with_extensions::<Ext>().build())
        .expect("failed to create shell");
    register_builtins(&mut shell);
    shell
}

fn with_shell<R>(f: impl FnOnce(&mut Shell) -> R) -> R {
    f(lock(&SHELL).get_or_insert_with(new_shell))
}

/// Read and unset the result variable. A value that parses as JSON is passed
/// through; anything else becomes a JSON string.
fn take_result(shell: &mut Shell) -> String {
    let name = match lock(&RESULT_VAR).clone() {
        name if name.is_empty() => "result".to_owned(),
        name => name,
    };
    let Some(value) = shell
        .env()
        .get(&name)
        .map(|(_, v)| v.value().to_cow_str(shell).into_owned())
    else {
        return String::new();
    };
    let _ = shell.env_mut().unset(&name);
    if serde_json::from_str::<serde_json::Value>(&value).is_ok() {
        value
    } else {
        serde_json::Value::String(value).to_string()
    }
}

/// First line of every snapshot: what wrote the serialized `Shell`. Bump the
/// brush part with the brush pin in Cargo.toml, and the version when the
/// guest's own state changes.
const STATE_FORMAT: &str = "eryx-shell-state/1 brush@25210fd6";

/// Start of a `restore-state` error for a snapshot in another state format,
/// followed by the snapshot's format, a tab, and the one this runtime reads.
/// Must match the host's `INCOMPATIBLE_STATE_PREFIX`.
const INCOMPATIBLE_STATE_PREFIX: &str = "eryx:incompatible-state-format:";

/// First line of framed code: `#eryx-preamble:<len>`, then `<len>` bytes of
/// host preamble, then the user code. The host frames every execution; only
/// one leading header is split off. Must match the host's `frame_code`.
const PREAMBLE_HEADER: &str = "#eryx-preamble:";

/// Split framed `code` into `(preamble, user_code)`. Code without a
/// well-formed header is all user code.
fn split_preamble(code: &str) -> (&str, &str) {
    code.strip_prefix(PREAMBLE_HEADER)
        .and_then(|rest| {
            let (len, rest) = rest.split_once('\n')?;
            let len: usize = len.parse().ok()?;
            Some((rest.get(..len)?, rest.get(len..)?))
        })
        .unwrap_or(("", code))
}

struct Sandbox;

impl Guest for Sandbox {
    async fn execute(code: String) -> Result<ExecuteOutput, ExecuteError> {
        // The host's preamble exports the secrets' placeholders, for child
        // commands; it runs first, and its status doesn't count.
        let (preamble, code) = split_preamble(&code);
        let mut shell = lock(&SHELL).take().unwrap_or_else(new_shell);
        // Callbacks can differ per execution; re-register the current set.
        register_builtins(&mut shell);
        if !preamble.is_empty() {
            let _ = run_local(shell.run_string(
                preamble,
                &SourceInfo::default(),
                &ExecutionParameters::default(),
            ))
            .await;
        }
        let status = run_local(shell.run_string(
            code,
            &SourceInfo::default(),
            &ExecutionParameters::default(),
        ))
        .await
        .map(|r| u8::from(r.exit_code));
        let result_json = take_result(&mut shell);
        *lock(&SHELL) = Some(shell);
        // std's stdout is line-buffered; flush so the host sees the tail.
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();

        // Output lives on fd 1/2, which the host appends to both arms.
        match status {
            Ok(0) => Ok(ExecuteOutput {
                stdout: vec![],
                stderr: vec![],
                result_json,
                result_error: String::new(),
            }),
            Ok(code) => Err(ExecuteError {
                message: format!("exit status {code}"),
                exit_code: Some(i32::from(code)),
                stdout: vec![],
                stderr: vec![],
            }),
            Err(e) => Err(ExecuteError {
                message: e.to_string(),
                exit_code: None,
                stdout: vec![],
                stderr: vec![],
            }),
        }
    }

    async fn snapshot_state() -> Result<StateSnapshot, String> {
        let mut data = format!("{STATE_FORMAT}\n").into_bytes();
        with_shell(|shell| serde_json::to_writer(&mut data, shell)).map_err(|e| e.to_string())?;
        Ok(StateSnapshot {
            data,
            skipped: vec![],
        })
    }

    async fn restore_state(data: Vec<u8>) -> Result<(), String> {
        // brush's serde form only reads back into the same brush, so refuse
        // any other format id. The prefix tells the host it's a mismatch, not
        // corrupt data.
        let (format, json) = match data.iter().position(|&b| b == b'\n') {
            Some(i) => (
                String::from_utf8_lossy(&data[..i]).into_owned(),
                &data[i + 1..],
            ),
            None => (String::new(), &data[..]),
        };
        if format != STATE_FORMAT {
            return Err(format!(
                "{INCOMPATIBLE_STATE_PREFIX}{format}\t{STATE_FORMAT}"
            ));
        }
        let mut shell: Shell = serde_json::from_slice(json).map_err(|e| e.to_string())?;
        register_builtins(&mut shell);
        *lock(&SHELL) = Some(shell);
        Ok(())
    }

    async fn clear_state() {
        *lock(&SHELL) = None;
    }

    fn finalize_preinit() {}

    async fn set_result_variable(name: String) {
        *lock(&RESULT_VAR) = name;
    }
}

export!(Sandbox);

/// Launches external commands as eryx child components, streaming the
/// shell's file descriptors to and from them.
#[derive(Clone, Default)]
struct EryxLauncher;

impl ProcessLauncher for EryxLauncher {
    fn spawn(&self, request: LaunchRequest) -> std::io::Result<ChildProcess> {
        use eryx::process::process::{Child, ProcessError, Spawned};
        let name = request
            .program
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&request.argv0)
            .to_owned();
        let cwd = request.working_dir.to_string_lossy().into_owned();
        let (stdin_tx, stdin_rx) = wit_stream::new::<u8>();
        let Spawned {
            child,
            stdout,
            stderr,
        } = match Child::spawn(&name, &request.args, &request.env, &cwd, stdin_rx) {
            Ok(spawned) => spawned,
            Err(ProcessError::CommandNotFound) => return Err(std::io::ErrorKind::NotFound.into()),
            Err(e) => return Err(std::io::Error::other(format!("{name}: {}", e.name()))),
        };
        let mut files = request.open_files;
        // The pumps and the wait run as tasks of their own, concurrently with
        // each other and the rest of the pipeline.
        let feed = tokio::task::spawn_local(feed(files.remove_fd(OpenFiles::STDIN_FD), stdin_tx));
        let out = tokio::task::spawn_local(drain(stdout, files.remove_fd(OpenFiles::STDOUT_FD)));
        let err = tokio::task::spawn_local(drain(stderr, files.remove_fd(OpenFiles::STDERR_FD)));
        let wait = tokio::task::spawn_local(async move { child.wait().await });
        Ok(ChildProcess::from_future(async move {
            let code = match wait.await.map_err(std::io::Error::other)? {
                Ok(code) => u8::try_from(code & 0xff).unwrap_or(1),
                // It crashed (e.g. trapped, or failed to grow its memory):
                // a failed command, as a process killed by SIGABRT would be.
                Err(e) => {
                    eprintln!("{name}: {}", e.name());
                    134
                }
            };
            // Its stdin is no use once it has exited.
            feed.abort();
            out.await.map_err(std::io::Error::other)??;
            err.await.map_err(std::io::Error::other)??;
            Ok(ExecutionResult::new(code))
        }))
    }
}

/// Copy `file` into a child's stdin until either ends.
async fn feed(
    file: Option<brush_core::openfiles::OpenFile>,
    mut stdin: wit_bindgen::StreamWriter<u8>,
) {
    use std::io::Read as _;
    let Some(mut file) = file else {
        return;
    };
    let mut buf = vec![0; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                note_read();
                // Leftovers mean the child stopped reading.
                if !stdin.write_all(buf[..n].to_vec()).await.is_empty() {
                    return;
                }
            }
            // An empty wasm pipe reports WouldBlock while its writer still
            // runs; yield so that stage can make progress.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => wait_for_input().await,
            Err(_) => return,
        }
    }
}

/// Copy a child's output stream into `file` until the child closes it.
async fn drain(
    mut stream: wit_bindgen::StreamReader<u8>,
    file: Option<brush_core::openfiles::OpenFile>,
) -> std::io::Result<()> {
    use std::io::Write as _;
    /// Counts this pump in [`PUMPS`], and wakes waiting readers when it ends.
    struct Pumping;
    impl Drop for Pumping {
        fn drop(&mut self) {
            PUMPS.set(PUMPS.get() - 1);
            PUMPED.with(|p| p.notify_waiters());
        }
    }
    PUMPS.set(PUMPS.get() + 1);
    let _pumping = Pumping;
    let mut file = file;
    loop {
        let (result, data) = stream.read(Vec::with_capacity(64 * 1024)).await;
        if let Some(out) = file.as_mut() {
            match out.write_all(&data) {
                Ok(()) => PUMPED.with(|p| p.notify_waiters()),
                // A reader that went away (`| head`) is not this command's
                // failure: drop the stream, so the child sees a broken pipe.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
                Err(e) => return Err(e),
            }
            // Let the reader catch up before reading more from the child. A
            // builtin reader doesn't signal, so check again every 1 ms.
            let read = READ.with(Arc::clone);
            while out.pending_bytes().is_some_and(|n| n > PIPE_HIGH_WATER) {
                // An empty write fails once every reader is gone.
                if out.write(&[]).is_err() {
                    return Ok(());
                }
                signalled_or_soon(&read).await;
            }
        }
        if !matches!(result, wit_bindgen::StreamResult::Complete(_)) {
            break;
        }
    }
    if let Some(mut out) = file {
        let _ = out.flush();
    }
    Ok(())
}

/// Runs a host callback as a command; see [`call`] for the mapping.
struct InvokeCommand {
    /// The callback name and its arguments.
    args: Vec<String>,
    /// Whether this ran as `invoke NAME`, rather than as `NAME`.
    via_invoke: bool,
}

impl builtins::ParseArgs for InvokeCommand {
    fn content(
        name: &str,
        content_type: builtins::ContentType,
        _options: &builtins::ContentOptions,
    ) -> Result<String, brush_core::Error> {
        match content_type {
            builtins::ContentType::ManPage => error::unimp("no man page"),
            _ => Ok(format!("{name} - invoke the eryx host callback")),
        }
    }

    fn parse(invocation: builtins::Invocation<'_>) -> Result<Self, builtins::UsageError> {
        let name = invocation.name().to_owned();
        let mut args: Vec<String> = invocation
            .into_args()
            .iter()
            .map(ToString::to_string)
            .collect();
        let via_invoke = name == "invoke";
        if !via_invoke {
            args.insert(0, name);
        }
        Ok(Self { args, via_invoke })
    }
}

impl<SE: ShellExtensions> builtins::Builtin<SE> for InvokeCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let Some((name, args)) = self.args.split_first() else {
            writeln!(context.stderr(), "usage: invoke NAME [ARG]...")?;
            return Ok(ExecutionResult::new(2));
        };
        let def = CALLBACKS.with(|c| c.borrow().get(name).cloned());
        if def.is_none() {
            let under_prefix =
                PREFIXES.with(|p| p.borrow().iter().any(|ns| name.starts_with(ns.as_str())));
            if !under_prefix {
                writeln!(context.stderr(), "invoke: {name}: no such callback")?;
                return Ok(ExecutionResult::new(127));
            }
        }
        // `invoke NAME '{...}'`: the object is the arguments, as is.
        let raw = match args {
            [json] if self.via_invoke => match serde_json::from_str(json) {
                Ok(v @ serde_json::Value::Object(_)) => Some(v),
                _ => None,
            },
            _ => None,
        };
        let arguments = match raw
            .map(call::Call::Invoke)
            .map_or_else(|| call::parse(def.as_ref(), args), Ok)
        {
            Ok(call::Call::Invoke(v)) => v,
            Ok(call::Call::Help) => {
                write!(context.stdout(), "{}", call::help(name, def.as_ref()))?;
                return Ok(ExecutionResult::new(0));
            }
            Err(e) => {
                writeln!(context.stderr(), "{name}: {e}")?;
                writeln!(
                    context.stderr(),
                    "usage: {name} [ARG]... [--key value]... [--json '{{...}}']"
                )?;
                return Ok(ExecutionResult::new(2));
            }
        };
        // Trace events in the CPython and QuickJS guests' shape: the
        // arguments are callback_start's context.
        let arguments = arguments.to_string();
        report_trace(
            0,
            &serde_json::json!({ "type": "callback_start", "name": name }).to_string(),
            &arguments,
        );
        // Sync builtin, async import: park on the call with waitable-set.wait.
        let outcome = wit_bindgen::block_on(invoke(name.clone(), arguments));
        let end = match &outcome {
            Ok(_) => serde_json::json!({ "type": "callback_end", "name": name }),
            Err(e) => serde_json::json!({ "type": "callback_end", "name": name, "error": e }),
        };
        report_trace(0, &end.to_string(), "");
        match outcome {
            Ok(out) => {
                if let Some(text) = call::render(&out) {
                    writeln!(context.stdout(), "{text}")?;
                }
                Ok(ExecutionResult::new(0))
            }
            Err(e) => {
                writeln!(context.stderr(), "{name}: {e}")?;
                Ok(ExecutionResult::new(1))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The shell's reserved words, as brush-core's `keywords.rs` lists them
    /// (it doesn't export them). A command can't take one of these names:
    /// it's parsed as syntax.
    const KEYWORDS: [&str; 22] = [
        "if", "then", "else", "elif", "fi", "case", "esac", "for", "select", "while", "until",
        "do", "done", "in", "function", "time", "{", "}", "!", "[[", "]]", "coproc",
    ];

    /// The host refuses callbacks named like these builtins and reserved
    /// words (#962), from its own copy of the list; this keeps it in step
    /// with what the guest registers.
    #[test]
    fn builtins_match_the_host_list() {
        let mut builtins: Vec<String> =
            brush_builtins::default_builtins::<super::Ext>(brush_builtins::BuiltinSet::BashMode)
                .into_keys()
                .chain(["invoke".to_string(), "grep".to_string()])
                .chain(KEYWORDS.iter().map(|k| (*k).to_string()))
                .collect();
        builtins.sort();
        builtins.dedup();
        let host: Vec<&str> = include_str!("../../eryx/src/reserved/shell.txt")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        assert_eq!(host, builtins, "update crates/eryx/src/reserved/shell.txt");
    }
}
