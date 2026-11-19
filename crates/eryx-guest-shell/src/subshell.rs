//! `sh` and `bash` as commands: a fresh brush shell, nested in this guest.
//!
//! A nested shell behaves like a child process. It sees only the exported
//! environment, the file descriptors and the working directory it's
//! launched with, and nothing it changes (variables, functions, options,
//! traps, cwd) flows back. It shares this guest's memory, filesystem,
//! callbacks and host commands, and it's never part of a snapshot.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{Read as _, Write as _};
use std::path::Path;

use brush_core::namedoptions::{self, ShellOptionKind};
use brush_core::openfiles::{self, OpenFile};
use brush_core::{ErrorKind, ExecutionExitCode, ShellFd, ShellVariable, SourceInfo};

use crate::{EryxLauncher, Ext, Shell};

/// The names the nested shell answers to.
pub(crate) const NAMES: [&str; 2] = ["sh", "bash"];

/// How deep shells may nest. Past this a nested shell exits 126 rather than
/// grow the guest's memory until it traps.
const MAX_DEPTH: usize = 16;

/// What a nested shell runs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `-c CMD [NAME [ARG]...]`: `NAME` is `$0`.
    Command {
        command: String,
        name: Option<String>,
        args: Vec<String>,
    },
    /// `FILE [ARG]...`.
    Script { path: String, args: Vec<String> },
    /// `-s [ARG]...`, or no operands: commands come from stdin.
    Stdin { args: Vec<String> },
}

/// A nested shell's command line, parsed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Args {
    pub(crate) mode: Mode,
    /// `set` options to turn on (`true`) or off: one-letter names from
    /// `-e`/`+e`, long ones from `-o NAME`/`+o NAME`.
    pub(crate) options: Vec<(String, bool)>,
}

/// Parse `sh`/`bash` arguments (not including `argv[0]`), as bash does:
/// options up to the first operand or `--`, then the operands.
///
/// # Errors
///
/// A message for an unknown option, or `-c`/`-o` without its argument.
pub(crate) fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut command_mode = false;
    let mut stdin_mode = false;
    let mut options = Vec::new();
    let mut rest = args.iter();
    let mut operands: Vec<String> = Vec::new();
    while let Some(arg) = rest.next() {
        let on = match arg.as_bytes().first() {
            Some(b'-') => true,
            Some(b'+') => false,
            _ => {
                operands.push(arg.clone());
                break;
            }
        };
        // `-` alone, like `--`, ends the options.
        if arg == "--" || arg == "-" {
            break;
        }
        if arg.starts_with("--") {
            return Err(format!("{arg}: invalid option"));
        }
        for letter in arg[1..].chars() {
            match letter {
                'c' if on => command_mode = true,
                's' if on => stdin_mode = true,
                'o' => {
                    let name = rest
                        .next()
                        .ok_or_else(|| format!("{}o: option requires an argument", &arg[..1]))?;
                    if namedoptions::options(ShellOptionKind::SetO)
                        .get(name)
                        .is_none()
                    {
                        return Err(format!("{name}: invalid option name"));
                    }
                    options.push((name.clone(), on));
                }
                // `-c`/`-s` take no `+` form; anything else must be a
                // `set` letter.
                _ if letter != 'c'
                    && letter != 's'
                    && namedoptions::options(ShellOptionKind::Set)
                        .get(letter.encode_utf8(&mut [0; 4]))
                        .is_some() =>
                {
                    options.push((letter.to_string(), on));
                }
                _ => return Err(format!("{}{letter}: invalid option", &arg[..1])),
            }
        }
    }
    operands.extend(rest.cloned());
    let mut operands = operands.into_iter();
    let mode = if command_mode {
        let command = operands.next().ok_or("-c: option requires an argument")?;
        Mode::Command {
            command,
            name: operands.next(),
            args: operands.collect(),
        }
    } else if stdin_mode {
        Mode::Stdin {
            args: operands.collect(),
        }
    } else {
        match operands.next() {
            Some(path) => Mode::Script {
                path,
                args: operands.collect(),
            },
            None => Mode::Stdin { args: vec![] },
        }
    };
    Ok(Args { mode, options })
}

/// Run `argv0` (`sh` or `bash`) with `args` as a nested shell, `depth`
/// shells deep, and return its exit status. `fds` are its open files; a
/// standard stream missing from them is the null device. Call it under the
/// component's executor (see `run_local`): the shell's pipeline stages and
/// children run as `spawn_local` tasks. With `wait_for_jobs` (a shell run as
/// a process of its own, whose jobs would die with it) it waits for its
/// background jobs before it returns, as an execution does; without, they
/// keep running after it, until the execution's end waits for them.
pub(crate) async fn run_shell(
    argv0: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    mut fds: HashMap<ShellFd, OpenFile>,
    depth: usize,
    wait_for_jobs: bool,
) -> u8 {
    for fd in [0, 1, 2] {
        if let Entry::Vacant(slot) = fds.entry(fd)
            && let Ok(null) = openfiles::null()
        {
            slot.insert(null);
        }
    }
    let mut stderr = fds.get(&2).cloned();
    let mut complain = |message: &str| {
        if let Some(err) = stderr.as_mut() {
            let _ = writeln!(err, "{argv0}: {message}");
        }
    };
    if depth >= MAX_DEPTH {
        complain(&format!("shells nested more than {MAX_DEPTH} deep"));
        return 126;
    }
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            complain(&e);
            complain(
                "usage: sh [-eux] [-o OPTION] [-c COMMAND [NAME [ARG]...] | -s [ARG]... | FILE [ARG]...]",
            );
            return 2;
        }
    };
    let script = match &parsed.mode {
        // Buffered whole: fine for `… | bash`, the usual case.
        Mode::Stdin { .. } => match read_all(fds.get(&0).cloned()).await {
            Ok(script) => Some(script),
            Err(e) => {
                complain(&format!("stdin: {e}"));
                return 1;
            }
        },
        _ => None,
    };
    let sh = argv0 == "sh";
    let (name, positional) = match &parsed.mode {
        Mode::Command { name, args, .. } => (name.clone(), Some(args.clone())),
        Mode::Stdin { args } => (None, Some(args.clone())),
        Mode::Script { .. } => (None, None),
    };
    let mut builder = brush_core::Shell::builder_with_extensions::<Ext>()
        .process_launcher(EryxLauncher { depth: depth + 1 })
        .do_not_inherit_env(true)
        .fds(fds)
        .working_dir(cwd.to_path_buf())
        .shell_name(name.unwrap_or_else(|| argv0.to_owned()))
        .maybe_shell_args(positional)
        .command_string_mode(matches!(parsed.mode, Mode::Command { .. }))
        .posix(sh)
        .sh_mode(sh)
        .profile(brush_core::ProfileLoadBehavior::Skip)
        .rc(brush_core::RcLoadBehavior::Skip);
    for (name, value) in env {
        let mut var = ShellVariable::new(value.as_str());
        var.export();
        builder = builder.var(name.clone(), var);
    }
    let mut shell: Shell = match builder.build().await {
        Ok(shell) => shell,
        Err(e) => {
            complain(&e.to_string());
            return 1;
        }
    };
    for (name, on) in &parsed.options {
        let kind = if name.len() == 1 {
            ShellOptionKind::Set
        } else {
            ShellOptionKind::SetO
        };
        if let Some(option) = namedoptions::options(kind).get(name) {
            option.set(shell.options_mut(), *on);
        }
    }
    crate::register_builtins(&mut shell);
    let result = match parsed.mode {
        Mode::Command { command, .. } => shell.run_dash_c_command(command).await,
        Mode::Script { path, args } => shell.run_script(Path::new(&path), args.into_iter()).await,
        Mode::Stdin { .. } => {
            let params = shell.default_exec_params();
            shell
                .run_string(
                    script.unwrap_or_default(),
                    &SourceInfo::from("stdin"),
                    &params,
                )
                .await
        }
    };
    let status = match result {
        Ok(_) => shell.last_exit_status(),
        Err(e) => {
            let _ = shell.display_error(&mut shell.stderr(), &e);
            // As bash: 127 for a script that isn't there, 126 for one it
            // can't read (a directory).
            match e.kind() {
                ErrorKind::FailedSourcingFile(_, io)
                    if io.kind() == std::io::ErrorKind::NotFound =>
                {
                    127
                }
                ErrorKind::FailedSourcingFile(..) => 126,
                _ => u8::from(ExecutionExitCode::from(&e)),
            }
        }
    };
    if wait_for_jobs {
        let time_left = crate::eryx::process::execution::time_left();
        let stopped = crate::jobs::finish_jobs(shell.jobs_mut(), time_left).await;
        crate::report_stopped_jobs(stopped);
    } else {
        crate::jobs::orphan(shell.jobs_mut());
    }
    status
}

/// Read `file` to its end, waiting out an empty pipe whose writer still runs.
async fn read_all(file: Option<OpenFile>) -> std::io::Result<String> {
    let mut data = Vec::new();
    let Some(mut file) = file else {
        return Ok(String::new());
    };
    let mut buf = vec![0; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                crate::note_read();
                data.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => crate::wait_for_input().await,
            Err(e) => return Err(e),
        }
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{Args, Mode, parse_args};

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn command_mode_takes_name_and_args() {
        let args = parse(&["-ec", "echo $0 $1", "me", "a"]).unwrap();
        assert_eq!(
            args.mode,
            Mode::Command {
                command: "echo $0 $1".into(),
                name: Some("me".into()),
                args: strings(&["a"]),
            }
        );
        assert_eq!(args.options, [("e".to_owned(), true)]);
        assert!(parse(&["-c"]).is_err());
    }

    #[test]
    fn script_and_stdin_modes() {
        assert_eq!(
            parse(&["-x", "f.sh", "-e", "b"]).unwrap().mode,
            Mode::Script {
                path: "f.sh".into(),
                args: strings(&["-e", "b"]),
            }
        );
        assert_eq!(
            parse(&["--", "-f.sh"]).unwrap().mode,
            Mode::Script {
                path: "-f.sh".into(),
                args: vec![],
            }
        );
        assert_eq!(parse(&[]).unwrap().mode, Mode::Stdin { args: vec![] });
        assert_eq!(
            parse(&["-s", "a"]).unwrap().mode,
            Mode::Stdin {
                args: strings(&["a"]),
            }
        );
    }

    #[test]
    fn long_options_and_unknown_ones() {
        let args = parse(&["-o", "pipefail", "+o", "errexit", "-c", "true"]).unwrap();
        assert_eq!(
            args.options,
            [("pipefail".to_owned(), true), ("errexit".to_owned(), false)]
        );
        assert!(parse(&["-o", "nope", "-c", "true"]).is_err());
        assert!(parse(&["-Z"]).is_err());
        assert!(parse(&["--login"]).is_err());
        assert!(parse(&["-o"]).is_err());
    }
}
