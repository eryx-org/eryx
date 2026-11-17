//! `eryx:process/pipe`: a nested command with live pipes to its stdin and
//! stdout, for child commands that talk to a command of their own while it
//! runs, as git does to its `remote-https` helper.
//!
//! A piped command runs on its own task, so it makes progress while its
//! caller computes or waits on another one; a caller connecting two of them
//! never deadlocks on which one is being polled. It holds a live-child slot
//! and memory from its execution's budget like any child, and dropping its
//! handle (which its caller's store does when it exits or is killed) aborts
//! it, so it never outlives its caller. The fuel it burns is moved to its
//! caller at each of the caller's pipe calls, so a caller computing while its
//! command runs can overshoot its fuel by the command's burn since its last
//! call, as children overshoot by an epoch tick each.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use wasmtime::component::{Linker, Resource, ResourceType};

use super::child_bindings::eryx::process::run::RunError;
use super::{
    CHILD_FUEL, ChildState, LiveSlot, MAX_SPAWN_DEPTH, NestedTrace, ProcessError, charge_caller,
    pipe,
};

/// Most of a piped command's stdout one `read` returns.
const MAX_READ: u64 = 64 * 1024;

/// A running piped command, a `piped` resource in its caller's table.
pub struct Piped {
    /// Its stdin, until closed.
    stdin: Option<pipe::Writer>,
    stdout: pipe::Reader,
    /// The command, until `wait` takes it.
    running: Option<JoinHandle<(Result<i32, String>, u64)>>,
    /// Copies its stderr to its caller's.
    stderr: Option<JoinHandle<()>>,
    /// Fuel it has burned, published on each epoch tick, and how much of
    /// that its caller has been charged.
    fuel_used: Arc<AtomicU64>,
    charged: u64,
    trace: Option<NestedTrace>,
    /// Its live-child slot, held here rather than by the command's task so
    /// dropping the handle frees it at once, not when the aborted task is
    /// next polled.
    _live: Option<LiveSlot>,
}

impl Drop for Piped {
    fn drop(&mut self) {
        // Dropped before `wait` finished: kill it. The trace records the end
        // as a kill (`NestedTrace`'s own drop).
        if let Some(running) = self.running.take() {
            running.abort();
        }
        if let Some(stderr) = self.stderr.take() {
            stderr.abort();
        }
    }
}

fn run_error(e: ProcessError) -> RunError {
    match e {
        ProcessError::CommandNotFound => RunError::CommandNotFound,
        ProcessError::LimitExceeded => RunError::LimitExceeded,
        ProcessError::SpawnFailed | ProcessError::IoError => RunError::SpawnFailed,
    }
}

/// Charge `res`'s caller what its command has burned since the last
/// charge; false (after warning) once the caller has no fuel left.
fn charge(
    store: &mut wasmtime::StoreContextMut<'_, ChildState>,
    res: &Resource<Piped>,
) -> wasmtime::Result<bool> {
    let piped = store.data_mut().table.get_mut(res)?;
    let used = piped.fuel_used.load(Ordering::Relaxed);
    let mut charged = piped.charged;
    let left = charge_caller(store, &mut charged, used);
    store.data_mut().table.get_mut(res)?.charged = charged;
    if !left {
        tracing::warn!("caller ran out of fuel; killing its piped command");
    }
    Ok(left)
}

/// Link `eryx:process/pipe` for children.
pub(super) fn add_to_linker(linker: &mut Linker<ChildState>) -> wasmtime::Result<()> {
    type Spawn = (String, Vec<String>, Vec<(String, String)>, String);
    let mut pipe = linker.instance("eryx:process/pipe@0.1.0")?;
    pipe.resource("piped", ResourceType::host::<Piped>(), |mut store, rep| {
        store
            .data_mut()
            .table
            .delete(Resource::<Piped>::new_own(rep))?;
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
                if !charge(&mut store, &res)? {
                    return Ok((Err(RunError::SpawnFailed),));
                }
                let Some(mut stdin) = store.data_mut().table.get(&res)?.stdin.clone() else {
                    return Ok((Err(RunError::SpawnFailed),));
                };
                // It exited or closed its stdin.
                Ok((match stdin.write_all(&data).await {
                    Ok(()) => Ok(data.len() as u64),
                    Err(_) => Err(RunError::SpawnFailed),
                },))
            })
        },
    )?;
    pipe.func_wrap(
        "[method]piped.close-stdin",
        |mut store, (res,): (Resource<Piped>,)| {
            store.data_mut().table.get_mut(&res)?.stdin = None;
            Ok(())
        },
    )?;
    pipe.func_wrap_async(
        "[method]piped.read",
        |mut store, (res, max): (Resource<Piped>, u64)| {
            Box::new(async move {
                if !charge(&mut store, &res)? {
                    return Ok((Err(RunError::SpawnFailed),));
                }
                let mut stdout = store.data_mut().table.get(&res)?.stdout.clone();
                let mut buf = vec![0; usize::try_from(max.clamp(1, MAX_READ)).unwrap_or(1)];
                // A closed pipe reads as its end.
                let n = stdout.read(&mut buf).await.unwrap_or(0);
                buf.truncate(n);
                Ok((Ok::<_, RunError>(buf),))
            })
        },
    )?;
    pipe.func_wrap_async(
        "[method]piped.wait",
        |mut store, (res,): (Resource<Piped>,)| {
            Box::new(async move {
                let piped = store.data_mut().table.get_mut(&res)?;
                piped.stdin = None;
                let (Some(running), stderr) = (piped.running.take(), piped.stderr.take()) else {
                    // Waited on twice.
                    return Ok((Err(RunError::SpawnFailed),));
                };
                let ended = running.await;
                if let Some(stderr) = stderr {
                    // Its stderr ends when it exits; finish copying it.
                    let _ = stderr.await;
                }
                let (code, used) = match ended {
                    Ok((code, used)) => (code, used),
                    Err(e) => (Err(format!("its task failed: {e}")), 0),
                };
                let piped = store.data_mut().table.get_mut(&res)?;
                let used = used.max(piped.fuel_used.load(Ordering::Relaxed));
                piped.fuel_used.store(used, Ordering::Relaxed);
                let left = charge(&mut store, &res)?;
                if let Some(trace) = store.data_mut().table.get_mut(&res)?.trace.take() {
                    trace.end(code.clone());
                }
                if !left {
                    return Ok((Err(RunError::SpawnFailed),));
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
    store: &mut wasmtime::StoreContextMut<'_, ChildState>,
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
    if spawner.depth > MAX_SPAWN_DEPTH {
        tracing::warn!(cmd, depth = spawner.depth, "nested spawns too deep");
        return Ok(Err(RunError::LimitExceeded));
    }
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
    let caller_stderr = state.stderr.clone();
    let trace = spawner
        .traces
        .clone()
        .map(|traces| NestedTrace::start(traces, prepared.run.argv.clone()));
    let mut run = prepared.run;
    run.fuel = store.get_fuel().unwrap_or(0).min(CHILD_FUEL);
    let live = run._live.take();
    let fuel_used = Arc::clone(&run.fuel_used);
    let mut stderr = prepared.stderr;
    let stderr = tokio::spawn(async move {
        let mut buf = vec![0; 16 * 1024];
        let mut to = caller_stderr;
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                // Without the caller's stderr (or once it's gone), drain it.
                Ok(n) => {
                    if let Some(to) = to.as_mut() {
                        let _ = to.write_all(&buf[..n]).await;
                    }
                }
            }
        }
    });
    let piped = Piped {
        stdin: Some(prepared.stdin),
        stdout: prepared.stdout,
        running: Some(tokio::spawn(run.run())),
        stderr: Some(stderr),
        fuel_used,
        charged: 0,
        trace,
        _live: live,
    };
    Ok(Ok(store.data_mut().table.push(piped)?))
}
