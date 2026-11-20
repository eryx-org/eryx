//! Output into a closed pipe (`eryx-precompile ... | head`) ends the program
//! quietly instead of panicking (#1135).

use std::process::{Command, Stdio};

#[test]
fn inspect_into_a_closed_pipe_exits_quietly() -> std::io::Result<()> {
    let dir = tempfile::tempdir()?;
    // An empty component: inspect prints several lines about it.
    let wasm = dir.path().join("empty.wasm");
    std::fs::write(&wasm, b"\0asm\x0d\0\x01\0")?;

    let (reader, writer) = std::io::pipe()?;
    drop(reader);
    let output = Command::new(env!("CARGO_BIN_EXE_eryx-precompile"))
        .args(["commands", "inspect"])
        .arg(&wasm)
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    Ok(())
}
