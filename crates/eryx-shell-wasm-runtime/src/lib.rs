//! SPIKE: a bash-compatible shell (brush) as an eryx guest.
//!
//! Exports the same `eryx:sandbox/sandbox` world as the CPython and QuickJS
//! guests, plus one extra import, `eryx:process`, for external commands:
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

use brush_core::{
    ExecutionContext, ExecutionParameters, ExecutionResult, Shell, ShellExtensions, SourceInfo,
    builtins, error,
};

wit_bindgen::generate!({
    path: "wit",
    world: "eryx:shell-guest/shell-sandbox",
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

/// Register brush's bash builtins plus one `invoke` builtin per host callback.
fn register_builtins(shell: &mut Shell) {
    for (name, reg) in brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode) {
        shell.register_builtin(name, reg);
    }
    shell.register_builtin("invoke", builtins::simple_builtin::<InvokeCommand, _>());
    for cb in list_callbacks() {
        shell.register_builtin(cb.name, builtins::simple_builtin::<InvokeCommand, _>());
    }
}

fn new_shell() -> Shell {
    brush_core::sys::process::set_spawn_handler(Box::new(spawn));
    let mut shell = block_on(Shell::builder().build()).expect("failed to create shell");
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

struct Sandbox;

impl Guest for Sandbox {
    async fn execute(code: String) -> Result<ExecuteOutput, ExecuteError> {
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
                stdout: vec![],
                stderr: vec![],
            }),
            Err(e) => Err(ExecuteError {
                message: e.to_string(),
                stdout: vec![],
                stderr: vec![],
            }),
        }
    }

    async fn snapshot_state() -> Result<StateSnapshot, String> {
        let data = with_shell(|shell| serde_json::to_vec(shell)).map_err(|e| e.to_string())?;
        Ok(StateSnapshot {
            data,
            skipped_globals: vec![],
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

/// brush's external-command hook: run `cmd` as a host child component.
fn spawn(
    cmd: &str,
    args: &[String],
    env: Vec<(String, String)>,
    cwd: &str,
) -> brush_core::sys::process::SpawnResult {
    use brush_core::sys::process::SpawnError;
    use eryx::process::process::{Child, ProcessError};
    match Child::spawn(cmd, args, &env, cwd) {
        Ok(child) => Ok(Box::new(ChildHandle(child))),
        Err(ProcessError::CommandNotFound) => Err(SpawnError::NotFound(cmd.to_owned())),
        Err(e) => Err(SpawnError::Failed(format!("{e:?}"))),
    }
}

struct ChildHandle(eryx::process::process::Child);

fn io_err(e: eryx::process::process::ProcessError) -> std::io::Error {
    std::io::Error::other(format!("{e:?}"))
}

impl brush_core::sys::process::ExternalChild for ChildHandle {
    fn write_stdin(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.write_stdin(data).map(|n| n as usize).map_err(io_err)
    }

    fn close_stdin(&mut self) {
        self.0.close_stdin();
    }

    fn read_stdout(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let data = self.0.read_stdout(buf.len() as u32).map_err(io_err)?;
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    fn read_stderr(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let data = self.0.read_stderr(buf.len() as u32).map_err(io_err)?;
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    fn wait(&mut self) -> std::io::Result<i32> {
        self.0.wait().map_err(io_err)
    }
}

/// Runs a host callback: `<callback> [--key value]... [--json '{..}']`, or
/// `invoke <callback> [json]`. Prints the JSON result (a bare string unquoted).
struct InvokeCommand;

impl builtins::SimpleCommand for InvokeCommand {
    fn get_content(
        name: &str,
        content_type: builtins::ContentType,
        _options: &builtins::ContentOptions,
    ) -> Result<String, brush_core::Error> {
        match content_type {
            builtins::ContentType::ManPage => error::unimp("no man page"),
            _ => Ok(format!("{name} - invoke the eryx host callback")),
        }
    }

    fn execute<SE: ShellExtensions, I: Iterator<Item = S>, S: AsRef<str>>(
        context: ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<ExecutionResult, brush_core::Error> {
        let mut args: Vec<String> = args.map(|s| s.as_ref().to_owned()).collect();
        if args.first().is_some_and(|a| a == "invoke") {
            args.remove(0);
        }
        let parsed = match args.split_first() {
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
        match wit_bindgen::block_on(invoke(name, json)) {
            Ok(out) => {
                let text = match serde_json::from_str::<serde_json::Value>(&out) {
                    Ok(serde_json::Value::String(s)) => s,
                    _ => out,
                };
                writeln!(context.stdout(), "{text}")?;
                Ok(ExecutionResult::new(0))
            }
            Err(e) => {
                writeln!(context.stderr(), "{}: {e}", args[0])?;
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
