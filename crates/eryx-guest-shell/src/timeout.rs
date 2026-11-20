//! The `timeout` builtin. uutils' `timeout` needs `std::process` and signals,
//! which WASI doesn't have, so the shell does it: the command goes through
//! the shell's own lookup (see [`crate::env::start_program`]) in a task of
//! its own, and if the duration runs out first, that task is stopped as
//! `kill` stops a job (see [`crate::jobs`]), with everything it started and
//! its child commands.

use std::io::Write as _;

use brush_core::commands::ShellForCommand;
use brush_core::{ExecutionContext, ExecutionExitCode, ExecutionResult, ShellExtensions, builtins};

use crate::jobs;

const TRY: &str = "Try 'timeout --help' for more information.";

const HELP: &str = "\
Usage: timeout [OPTION] DURATION COMMAND [ARG]...
Start COMMAND, and stop it if still running after DURATION.

  --preserve-status  on timeout, exit with 128 plus the signal's number
                     rather than 124
  --foreground       accepted and ignored
  -k, --kill-after=DURATION
                     accepted; the command is always stopped at once
  -s, --signal=SIGNAL
                     the signal to report under --preserve-status (TERM by
                     default); WASI has no signals, so any signal stops it
      --help         display this help and exit
      --version      output version information and exit

DURATION is a number with an optional suffix: s for seconds (the default),
m for minutes, h for hours or d for days. 0 disables the timeout.

Exits 124 if the command timed out (and --preserve-status isn't given), 125
if timeout itself fails, 126 if the command can't be run, 127 if it isn't
found, and otherwise with the command's status.
";

/// What `timeout` was asked to do.
#[derive(Debug, PartialEq)]
enum Request {
    Help,
    Version,
    Run(Run),
}

#[derive(Debug, PartialEq)]
struct Run {
    preserve_status: bool,
    /// The signal's number, for the status under `--preserve-status`.
    signal: i32,
    /// How long the command gets, in nanoseconds; `None` for no limit.
    limit_ns: Option<u64>,
    /// The command and its arguments.
    command: Vec<String>,
}

/// A GNU duration in seconds: a non-negative float with an optional `s`,
/// `m`, `h` or `d` suffix.
fn parse_duration(spec: &str) -> Option<f64> {
    let (number, unit) = match spec.as_bytes().last()? {
        b's' => (&spec[..spec.len() - 1], 1.0),
        b'm' => (&spec[..spec.len() - 1], 60.0),
        b'h' => (&spec[..spec.len() - 1], 3600.0),
        b'd' => (&spec[..spec.len() - 1], 86400.0),
        _ => (spec, 1.0),
    };
    let seconds = number.parse::<f64>().ok()? * unit;
    (seconds >= 0.0).then_some(seconds)
}

/// Parse GNU `timeout`'s arguments. Options stop at the first operand, the
/// duration, so the command's own options are left alone.
fn parse(args: &[String]) -> Result<Request, String> {
    let mut preserve_status = false;
    let mut signal = 15;
    let mut args = args.iter();
    let duration = loop {
        let Some(arg) = args.next() else {
            return Err("missing operand".to_owned());
        };
        // An option's value: `-s SIG`, `-sSIG`, `--signal SIG` or
        // `--signal=SIG`.
        let mut value = |name: &str, attached: &str| -> Result<String, String> {
            match attached {
                "" => args
                    .next()
                    .cloned()
                    .ok_or(format!("option requires an argument -- '{name}'")),
                v => Ok(v.trim_start_matches('=').to_owned()),
            }
        };
        let (signal_arg, kill_after) = match arg.as_str() {
            "--help" => return Ok(Request::Help),
            "--version" => return Ok(Request::Version),
            "--preserve-status" => {
                preserve_status = true;
                continue;
            }
            "--foreground" => continue,
            "--" => break args.next().ok_or("missing operand")?,
            _ if arg.starts_with("--signal")
                && matches!(arg.as_bytes().get(8), None | Some(b'=')) =>
            {
                (Some(value("signal", &arg[8..])?), None)
            }
            _ if arg.starts_with("--kill-after")
                && matches!(arg.as_bytes().get(12), None | Some(b'=')) =>
            {
                (None, Some(value("kill-after", &arg[12..])?))
            }
            _ if arg.starts_with("-s") => (Some(value("s", &arg[2..])?), None),
            _ if arg.starts_with("-k") => (None, Some(value("k", &arg[2..])?)),
            _ if arg.starts_with("--") => return Err(format!("unrecognized option '{arg}'")),
            _ if arg.starts_with('-') && arg.len() > 1 => {
                let option = arg.chars().nth(1).unwrap_or_default();
                return Err(format!("invalid option -- '{option}'"));
            }
            _ => break arg,
        };
        if let Some(spec) = signal_arg {
            signal = jobs::parse_signal(&spec).ok_or(format!("'{spec}': invalid signal"))?;
        }
        if let Some(spec) = kill_after {
            parse_duration(&spec).ok_or(format!("invalid time interval '{spec}'"))?;
        }
    };
    let seconds = parse_duration(duration).ok_or(format!("invalid time interval '{duration}'"))?;
    let command: Vec<String> = args.cloned().collect();
    if command.is_empty() {
        return Err("missing operand".to_owned());
    }
    Ok(Request::Run(Run {
        preserve_status,
        signal,
        // Float to int saturates, so a huge duration (or `inf`) is ~584
        // years; a tiny non-zero one still times out.
        limit_ns: (seconds > 0.0).then(|| ((seconds * 1e9) as u64).max(1)),
        command,
    }))
}

pub(crate) struct TimeoutCommand {
    args: Vec<String>,
}

impl builtins::ParseArgs for TimeoutCommand {
    fn content(
        name: &str,
        content_type: builtins::ContentType,
        _options: &builtins::ContentOptions,
    ) -> Result<String, brush_core::Error> {
        match content_type {
            builtins::ContentType::DetailedHelp | builtins::ContentType::ManPage => {
                Ok(HELP.to_owned())
            }
            _ => Ok(format!("{name} - run a command with a time limit")),
        }
    }

    fn parse(invocation: builtins::Invocation<'_>) -> Result<Self, builtins::UsageError> {
        let args = invocation
            .into_args()
            .iter()
            .map(ToString::to_string)
            .collect();
        Ok(Self { args })
    }
}

impl<SE: ShellExtensions> builtins::Builtin<SE> for TimeoutCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let run = match parse(&self.args) {
            Ok(Request::Run(run)) => run,
            Ok(Request::Help) => {
                write!(context.stdout(), "{HELP}")?;
                return Ok(ExecutionResult::success());
            }
            Ok(Request::Version) => {
                writeln!(context.stdout(), "timeout (eryx shell builtin)")?;
                return Ok(ExecutionResult::success());
            }
            Err(e) => {
                writeln!(context.stderr(), "timeout: {e}\n{TRY}")?;
                return Ok(ExecutionResult::new(125));
            }
        };
        // The task must own what it runs in, so it can outlive this call's
        // borrows until it's stopped. It's a copy, as `env`'s is: nothing the
        // command changes reaches the caller.
        let target = Box::new(context.shell.clone());
        let mut parent = context.shell.clone();
        let params = context.params.clone();
        let command = run.command;
        let (id, mut task) = jobs::spawn_local(async move {
            let shell = ShellForCommand::OwnedShell {
                target,
                parent: &mut parent,
            };
            let started = crate::env::start_program(shell, params, &command).await?;
            started.wait().await.map(ExecutionResult::from)
        });
        let finished = match run.limit_ns {
            None => Some((&mut task).await),
            Some(ns) => {
                let mut timer = std::pin::pin!(crate::sleep(ns));
                std::future::poll_fn(|cx| {
                    if let std::task::Poll::Ready(done) = std::pin::Pin::new(&mut task).poll(cx) {
                        std::task::Poll::Ready(Some(done))
                    } else if timer.as_mut().poll(cx).is_ready() {
                        std::task::Poll::Ready(None)
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .await
            }
        };
        match finished {
            Some(Ok(Ok(result))) => Ok(result),
            Some(Ok(Err(e))) => {
                // Not found (127), can't be run (126), and the like.
                writeln!(context.stderr(), "timeout: {e}")?;
                Ok(ExecutionResult::new(u8::from(ExecutionExitCode::from(&e))))
            }
            Some(Err(e)) => {
                writeln!(context.stderr(), "timeout: {e}")?;
                Ok(ExecutionResult::new(125))
            }
            None => {
                jobs::abort_tree(id);
                // The task is dropped, and its child's `wait` cancelled
                // (killing the child), the next time the executor gets to
                // it; wait for that, and give the tasks it started theirs, so
                // the command is gone by the time `timeout` returns.
                let _ = task.await;
                tokio::task::yield_now().await;
                let status = if run.preserve_status {
                    u8::try_from(128 + run.signal).unwrap_or(255)
                } else {
                    124
                };
                Ok(ExecutionResult::new(status))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Request, Run, parse, parse_duration};

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn run(line: &str) -> Run {
        match parse(&args(line)) {
            Ok(Request::Run(run)) => run,
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn durations_take_gnu_suffixes() {
        assert_eq!(parse_duration("30"), Some(30.0));
        assert_eq!(parse_duration("0.5s"), Some(0.5));
        assert_eq!(parse_duration("2m"), Some(120.0));
        assert_eq!(parse_duration("1.5h"), Some(5400.0));
        assert_eq!(parse_duration("1d"), Some(86400.0));
        assert_eq!(parse_duration(".25"), Some(0.25));
        assert_eq!(parse_duration("inf"), Some(f64::INFINITY));
        for bad in ["", "s", "-1", "1ms", "1x", "nan", "abc"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn timeout_takes_gnu_options() {
        let r = run("30 pytest -x");
        assert_eq!(
            r,
            Run {
                preserve_status: false,
                signal: 15,
                limit_ns: Some(30_000_000_000),
                command: args("pytest -x"),
            }
        );
        assert_eq!(run("0 sleep 1").limit_ns, None);
        assert_eq!(run("1e-12 true").limit_ns, Some(1));
        assert_eq!(run("inf true").limit_ns, Some(u64::MAX));
        let r = run("--preserve-status --foreground -s KILL -k 5 1m cmd -s 3");
        assert!(r.preserve_status);
        assert_eq!(r.signal, 9);
        assert_eq!(r.limit_ns, Some(60_000_000_000));
        assert_eq!(r.command, args("cmd -s 3"));
        assert_eq!(run("--signal=INT --kill-after=1s 1 cmd").signal, 2);
        assert_eq!(run("--signal HUP --kill-after 1 1 cmd").signal, 1);
        assert_eq!(run("-sTERM -k1 1 cmd").signal, 15);
        assert_eq!(run("-- 1 -cmd").command, args("-cmd"));
        assert_eq!(parse(&args("--help")), Ok(Request::Help));
        assert_eq!(parse(&args("--version")), Ok(Request::Version));
        for bad in [
            "",
            "5",
            "-- 5",
            "x cmd",
            "-s BOGUS 1 cmd",
            "-s",
            "-k x 1 cmd",
            "--bogus 1 cmd",
            "-v 1 cmd",
            "-é 1 cmd",
            "--signalx 1 cmd",
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad}");
        }
    }
}
