//! `eryx:process/pipe`: a nested command with live pipes to its stdin and
//! stdout, for child commands that talk to a command of their own while it
//! runs, as git does to its `remote-https` helper.
//!
//! A piped command runs only while its caller is in a pipe call: each
//! `write`, `read` and `wait` polls every one of the caller's piped
//! commands, not just the one it's on, so a chain the caller connects (one's
//! output written into another) keeps moving. Everything runs on the
//! execution's own task, as the shell's children and nested `run`s do: one
//! execution never uses more than one core. Each poll returns by the
//! command's next epoch tick and is followed by charging what it burned to
//! the caller, so piped commands overshoot their execution's fuel by at most
//! one tick of work each, as children do.
//!
//! The commands live in their caller's state ([`Runs`]), so dropping a
//! handle, or the caller exiting or being killed mid-call, drops them, and
//! with them their store and their live-child slot, at once.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use wasmtime::StoreContextMut;
use wasmtime::component::{Linker, Resource, ResourceType};

use super::child_bindings::eryx::process::run::RunError;
use super::{ChildState, Held, LiveSlot, NestedTrace, ProcessError, charge_caller, pipe};

/// Most of a piped command's stdout one `read` returns.
const MAX_READ: u64 = 64 * 1024;

/// A `piped` resource: the id of a command in its caller's [`Runs`].
pub struct Piped(u64);

/// A caller's piped commands, by id.
#[derive(Default)]
pub(super) struct Runs {
    next: u64,
    runs: HashMap<u64, Run>,
}

type RunFuture = Pin<Box<dyn Future<Output = (Result<i32, String>, u64)> + Send>>;

/// One piped command.
struct Run {
    /// The command, until it ends.
    running: Option<RunFuture>,
    /// How it ended, once it has.
    ended: Option<Result<i32, String>>,
    /// Its stdin, until closed.
    stdin: Option<pipe::Writer>,
    stdout: pipe::Reader,
    /// Its stderr, until it ends, and bytes read from it the caller's stderr
    /// hasn't taken yet.
    stderr: Option<pipe::Reader>,
    pending: Vec<u8>,
    /// Fuel it has burned (published on each epoch tick), and how much of
    /// that its caller has been charged.
    fuel_used: Arc<AtomicU64>,
    charged: u64,
    trace: Option<NestedTrace>,
    /// Its live-child slot, held until the handle is dropped rather than
    /// only while it runs: an ended command whose handle is kept still holds
    /// its stdout's pipe buffer, so it keeps counting against the cap.
    _live: Option<LiveSlot>,
}

impl Run {
    /// Poll the command and copy what it wrote to stderr to `to` (the
    /// caller's stderr, or nowhere).
    fn poll(&mut self, cx: &mut Context<'_>, to: Option<&pipe::Writer>) {
        if let Some(running) = &mut self.running
            && let Poll::Ready((code, used)) = running.as_mut().poll(cx)
        {
            self.fuel_used.fetch_max(used, Ordering::Relaxed);
            self.ended = Some(code);
            // Dropping its store closes its end of the pipes.
            self.running = None;
        }
        let mut buf = [0u8; 16 * 1024];
        loop {
            if !self.pending.is_empty() {
                match to.map(|to| to.poll_write(cx, &self.pending)) {
                    Some(Poll::Ready(Some(n))) => {
                        self.pending.drain(..n);
                        continue;
                    }
                    Some(Poll::Pending) => break,
                    // No caller stderr, or it's gone: drop it.
                    Some(Poll::Ready(None)) | None => self.pending.clear(),
                }
            }
            let Some(stderr) = &self.stderr else { break };
            match stderr.poll_read(cx, &mut buf) {
                Poll::Ready(0) => self.stderr = None,
                Poll::Ready(n) => self.pending.extend_from_slice(&buf[..n]),
                Poll::Pending => break,
            }
        }
    }

    /// Fuel burned since the last charge, marked charged.
    fn take_fuel(&mut self) -> u64 {
        let used = self.fuel_used.load(Ordering::Relaxed);
        let delta = used.saturating_sub(self.charged);
        self.charged = self.charged.max(used);
        delta
    }
}

/// Poll all of the caller's piped commands and charge it what they burned:
/// false once it has no fuel left.
fn drive(store: &mut StoreContextMut<'_, ChildState>, cx: &mut Context<'_>) -> bool {
    let state = store.data_mut();
    let to = state.stderr.clone();
    let mut burned = 0;
    for run in state.piped.runs.values_mut() {
        run.poll(cx, to.as_ref());
        burned += run.take_fuel();
    }
    charge_caller(store, &mut 0, burned)
}

fn run_error(e: ProcessError) -> RunError {
    match e {
        ProcessError::CommandNotFound => RunError::CommandNotFound,
        ProcessError::LimitExceeded => RunError::LimitExceeded,
        ProcessError::SpawnFailed | ProcessError::IoError => RunError::SpawnFailed,
    }
}

/// The run `res` names in its caller's state.
fn run<'a>(state: &'a mut ChildState, res: &Resource<Piped>) -> wasmtime::Result<&'a mut Run> {
    let id = state.table.get(res)?.0;
    state
        .piped
        .runs
        .get_mut(&id)
        .ok_or_else(|| wasmtime::format_err!("piped command {id} is gone"))
}

fn out_of_fuel() -> RunError {
    tracing::warn!("caller ran out of fuel in a pipe call");
    RunError::SpawnFailed
}

/// Link `eryx:process/pipe` for children.
pub(super) fn add_to_linker(linker: &mut Linker<ChildState>) -> wasmtime::Result<()> {
    type Spawn = (String, Vec<String>, Vec<(String, String)>, String);
    let mut pipe = linker.instance("eryx:process/pipe@0.1.0")?;
    pipe.resource("piped", ResourceType::host::<Piped>(), |mut store, rep| {
        let Piped(id) = store
            .data_mut()
            .table
            .delete(Resource::<Piped>::new_own(rep))?;
        // Dropping the run kills it if it's still going; its trace records a
        // kill. What it burned since its last charge is charged now.
        if let Some(mut run) = store.data_mut().piped.runs.remove(&id) {
            let burned = run.take_fuel();
            // One that exited but wasn't waited for ends its trace with how
            // it ended, not as killed.
            if let (Some(trace), Some(ended)) = (run.trace.take(), &run.ended) {
                trace.end(ended.clone());
            }
            drop(run);
            charge_caller(&mut store, &mut 0, burned);
        }
        Ok(())
    })?;
    pipe.func_wrap(
        "[static]piped.spawn",
        |mut store, (cmd, args, env, cwd): Spawn| Ok((spawn(&mut store, cmd, args, env, cwd)?,)),
    )?;
    pipe.func_wrap_async(
        "[method]piped.write",
        |mut store, (res, data): (Resource<Piped>, Vec<u8>)| {
            Box::new(async move {
                // The data is held on the host until it's written.
                let Some(budget) = store.data().spawner.as_ref().map(|s| s.budget.clone()) else {
                    return Ok((Err(RunError::SpawnFailed),));
                };
                let mut held = Held { budget, bytes: 0 };
                if held.charge(data.len() as u64).is_err() {
                    return Ok((Err(RunError::LimitExceeded),));
                }
                let mut written = 0;
                let result = std::future::poll_fn(|cx| {
                    if !drive(&mut store, cx) {
                        return Poll::Ready(Ok(Err(out_of_fuel())));
                    }
                    let run = match run(store.data_mut(), &res) {
                        Ok(run) => run,
                        Err(e) => return Poll::Ready(Err(e)),
                    };
                    let Some(stdin) = &run.stdin else {
                        return Poll::Ready(Ok(Err(RunError::SpawnFailed)));
                    };
                    while written < data.len() {
                        match stdin.poll_write(cx, &data[written..]) {
                            Poll::Ready(Some(n)) => written += n,
                            // It exited or closed its stdin.
                            Poll::Ready(None) => {
                                return Poll::Ready(Ok(Err(RunError::SpawnFailed)));
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                    Poll::Ready(Ok(Ok(data.len() as u64)))
                })
                .await?;
                drop(held);
                Ok((result,))
            })
        },
    )?;
    pipe.func_wrap(
        "[method]piped.close-stdin",
        |mut store, (res,): (Resource<Piped>,)| {
            run(store.data_mut(), &res)?.stdin = None;
            Ok(())
        },
    )?;
    pipe.func_wrap_async(
        "[method]piped.read",
        |mut store, (res, max): (Resource<Piped>, u64)| {
            Box::new(async move {
                let mut buf = vec![0; usize::try_from(max.clamp(1, MAX_READ)).unwrap_or(1)];
                let result = std::future::poll_fn(|cx| {
                    if !drive(&mut store, cx) {
                        return Poll::Ready(Ok(Err(out_of_fuel())));
                    }
                    let run = match run(store.data_mut(), &res) {
                        Ok(run) => run,
                        Err(e) => return Poll::Ready(Err(e)),
                    };
                    // Its end of the pipe closes when it exits.
                    run.stdout
                        .poll_read(cx, &mut buf)
                        .map(|n| Ok(Ok(buf[..n].to_vec())))
                })
                .await?;
                Ok((result,))
            })
        },
    )?;
    pipe.func_wrap_async(
        "[method]piped.wait",
        |mut store, (res,): (Resource<Piped>,)| {
            Box::new(async move {
                run(store.data_mut(), &res)?.stdin = None;
                let result = std::future::poll_fn(|cx| {
                    if !drive(&mut store, cx) {
                        return Poll::Ready(Ok(None));
                    }
                    let run = match run(store.data_mut(), &res) {
                        Ok(run) => run,
                        Err(e) => return Poll::Ready(Err(e)),
                    };
                    // Done once it has exited and its stderr is all passed on.
                    if run.ended.is_some() && run.stderr.is_none() && run.pending.is_empty() {
                        // Kept, so waiting again returns it again.
                        Poll::Ready(Ok(run.ended.clone()))
                    } else {
                        Poll::Pending
                    }
                })
                .await?;
                let run = run(store.data_mut(), &res)?;
                let trace = run.trace.take();
                let Some(code) = result else {
                    if let Some(trace) = trace {
                        trace.end(Err("its caller ran out of fuel".to_owned()));
                    }
                    return Ok((Err(out_of_fuel()),));
                };
                if let Some(trace) = trace {
                    trace.end(code.clone());
                }
                Ok((code.map_err(|e| {
                    tracing::warn!(error = %e, "piped command failed");
                    RunError::SpawnFailed
                }),))
            })
        },
    )?;
    Ok(())
}

/// `[static]piped.spawn`: start `cmd` for the child whose store this is.
fn spawn(
    store: &mut StoreContextMut<'_, ChildState>,
    cmd: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: String,
) -> wasmtime::Result<Result<Resource<Piped>, RunError>> {
    let state = store.data_mut();
    let Some(spawner) = state.spawner.clone() else {
        tracing::warn!(
            cmd,
            "a child whose command may not spawn tried to pipe a command"
        );
        return Ok(Err(RunError::SpawnFailed));
    };
    let prepared = match spawner.prepare(
        cmd,
        args,
        env,
        cwd,
        #[cfg(any(feature = "unstable-vfs", feature = "shell"))]
        state.vfs.as_ref().map(eryx_vfs::HybridVfsCtx::fork),
    ) {
        Ok(prepared) => prepared,
        Err(e) => return Ok(Err(run_error(e))),
    };
    let trace = spawner
        .traces
        .clone()
        .map(|traces| NestedTrace::start(traces, prepared.run.argv.clone()));
    let mut command = prepared.run;
    command.fuel = store.get_fuel().unwrap_or(0);
    let live = command._live.take();
    let run = Run {
        fuel_used: Arc::clone(&command.fuel_used),
        running: Some(Box::pin(command.run())),
        ended: None,
        stdin: Some(prepared.stdin),
        stdout: prepared.stdout,
        stderr: Some(prepared.stderr),
        pending: Vec::new(),
        charged: 0,
        trace,
        _live: live,
    };
    let state = store.data_mut();
    let id = state.piped.next;
    state.piped.next += 1;
    state.piped.runs.insert(id, run);
    Ok(Ok(state.table.push(Piped(id))?))
}
