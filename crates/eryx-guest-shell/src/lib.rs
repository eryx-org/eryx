//! A bash-compatible shell (brush) as an eryx guest.
//!
//! Exports the same `eryx:sandbox/sandbox` world as the CPython and QuickJS
//! guests. With the `process` feature it also imports `eryx:process` for
//! external commands; the host doesn't provide that interface yet (#809), so
//! the feature is off by default and unknown commands exit 127:
//!
//! - `execute` runs a script on a persistent brush `Shell`. Output goes to the
//!   guest's WASI fd 1/2, which the host already captures.
//! - Host callbacks are shell commands: `get_time`, `add --a 2 --b 3`,
//!   `add --json '{"a":2}'`, or `invoke add '{"a":2}'`. The JSON result goes to
//!   stdout, so `$(add --a 2 --b 3 | ...)` composes like any other command.
//! - A non-zero exit status becomes an `execute-error` ("exit status N").
//! - `snapshot-state` is brush's own serde form of the `Shell`.
//! - Anything that isn't a builtin, function or callback is spawned through
//!   `eryx:process` (host-registered WASI commands, e.g. uutils coreutils).

#![allow(clippy::expect_used)] // single-threaded guest: a broken invariant traps either way

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write as _;

use brush_core::extensions::{
    DefaultErrorFormatter, DefaultTaskSpawner, ProcessLauncher, ShellExtensionsImpl,
};
#[cfg(feature = "process")]
use brush_core::openfiles::OpenFiles;
use brush_core::processes::{ChildProcess, LaunchRequest};
use brush_core::{
    ExecutionContext, ExecutionParameters, ExecutionResult, ShellExtensions, SourceInfo, builtins,
    error,
};

/// brush with external commands launched as eryx child components.
type Ext = ShellExtensionsImpl<DefaultErrorFormatter, EryxLauncher, DefaultTaskSpawner>;
type Shell = brush_core::Shell<Ext>;

/// Where host commands appear to live. Nothing is there: the shell finds them
/// through its command table, which `register_builtins` seeds.
#[cfg(feature = "process")]
const COMMAND_DIR: &str = "/eryx/bin";

#[cfg(feature = "process")]
wit_bindgen::generate!({
    path: "wit",
    world: "eryx:shell-guest/shell-sandbox",
    generate_all,
});

#[cfg(not(feature = "process"))]
wit_bindgen::generate!({
    path: "wit",
    world: "eryx:sandbox/sandbox",
    generate_all,
});

thread_local! {
    static RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("failed to create tokio runtime");
    static SHELL: RefCell<Option<Shell>> = const { RefCell::new(None) };
    static RESULT_VAR: RefCell<String> = RefCell::new("result".into());
}

fn block_on<F: Future>(f: F) -> F::Output {
    RUNTIME.with(|rt| rt.block_on(f))
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
    for cb in list_callbacks() {
        shell
            .builtins_mut()
            .register(cb.name, builtins::Registration::of::<InvokeCommand>());
    }
    #[cfg(feature = "process")]
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
    SHELL.with(|s| f(s.borrow_mut().get_or_insert_with(new_shell)))
}

/// Read and unset the result variable. A value that parses as JSON is passed
/// through; anything else becomes a JSON string.
fn take_result(shell: &mut Shell) -> String {
    let name = RESULT_VAR.with(|r| r.borrow().clone());
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
        // The host sends the shell no preamble, so only the user code is left.
        let (_, code) = split_preamble(&code);
        let code = code.to_string();
        let (status, result_json) = with_shell(|shell| {
            // Callbacks can differ per execution; re-register the current set.
            register_builtins(shell);
            let status = block_on(shell.run_string(
                &code,
                &SourceInfo::default(),
                &ExecutionParameters::default(),
            ))
            .map(|r| u8::from(r.exit_code));
            (status, take_result(shell))
        });
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
        let data = with_shell(|shell| serde_json::to_vec(shell)).map_err(|e| e.to_string())?;
        Ok(StateSnapshot {
            data,
            skipped: vec![],
        })
    }

    async fn restore_state(data: Vec<u8>) -> Result<(), String> {
        let mut shell: Shell = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        register_builtins(&mut shell);
        SHELL.with(|s| *s.borrow_mut() = Some(shell));
        Ok(())
    }

    async fn clear_state() {
        SHELL.with(|s| *s.borrow_mut() = None);
    }

    fn finalize_preinit() {}

    async fn set_result_variable(name: String) {
        RESULT_VAR.with(|r| *r.borrow_mut() = name);
    }
}

export!(Sandbox);

/// Launches external commands as eryx child components, in batch: the child
/// gets all of its stdin once the writer is done, then its output is written
/// to the shell's stdout and stderr.
#[derive(Clone, Default)]
struct EryxLauncher;

impl ProcessLauncher for EryxLauncher {
    #[cfg(not(feature = "process"))]
    fn spawn(&self, _request: LaunchRequest) -> std::io::Result<ChildProcess> {
        Err(std::io::ErrorKind::NotFound.into())
    }

    #[cfg(feature = "process")]
    fn spawn(&self, request: LaunchRequest) -> std::io::Result<ChildProcess> {
        use eryx::process::process::{Child, ProcessError};
        let name = request
            .program
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&request.argv0)
            .to_owned();
        let cwd = request.working_dir.to_string_lossy().into_owned();
        let child = match Child::spawn(&name, &request.args, &request.env, &cwd) {
            Ok(child) => child,
            Err(ProcessError::CommandNotFound) => return Err(std::io::ErrorKind::NotFound.into()),
            Err(e) => return Err(std::io::Error::other(format!("{name}: {e:?}"))),
        };
        let mut files = request.open_files;
        Ok(ChildProcess::from_future(run_child(
            child,
            std::mem::take(&mut files),
        )))
    }
}

#[cfg(feature = "process")]
fn io_err(e: eryx::process::process::ProcessError) -> std::io::Error {
    std::io::Error::other(format!("{e:?}"))
}

#[cfg(feature = "process")]
async fn run_child(
    child: eryx::process::process::Child,
    mut files: OpenFiles,
) -> std::io::Result<ExecutionResult> {
    use std::io::Read as _;
    // An empty wasm pipe reports WouldBlock while its writer still runs; yield
    // so that pipeline stage can make progress, and stop at end-of-file.
    if let Some(mut stdin) = files.remove_fd(OpenFiles::STDIN_FD) {
        let mut buf = [0; 64 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    child.write_stdin(&buf[..n]).map_err(io_err)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::task::yield_now().await;
                }
                Err(e) => return Err(e),
            }
        }
    }
    child.close_stdin();
    let code = child.wait().map_err(io_err)?;
    for (fd, read) in [(OpenFiles::STDOUT_FD, 0u8), (OpenFiles::STDERR_FD, 1u8)] {
        let Some(mut out) = files.remove_fd(fd) else {
            continue;
        };
        loop {
            let data = if read == 0 {
                child.read_stdout(64 * 1024)
            } else {
                child.read_stderr(64 * 1024)
            }
            .map_err(io_err)?;
            if data.is_empty() {
                break;
            }
            // A reader that went away (`| head`) is not this command's failure.
            if let Err(e) = out.write_all(&data) {
                if e.kind() == std::io::ErrorKind::BrokenPipe {
                    break;
                }
                return Err(e);
            }
        }
        let _ = out.flush();
    }
    Ok(ExecutionResult::new(u8::try_from(code & 0xff).unwrap_or(1)))
}

/// Runs a host callback: `<callback> [--key value]... [--json '{..}']`, or
/// `invoke <callback> [json]`. Prints the JSON result (a bare string unquoted).
struct InvokeCommand {
    /// The callback name and its arguments.
    args: Vec<String>,
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
        if name != "invoke" {
            args.insert(0, name);
        }
        Ok(Self { args })
    }
}

impl<SE: ShellExtensions> builtins::Builtin<SE> for InvokeCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let parsed = match self.args.split_first() {
            None => Err("usage: invoke <callback> [json]".to_owned()),
            Some((name, [json])) if !json.starts_with("--") => Ok((name.clone(), json.clone())),
            Some((name, rest)) => parse_flags(rest).map(|v| (name.clone(), v.to_string())),
        };
        let (name, json) = match parsed {
            Ok(p) => p,
            Err(e) => {
                writeln!(context.stderr(), "{e}")?;
                return Ok(ExecutionResult::new(2));
            }
        };
        // Sync builtin, async import: park on the call with waitable-set.wait.
        match wit_bindgen::block_on(invoke(name.clone(), json)) {
            Ok(out) => {
                let text = match serde_json::from_str::<serde_json::Value>(&out) {
                    Ok(serde_json::Value::String(s)) => s,
                    _ => out,
                };
                writeln!(context.stdout(), "{text}")?;
                Ok(ExecutionResult::new(0))
            }
            Err(e) => {
                writeln!(context.stderr(), "{name}: {e}")?;
                Ok(ExecutionResult::new(1))
            }
        }
    }
}

/// `--key value` pairs (values parsed as JSON, else strings) merged over an
/// optional `--json '{..}'` object.
fn parse_flags(args: &[String]) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    let mut extra = HashMap::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let key = arg
            .strip_prefix("--")
            .filter(|k| !k.is_empty())
            .ok_or_else(|| format!("unexpected argument: {arg}"))?;
        let value = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
        if key == "json" {
            match serde_json::from_str(value) {
                Ok(serde_json::Value::Object(o)) => obj = o,
                _ => return Err("--json needs a JSON object".into()),
            }
        } else {
            let v = serde_json::from_str(value)
                .unwrap_or_else(|_| serde_json::Value::String(value.clone()));
            extra.insert(key.to_owned(), v);
        }
    }
    obj.extend(extra);
    Ok(serde_json::Value::Object(obj))
}
