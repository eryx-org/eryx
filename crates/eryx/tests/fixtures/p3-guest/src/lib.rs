//! A WASI 0.3 guest for eryx's sandbox world. Each line of `code` is a
//! command:
//!
//! - `print TEXT`: write `TEXT` to stdout;
//! - `write PATH TEXT`, `read PATH`: write or print a file;
//! - `connect ADDR`: try a TCP connection, and print `connected` or the error
//!   kind.
//!
//! Blank lines and `#` lines (the host's preamble header) are skipped; any
//! other line, or a command that fails, fails the execution.

use std::io::Write as _;

wit_bindgen::generate!({
    path: "../../../../eryx-runtime/wit",
    world: "eryx:sandbox/sandbox",
    generate_all,
});

struct Sandbox;

fn run(line: &str) -> Result<(), String> {
    let mut out = std::io::stdout();
    match line.split_once(' ').unwrap_or((line, "")) {
        ("print", text) => writeln!(out, "{text}").map_err(|e| e.to_string()),
        ("write", rest) => {
            let (path, text) = rest.split_once(' ').unwrap_or((rest, ""));
            std::fs::write(path, text).map_err(|e| format!("write {path}: {e}"))
        }
        ("read", path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
            writeln!(out, "{text}").map_err(|e| e.to_string())
        }
        ("connect", addr) => {
            let result = match std::net::TcpStream::connect(addr) {
                Ok(_) => "connected".to_owned(),
                Err(e) => format!("{:?}", e.kind()),
            };
            writeln!(out, "{result}").map_err(|e| e.to_string())
        }
        _ => Err(format!("unknown command: {line}")),
    }
}

impl Guest for Sandbox {
    async fn execute(code: String) -> Result<ExecuteOutput, ExecuteError> {
        let result = code
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .try_for_each(run);
        let _ = std::io::stdout().flush();
        result
            .map(|()| ExecuteOutput {
                stdout: vec![],
                stderr: vec![],
                result_json: String::new(),
                result_error: String::new(),
            })
            .map_err(|message| ExecuteError {
                message,
                exit_code: None,
                stdout: vec![],
                stderr: vec![],
            })
    }

    async fn snapshot_state() -> Result<StateSnapshot, String> {
        Err("no state".into())
    }

    async fn restore_state(_data: Vec<u8>) -> Result<(), String> {
        Err("no state".into())
    }

    async fn clear_state() {}

    fn finalize_preinit() {}

    async fn set_result_variable(_name: String) {}

    async fn run_command(_args: Vec<String>, _env: Vec<(String, String)>, _cwd: String) -> i32 {
        127
    }
}

export!(Sandbox);
