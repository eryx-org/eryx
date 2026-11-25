//! The host terminal in raw mode while a guest shell owns it, through
//! `stty` (no termios dependency for a spike).

use std::process::{Command, Stdio};
use std::sync::Mutex;

/// `stty -g` from before raw mode, to put back.
static SAVED: Mutex<Option<String>> = Mutex::new(None);

fn stty(args: &[&str]) -> Option<String> {
    let out = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Raw mode, no echo. Returns the terminal's (rows, cols).
pub fn raw() -> Option<(u16, u16)> {
    let saved = stty(&["-g"])?;
    stty(&["raw", "-echo"])?;
    *SAVED.lock().unwrap() = Some(saved);
    let size = stty(&["size"])?;
    let (rows, cols) = size.split_once(' ')?;
    Some((rows.parse().ok()?, cols.parse().ok()?))
}

pub fn is_raw() -> bool {
    SAVED.lock().unwrap().is_some()
}

/// Puts the terminal back as it was; harmless when it never left.
pub fn restore() {
    if let Some(saved) = SAVED.lock().unwrap().take() {
        stty(&[&saved]);
    }
}
