//! Background jobs: the shell's tasks as a tree, so a job can be stopped with
//! everything it started; the `kill` builtin; and what happens to jobs still
//! running when an execution ends.
//!
//! A job is brush's: `cmd &` runs `cmd` in a task of its own, with an ID of
//! the shell's for `$!` (no process has one here). There are no signals on
//! WASI, so `kill` stops a job by aborting its tasks. That drops its
//! pipeline stages and pumps, and cancels the host's `wait` on its children,
//! which kills them. The job then reads as having exited with 128 plus the
//! signal's number, as one ended by that signal would.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Write as _;

use brush_core::{ExecutionContext, ExecutionResult, ShellExtensions, builtins, error, jobs};

tokio::task_local! {
    /// The tasks a task runs under, outermost first, ending with its own ID.
    static LINEAGE: Vec<u64>;
}

thread_local! {
    /// The tasks still running, each with its lineage.
    static TASKS: RefCell<HashMap<u64, (Vec<u64>, tokio::task::AbortHandle)>> =
        RefCell::new(HashMap::new());
    static NEXT_TASK: Cell<u64> = const { Cell::new(0) };
    /// Jobs a nested shell left running when it exited (see [`orphan`]).
    static ORPHANS: RefCell<Vec<jobs::Job>> = const { RefCell::new(Vec::new()) };
}

/// Keep the jobs a nested shell leaves running when it exits, as bash's
/// keep running without it, for the execution's [`finish_jobs`] to wait for.
pub(crate) fn orphan(jobs: &mut jobs::JobManager) {
    ORPHANS.with(|o| o.borrow_mut().append(&mut jobs.jobs));
}

/// [`tokio::task::spawn_local`], recording the task under the one that
/// spawns it, so that [`abort_tree`] stops it with its parent.
pub(crate) fn spawn_local<F>(future: F) -> (u64, tokio::task::JoinHandle<F::Output>)
where
    F: Future + 'static,
    F::Output: 'static,
{
    /// Forgets the task when it finishes or is dropped.
    struct Running(u64);
    impl Drop for Running {
        fn drop(&mut self) {
            TASKS.with(|t| t.borrow_mut().remove(&self.0));
        }
    }
    let id = NEXT_TASK.get();
    NEXT_TASK.set(id + 1);
    let mut lineage = LINEAGE.try_with(Clone::clone).unwrap_or_default();
    lineage.push(id);
    let abort = lineage.clone();
    let task = tokio::task::spawn_local(LINEAGE.scope(lineage, async move {
        let _running = Running(id);
        future.await
    }));
    // A spawned task doesn't start until its spawner yields, so it's
    // recorded before it can finish.
    TASKS.with(|t| t.borrow_mut().insert(id, (abort, task.abort_handle())));
    (id, task)
}

/// Abort the task `id` and every task it started, however deep.
pub(crate) fn abort_tree(id: u64) {
    TASKS.with(|t| {
        for (lineage, task) in t.borrow().values() {
            if lineage.contains(&id) {
                task.abort();
            }
        }
    });
}

/// How long before the execution's timeout [`finish_jobs`] gives up on the
/// jobs, so the guest can stop them and return before the host's deadline.
const TIMEOUT_MARGIN_NS: u64 = 50_000_000;

/// Finish the jobs still running once the script has ended, as a trailing
/// `wait` would: wait for them all, but if the execution would time out
/// first (`time_left_ns`, none without a timeout), stop them in time to
/// return the script's own result. Either way the shell has no jobs left
/// afterwards, nor any [orphans](orphan). Returns how many were stopped.
pub(crate) async fn finish_jobs(jobs: &mut jobs::JobManager, time_left_ns: Option<u64>) -> usize {
    // Nested shells' leftover jobs end with the execution too.
    ORPHANS.with(|o| jobs.jobs.append(&mut o.borrow_mut()));
    let finished = {
        let mut waited = std::pin::pin!(jobs.wait_all());
        let budget = time_left_ns.map(|left| left.saturating_sub(TIMEOUT_MARGIN_NS));
        let mut timer = std::pin::pin!(async move {
            match budget {
                Some(ns) => crate::sleep(ns).await,
                None => std::future::pending().await,
            }
        });
        std::future::poll_fn(|cx| {
            if waited.as_mut().poll(cx).is_ready() {
                std::task::Poll::Ready(true)
            } else if timer.as_mut().poll(cx).is_ready() {
                std::task::Poll::Ready(false)
            } else {
                std::task::Poll::Pending
            }
        })
        .await
    };
    let mut stopped = 0;
    if !finished {
        // Kill the children first, all at once: aborting a job cancels its
        // child's `wait`, which blocks until the host has dropped it, and
        // meanwhile every other child still running gets a turn, an epoch
        // tick for a busy one. One by one that eats the margin.
        crate::eryx::process::execution::kill_children();
        for job in &mut jobs.jobs {
            if matches!(job.poll_done(), Ok(None)) && !matches!(job.state, jobs::JobState::Done) {
                // As a shell's jobs get SIGHUP when it exits.
                job.abort((128 + 1).into());
                stopped += 1;
            }
        }
    }
    jobs.jobs.clear();
    stopped
}

/// The signals `kill` knows, as Linux numbers them. All of them stop the job.
const SIGNALS: [(&str, i32); 9] = [
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("KILL", 9),
    ("USR1", 10),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
];

const USAGE: &str =
    "usage: kill [-s SIGSPEC | -n SIGNUM | -SIGSPEC] PID | JOBSPEC ... or kill -l [SIGSPEC]";

/// A signal named or numbered as `kill` takes it: `TERM`, `SIGTERM`, `term`
/// or `15`, and `0` for none.
pub(crate) fn parse_signal(spec: &str) -> Option<i32> {
    if let Ok(n) = spec.parse::<i32>() {
        return (n == 0 || SIGNALS.iter().any(|&(_, m)| m == n)).then_some(n);
    }
    let upper = spec.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    SIGNALS.iter().find(|&&(s, _)| s == name).map(|&(_, n)| n)
}

/// What `kill` was asked to do.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    /// Send the signal (0: none, just check) to each target.
    Signal(i32, Vec<String>),
    /// `-l`: list the signals, or name or number the given ones.
    List(Vec<String>),
}

fn parse(args: &[String]) -> Result<Request, String> {
    let mut signal = 15;
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next_if(|a| a.starts_with('-') && a.len() > 1) {
        match arg.as_str() {
            "--" => break,
            "-l" | "-L" => return Ok(Request::List(args.cloned().collect())),
            "-s" | "-n" => {
                let spec = args.next().ok_or(USAGE)?;
                signal =
                    parse_signal(spec).ok_or(format!("{spec}: invalid signal specification"))?;
            }
            _ => {
                let spec = &arg[1..];
                signal =
                    parse_signal(spec).ok_or(format!("{spec}: invalid signal specification"))?;
            }
        }
    }
    let targets: Vec<String> = args.cloned().collect();
    if targets.is_empty() {
        return Err(USAGE.to_owned());
    }
    Ok(Request::Signal(signal, targets))
}

/// `kill`: stops the shell's jobs (no process here has a signal to get).
pub(crate) struct KillCommand {
    args: Vec<String>,
}

impl builtins::ParseArgs for KillCommand {
    fn content(
        name: &str,
        content_type: builtins::ContentType,
        _options: &builtins::ContentOptions,
    ) -> Result<String, brush_core::Error> {
        match content_type {
            builtins::ContentType::ManPage => error::unimp("no man page"),
            _ => Ok(format!("{name} - stop a background job")),
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

impl<SE: ShellExtensions> builtins::Builtin<SE> for KillCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let (signal, targets) = match parse(&self.args) {
            Ok(Request::Signal(signal, targets)) => (signal, targets),
            Ok(Request::List(specs)) => return list(&context, &specs),
            Err(e) => {
                writeln!(context.stderr(), "kill: {e}")?;
                return Ok(ExecutionResult::new(2));
            }
        };
        let mut status = 0;
        for target in &targets {
            let jobs = context.shell.jobs_mut();
            let job = if target.starts_with('%') {
                jobs.resolve_job_spec(target)
            } else if let Ok(pid) = target.parse() {
                jobs.find_by_pid(pid)
            } else {
                writeln!(
                    context.stderr(),
                    "kill: {target}: arguments must be process or job IDs"
                )?;
                status = 1;
                continue;
            };
            // A finished job that hasn't been waited for is still there, as a
            // zombie process is: signalling it does nothing, and `kill -0`
            // says it's gone.
            let found = job.is_some();
            let running = job.is_some_and(|job| {
                matches!(job.poll_done(), Ok(None))
                    && !matches!(job.state, jobs::JobState::Done)
                    && (signal == 0 || job.abort(u8::try_from(128 + signal).unwrap_or(255).into()))
            });
            if !running && (signal == 0 || !found) {
                if target.starts_with('%') {
                    writeln!(context.stderr(), "kill: {target}: no such job")?;
                } else {
                    writeln!(context.stderr(), "kill: ({target}) - No such process")?;
                }
                status = 1;
            }
        }
        // An aborted task is dropped (and its child killed) the next time
        // the executor gets to it: let it, so what the job held is free by
        // the time `kill` returns.
        tokio::task::yield_now().await;
        Ok(ExecutionResult::new(status))
    }
}

/// `kill -l`: every signal, or each spec's name (for a number, or an exit
/// status above 128) or number (for a name).
fn list<SE: ShellExtensions>(
    context: &ExecutionContext<'_, SE>,
    specs: &[String],
) -> Result<ExecutionResult, brush_core::Error> {
    if specs.is_empty() {
        for (name, n) in SIGNALS {
            writeln!(context.stdout(), "{n:2}) SIG{name}")?;
        }
        return Ok(ExecutionResult::success());
    }
    let mut status = 0;
    for spec in specs {
        let named = match spec.parse::<i32>() {
            Ok(n) => {
                let n = if n > 128 { n - 128 } else { n };
                SIGNALS
                    .iter()
                    .find(|&&(_, m)| m == n)
                    .map(|&(name, _)| name.to_owned())
            }
            Err(_) => parse_signal(spec)
                .filter(|&n| n != 0)
                .map(|n| n.to_string()),
        };
        match named {
            Some(named) => writeln!(context.stdout(), "{named}")?,
            None => {
                writeln!(
                    context.stderr(),
                    "kill: {spec}: invalid signal specification"
                )?;
                status = 1;
            }
        }
    }
    Ok(ExecutionResult::new(status))
}

#[cfg(test)]
mod tests {
    use super::{Request, parse, parse_signal};

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn signals_are_named_or_numbered() {
        assert_eq!(parse_signal("TERM"), Some(15));
        assert_eq!(parse_signal("sigkill"), Some(9));
        assert_eq!(parse_signal("9"), Some(9));
        assert_eq!(parse_signal("0"), Some(0));
        assert_eq!(parse_signal("STOP"), None);
        assert_eq!(parse_signal("99"), None);
    }

    #[test]
    fn kill_takes_bash_style_options() {
        let signal = |n, t: &str| Ok(Request::Signal(n, args(t)));
        assert_eq!(parse(&args("%1")), signal(15, "%1"));
        assert_eq!(parse(&args("-9 123 %2")), signal(9, "123 %2"));
        assert_eq!(parse(&args("-s INT %1")), signal(2, "%1"));
        assert_eq!(parse(&args("-n 0 %1")), signal(0, "%1"));
        assert_eq!(parse(&args("-KILL -- %1")), signal(9, "%1"));
        assert_eq!(parse(&args("-l 143")), Ok(Request::List(args("143"))));
        assert!(parse(&args("-9")).is_err());
        assert!(parse(&args("-STOP %1")).is_err());
    }
}
