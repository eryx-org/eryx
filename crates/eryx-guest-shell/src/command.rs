//! Command callbacks (#1241): host callbacks that run as real commands.
//!
//! The host marks one with `"x-eryx-command": true` in its parameters
//! schema. Its arguments go to the host as given, with its stdin, as
//! `{"args": [...], "stdin": "<base64>"}`, and the host returns
//! `{"stdout": "<base64>", "stderr": "<base64>", "exit_code": N}`, which is
//! written to the command's stdout and stderr and becomes its status.
//!
//! Stdin is read to its end before the call, up to [`MAX_COMMAND_STDIN`]:
//! the callback protocol is one request and one response, and a replay
//! journal keys each call on its whole input. Like `ssh`, the command so
//! consumes input it may not need.

use std::io::{Read as _, Write as _};

use brush_core::{ExecutionContext, ExecutionResult, ShellExtensions};
use serde_json::Value;

/// Most bytes of stdin a command callback gets. Must match the host's
/// `MAX_COMMAND_STDIN_BYTES`.
const MAX_COMMAND_STDIN: usize = 8 * 1024 * 1024;

/// The schema key that marks a command callback. Must match the host's
/// `COMMAND_SCHEMA_KEY`.
const COMMAND_SCHEMA_KEY: &str = "x-eryx-command";

/// Whether a callback's parameters schema marks it as a command callback.
pub(crate) fn is_command(schema: Option<&Value>) -> bool {
    schema.and_then(|s| s.get(COMMAND_SCHEMA_KEY)) == Some(&Value::Bool(true))
}

/// Run the command callback `name` with `args`.
pub(crate) async fn run<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    name: &str,
    args: &[String],
) -> Result<ExecutionResult, brush_core::Error> {
    let stdin = match read_stdin(context).await {
        Ok(Some(stdin)) => stdin,
        Ok(None) => {
            writeln!(
                context.stderr(),
                "{name}: stdin is over the {} MiB limit",
                MAX_COMMAND_STDIN >> 20
            )?;
            return Ok(ExecutionResult::new(1));
        }
        Err(e) => {
            writeln!(context.stderr(), "{name}: reading stdin: {e}")?;
            return Ok(ExecutionResult::new(1));
        }
    };
    // Keys in sorted order: the canonical form replay journals key on.
    let arguments = serde_json::json!({ "args": args, "stdin": encode(&stdin) }).to_string();
    // The trace gets the arguments, not the input, which may be large.
    let traced = serde_json::json!({ "args": args }).to_string();
    let result = match crate::call_host(name, arguments, &traced) {
        Ok(result) => result,
        Err(e) => {
            writeln!(context.stderr(), "{name}: {e}")?;
            return Ok(ExecutionResult::new(1));
        }
    };
    let Some((stdout, stderr, code)) = decode_result(&result) else {
        writeln!(context.stderr(), "{name}: the host sent a malformed result")?;
        return Ok(ExecutionResult::new(1));
    };
    write_all(context.stdout(), &stdout)?;
    write_all(context.stderr(), &stderr)?;
    Ok(ExecutionResult::new(u8::try_from(code & 0xff).unwrap_or(1)))
}

/// Write `data` to `file`, as a command whose reader went away (`| head`)
/// would: what doesn't fit is dropped, and it's not this command's failure.
fn write_all(mut file: impl std::io::Write, data: &[u8]) -> std::io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    match file.write_all(data).and_then(|()| file.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// All of stdin, or `None` once it's over [`MAX_COMMAND_STDIN`]. As
/// [`crate::input::read_stdin`], waiting on an empty pipe whose writer runs.
async fn read_stdin<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut stdin = context.stdin();
    let mut data = Vec::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return Ok(Some(data)),
            Ok(n) => {
                crate::note_read();
                data.extend_from_slice(&buf[..n]);
                if data.len() > MAX_COMMAND_STDIN {
                    return Ok(None);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => crate::wait_for_input().await,
            Err(e) => return Err(e),
        }
    }
}

/// A command result's stdout, stderr and exit code.
fn decode_result(json: &str) -> Option<(Vec<u8>, Vec<u8>, i32)> {
    let value: Value = serde_json::from_str(json).ok()?;
    let bytes = |key| match value.get(key) {
        None => Some(Vec::new()),
        Some(v) => decode(v.as_str()?),
    };
    let code = match value.get("exit_code") {
        None => 0,
        Some(v) => i32::try_from(v.as_i64()?).ok()?,
    };
    Some((bytes("stdout")?, bytes("stderr")?, code))
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64, with padding.
fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode standard base64 with padding; `None` if it isn't.
fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for (i, chunk) in text.chunks(4).enumerate() {
        let last = i + 1 == text.len() / 4;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            let v = ALPHABET.iter().position(|&a| a == c)?;
            n = n << 6 | u32::try_from(v).ok()?;
        }
        n <<= 6 * pad;
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - pad]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for (raw, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (&[0xff, 0x00, 0xfe][..], "/wD+"),
        ] {
            assert_eq!(encode(raw), text);
            assert_eq!(decode(text).as_deref(), Some(raw));
        }
        assert_eq!(decode("Zg="), None);
        assert_eq!(decode("Zg==Zg=="), None);
        assert_eq!(decode("Z!=="), None);
    }

    #[test]
    fn results_decode() {
        assert_eq!(
            decode_result(r#"{"stdout":"aGk=","stderr":"","exit_code":3}"#),
            Some((b"hi".to_vec(), vec![], 3))
        );
        assert_eq!(decode_result("{}"), Some((vec![], vec![], 0)));
        assert_eq!(decode_result(r#"{"stdout":1}"#), None);
    }
}
