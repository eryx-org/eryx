//! Input for the text-processing builtins (`grep`): stdin or files,
//! resolved like the shell resolves paths.

use std::io::Read as _;

use brush_core::{ExecutionContext, ShellExtensions};

/// Read all of stdin. In a pipeline an empty wasm pipe reports `WouldBlock`
/// while its writer still runs; see [`crate::wait_for_input`].
pub async fn read_stdin<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
) -> std::io::Result<Vec<u8>> {
    let mut stdin = context.stdin();
    let mut data = Vec::new();
    let mut buf = [0; 64 * 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return Ok(data),
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => crate::wait_for_input().await,
            Err(e) => return Err(e),
        }
    }
}

/// Read `path`, relative to the shell's working directory.
pub fn read_file<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    path: &str,
) -> std::io::Result<Vec<u8>> {
    std::fs::read(context.shell.absolute_path(path))
}
