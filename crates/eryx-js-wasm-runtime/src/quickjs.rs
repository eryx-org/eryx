//! QuickJS engine wrapper for eryx-js-wasm-runtime.
//!
//! This is the JS analog of `eryx-wasm-runtime/src/python.rs`. Where the Python
//! guest drives CPython through PyO3/`PyRun_SimpleString`, this drives QuickJS
//! through the `rquickjs` bindings.
//!
//! Responsibilities mirrored from the Python guest:
//!  - Hold a persistent JS runtime + context so user state persists across
//!    `execute()` calls (the JS analog of `_eryx_user_globals`).
//!  - Run user code, capturing stdout/stderr and streaming each write to the host
//!    via the `report-output` import (stream-id 0 = stdout, 1 = stderr).
//!  - Capture a configurable "result" variable, JSON-serialize it, and hand it back.
//!  - Pump the JS microtask/job queue so Promises resolve.
//!  - Bridge host `invoke()` callbacks into JS (sketched; see TODOs).
//!
//! SPIKE: The whole-file structure is meant to be read for feasibility, not run.
//! Several seams that the Python guest solves with CPython-specific machinery
//! (pickle, sys.settrace, asyncio) are stubbed with `// TODO:` markers because the
//! JS model genuinely differs.

#![allow(dead_code)]

use std::cell::RefCell;

use rquickjs::{CatchResultExt, Context, Ctx, Function, Object, Runtime, Value, function::Func};

// =============================================================================
// Host callback plumbing (mirrors python.rs thread-local callback registry)
// =============================================================================
//
// lib.rs registers these before each export call (inside `with_wit`) and clears
// them afterwards, exactly like the Python guest. They forward JS-side intrinsics
// (console.log, eryx.invoke) to the WIT imports.

/// Stream a chunk of output to the host. `stream`: 0 = stdout, 1 = stderr.
pub type ReportOutputCallback = fn(u32, &str);

/// Invoke a host callback synchronously. Returns the JSON result, or an error.
///
/// SPIKE: The Python guest also has an *async* invoke path (`InvokeResult::Pending`)
/// wired into the Component Model async protocol. The async story for JS is
/// sketched in `execute_js` (microtask pump) but the pending/suspend bridge is a
/// TODO — see notes there.
pub type InvokeCallback = fn(&str, &str) -> Result<String, String>;

thread_local! {
    static REPORT_OUTPUT_CALLBACK: RefCell<Option<ReportOutputCallback>> = const { RefCell::new(None) };
    static INVOKE_CALLBACK: RefCell<Option<InvokeCallback>> = const { RefCell::new(None) };
}

/// Register the report-output callback (called by lib.rs in `with_wit`).
pub fn set_report_output_callback(cb: Option<ReportOutputCallback>) {
    REPORT_OUTPUT_CALLBACK.with(|c| *c.borrow_mut() = cb);
}

/// Register the invoke callback (called by lib.rs in `with_wit`).
pub fn set_invoke_callback(cb: Option<InvokeCallback>) {
    INVOKE_CALLBACK.with(|c| *c.borrow_mut() = cb);
}

/// Forward an output chunk to the host, if a callback is registered.
fn do_report_output(stream: u32, data: &str) {
    REPORT_OUTPUT_CALLBACK.with(|c| {
        if let Some(cb) = c.borrow().as_ref() {
            cb(stream, data);
        }
        // No callback => output streaming disabled, not an error (mirrors python.rs).
    });
}

/// Forward an invoke to the host, if a callback is registered.
fn do_invoke(name: &str, args_json: &str) -> Result<String, String> {
    INVOKE_CALLBACK.with(|c| {
        let cb = c.borrow();
        let cb = cb.as_ref().ok_or_else(|| {
            "invoke() called outside of execute context - callbacks can only be \
             called during code execution"
                .to_string()
        })?;
        cb(name, args_json)
    })
}

// =============================================================================
// Execute result types (mirror python.rs ExecuteResult / ExecuteOutput)
// =============================================================================

/// Output from executing JS code. Field-for-field identical to the Python guest's
/// `ExecuteOutput` so lib.rs can build the same WIT `execute-output` record.
#[derive(Debug, Clone, Default)]
pub struct ExecuteOutput {
    /// Captured stdout (console.log / process.stdout.write).
    pub stdout: String,
    /// Captured stderr (console.error / console.warn).
    pub stderr: String,
    /// JSON-serialized value of the user's result variable, or "" if unset.
    pub result: String,
    /// Reason result capture failed (e.g. not JSON-serializable), or "" on success.
    pub result_error: String,
}

/// Result of executing JS code. Mirrors `python::ExecuteResult`.
#[derive(Debug)]
pub enum ExecuteResult {
    /// Completed successfully with captured output.
    Complete(ExecuteOutput),
    /// Completed with an error (uncaught JS exception, etc.).
    Error(String),
    /// SPIKE/TODO: Suspended waiting for an async host callback. The Python guest
    /// returns the Component Model callback code here (`WAIT | waitable_set << 4`).
    /// The JS equivalent requires bridging an unresolved Promise to a host subtask;
    /// not implemented in the spike. See `execute_js`.
    Pending(u32),
}

// =============================================================================
// Persistent engine state
// =============================================================================
//
// WASM is single-threaded, so a thread-local holding the live Runtime + Context
// is sufficient and matches the Python guest's `PYTHON_INITIALIZED` + global
// interpreter model. The Context owns the persistent global object, which is our
// equivalent of `_eryx_user_globals`.

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

/// The live QuickJS engine. Holds the runtime and a single long-lived context.
struct Engine {
    runtime: Runtime,
    context: Context,
    /// Name of the user variable captured as the structured result. Defaults to
    /// "result"; configurable via the `set-result-variable` export.
    result_var_name: String,
}

/// Per-execution output accumulator. A `RefCell` of this is shared with the JS
/// `console` shim closures so each `console.log` both appends here AND streams to
/// the host. Mirrors the Python `_EryxStreamingWriter` pair.
#[derive(Default)]
struct OutputBuffers {
    stdout: String,
    stderr: String,
}

thread_local! {
    static OUTPUT: RefCell<OutputBuffers> = RefCell::new(OutputBuffers::default());
}

/// Initialize the QuickJS engine. Called once from `Interpreter::initialize`.
/// Subsequent calls are no-ops (mirrors `initialize_python`).
pub fn initialize_js() {
    ENGINE.with(|cell| {
        if cell.borrow().is_some() {
            return;
        }

        // SPIKE: unwrap is fine for a spike; the real guest would surface this.
        let runtime = Runtime::new().expect("failed to create QuickJS runtime");
        let context = Context::full(&runtime).expect("failed to create QuickJS context");

        // Install the host-facing intrinsics (console.*, eryx.invoke) into the
        // persistent global object exactly once. This is the JS analog of
        // injecting `_eryx`, `_eryx_async`, and the stdout/stderr writers during
        // Python init.
        context.with(|ctx| {
            install_console(&ctx);
            install_eryx_intrinsics(&ctx);
        });

        *cell.borrow_mut() = Some(Engine {
            runtime,
            context,
            result_var_name: "result".to_string(),
        });
    });
}

/// Set the result-variable name (backs the `set-result-variable` export).
/// Mirrors `python::set_result_variable_name`.
pub fn set_result_variable_name(name: &str) {
    ENGINE.with(|cell| {
        if let Some(engine) = cell.borrow_mut().as_mut() {
            engine.result_var_name = name.to_string();
        }
    });
}

// =============================================================================
// Intrinsics installed into the JS global object
// =============================================================================

/// Install a `console` object whose methods both buffer output and stream it to
/// the host via `report-output`. This is the JS analog of redirecting Python's
/// `sys.stdout`/`sys.stderr` to `_EryxStreamingWriter`.
fn install_console(ctx: &Ctx<'_>) {
    let globals = ctx.globals();

    let console = Object::new(ctx.clone()).expect("create console object");

    // console.log / console.info -> stdout (stream 0).
    let log = Function::new(ctx.clone(), |args_text: String| {
        OUTPUT.with(|o| o.borrow_mut().stdout.push_str(&args_text));
        OUTPUT.with(|o| o.borrow_mut().stdout.push('\n'));
        do_report_output(0, &format!("{args_text}\n"));
    })
    .expect("create console.log");

    // console.error / console.warn -> stderr (stream 1).
    let err = Function::new(ctx.clone(), |args_text: String| {
        OUTPUT.with(|o| o.borrow_mut().stderr.push_str(&args_text));
        OUTPUT.with(|o| o.borrow_mut().stderr.push('\n'));
        do_report_output(1, &format!("{args_text}\n"));
    })
    .expect("create console.error");

    // SPIKE: We register the Rust functions under private names and then wrap them
    // in JS so that the *variadic* `console.log(a, b, c)` argument-joining (which
    // QuickJS/JS does naturally with template handling) happens in JS, keeping the
    // Rust side a simple `String -> ()`. A real impl might accept `Rest<Value>` and
    // format each argument with its JS `String(...)` coercion + util.inspect-style
    // formatting for objects.
    console.set("_log", log).expect("set console._log");
    console.set("_err", err).expect("set console._err");
    globals.set("console", console).expect("set console");

    // Thin JS shim to give us real variadic console.* that coerces every arg.
    ctx.eval::<(), _>(
        r#"
        (() => {
            const fmt = (args) => args.map(a => {
                if (typeof a === 'string') return a;
                try { return JSON.stringify(a); } catch { return String(a); }
            }).join(' ');
            const _log = console._log, _err = console._err;
            console.log = (...a) => _log(fmt(a));
            console.info = (...a) => _log(fmt(a));
            console.debug = (...a) => _log(fmt(a));
            console.error = (...a) => _err(fmt(a));
            console.warn = (...a) => _err(fmt(a));
            delete console._log; delete console._err;
        })();
        "#,
    )
    .expect("install console shim");
}

/// Install the `eryx` host bridge object: `eryx.invoke(name, argsJson)`.
///
/// This is the JS analog of the Python `_eryx` C-extension module plus the
/// `invoke()` helper. For the spike we expose a *synchronous* invoke that blocks
/// on the host callback, matching `python::do_invoke` (the sync path).
fn install_eryx_intrinsics(ctx: &Ctx<'_>) {
    let globals = ctx.globals();
    let eryx = Object::new(ctx.clone()).expect("create eryx object");

    // eryx.invoke(name, argsJson) -> resultJson  (synchronous).
    //
    // SPIKE: A faithful port needs an *async* invoke returning a Promise, so JS
    // user code can `await eryx.invoke(...)`. That requires:
    //   1. Creating a JS Promise and stashing its resolve/reject callbacks.
    //   2. Calling the host's async `invoke` import; if it returns Pending, return
    //      Pending up to lib.rs (Component Model WAIT) instead of resolving now.
    //   3. On `export_async_callback`, resolving the stashed Promise and re-pumping
    //      the job queue (see `pump_jobs`).
    // The Python guest does all of this through asyncio + `_eryx_async`. QuickJS has
    // no asyncio; we'd drive Promises directly via the host job queue. TODO.
    let invoke = Function::new(ctx.clone(), |name: String, args_json: String| {
        match do_invoke(&name, &args_json) {
            Ok(result) => Ok(result),
            // Surface host errors as thrown JS exceptions. `rquickjs` maps a Rust
            // `Err(String)` returned from a `Function` into a thrown JS Error when
            // the closure returns `Result`. (Using a plain String error here for
            // the spike; a real impl would build a proper `rquickjs::Error`.)
            Err(e) => Err(rquickjs::Error::new_from_js_message("invoke", "host", e)),
        }
    })
    .expect("create eryx.invoke");

    eryx.set("invoke", invoke).expect("set eryx.invoke");
    globals.set("eryx", eryx).expect("set eryx");
}

// =============================================================================
// Execute
// =============================================================================

/// Execute user JS code in the persistent context. Mirrors `execute_python`.
///
/// Flow:
///   1. Reset the per-execution output buffers (the global object / user vars
///      persist; only output is per-run, like Python resetting the StringIOs).
///   2. Eval the user code. State (top-level `var`/`globalThis` assignments,
///      function declarations) persists into the next call via the shared context.
///   3. Pump the job queue so any Promises created by the code settle.
///   4. Capture + JSON-serialize the result variable, then consume it.
///
/// NUL-byte note: QuickJS source is a length-delimited buffer, so embedded NULs do
/// not truncate the way a CPython `CString` would. The Python guest rejects NULs up
/// front; here it's a non-issue, but the host-side validation still applies.
pub fn execute_js(code: &str) -> ExecuteResult {
    ENGINE.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(engine) = borrow.as_mut() else {
            return ExecuteResult::Error("JS engine not initialized".to_string());
        };

        // Reset per-execution output (user globals are intentionally NOT reset).
        OUTPUT.with(|o| *o.borrow_mut() = OutputBuffers::default());

        let result_var = engine.result_var_name.clone();

        // Step 1: eval the user code inside the JS realm. `Context::with` enters the
        // realm and hands us a `Ctx`. `.catch(&ctx)` converts a raw JS exception
        // into a `CaughtError` carrying the message + stack — analogous to reading
        // the Python traceback out of captured stderr.
        let eval_ok: Result<(), String> =
            engine
                .context
                .with(|ctx| match ctx.eval::<Value, _>(code).catch(&ctx) {
                    Ok(_value) => Ok(()),
                    Err(err) => Err(format!("{err}")),
                });

        // Step 2: pump the microtask/job queue to completion. The pending-job API
        // lives on the `Runtime` (not the `Ctx`), so this runs outside the `with`
        // closure above.
        //
        // SPIKE: This drains *synchronously available* jobs. If a job is blocked on
        // a host async invoke (an unresolved Promise awaiting `eryx.invoke`), there
        // is nothing left to run and globals may not be final. That is exactly the
        // suspend point where a real impl returns `ExecuteResult::Pending`. TODO.
        if eval_ok.is_ok() {
            pump_jobs(&engine.runtime);
        }

        // Step 3: capture the result variable (read from globalThis), serialize, and
        // consume it so it can't leak into a later run. Returns (json, error) in one
        // shot, exactly like the Python guest.
        let outcome: Result<(String, String), String> =
            eval_ok.map(|()| engine.context.with(|ctx| capture_result(&ctx, &result_var)));

        // Snapshot output regardless of success/failure.
        let (stdout, stderr) = OUTPUT.with(|o| {
            let o = o.borrow();
            (o.stdout.clone(), o.stderr.clone())
        });

        match outcome {
            Ok((result, result_error)) => ExecuteResult::Complete(ExecuteOutput {
                stdout: stdout.trim_end_matches('\n').to_string(),
                stderr: stderr.trim_end_matches('\n').to_string(),
                result,
                result_error,
            }),
            Err(error) => {
                // Mirror the Python error path: consume any result the script set
                // before throwing so it can't leak into a later successful run.
                engine.context.with(|ctx| discard_result(&ctx, &result_var));
                // Prefer captured stderr + the exception message, like python.rs.
                let combined = if stderr.is_empty() {
                    error
                } else {
                    format!("{}\n{error}", stderr.trim_end_matches('\n'))
                };
                ExecuteResult::Error(combined)
            }
        }
    })
}

/// Pump the QuickJS job queue until no jobs remain.
///
/// This is the JS analog of running the asyncio event loop in `_eryx_async`. JS
/// Promise reactions are scheduled as QuickJS "jobs"; nothing runs them unless we
/// explicitly drive `execute_pending_job`. The pending-job API is on `Runtime`,
/// so we take the runtime directly (it is not reachable through `Ctx`).
fn pump_jobs(rt: &Runtime) {
    loop {
        match rt.execute_pending_job() {
            // A job ran; keep going — it may have scheduled more.
            Ok(true) => continue,
            // Queue empty.
            Ok(false) => break,
            // A job threw. Best-effort: surface to stderr and stop, mirroring how
            // an uncaught exception in an asyncio task is reported. A real impl
            // would thread this into the ExecuteResult::Error path.
            Err(e) => {
                do_report_output(1, &format!("uncaught (in promise): {e}\n"));
                break;
            }
        }
    }
}

// =============================================================================
// Result variable capture (mirrors _eryx_capture_result / discard_result)
// =============================================================================

/// Read `globalThis[name]`, JSON-serialize it, then delete it (consume-after-read).
///
/// Returns `(json, error)`:
///  - json: the JSON string, or "" if the variable is unset.
///  - error: a message when the variable exists but is not JSON-serializable,
///    else "". (JSON.stringify of `undefined` / a function yields `None` here,
///    which we treat as "unset".)
///
/// Never throws: result capture is a soft side channel, exactly like the Python
/// guest. JSON semantics differ slightly from Python — see the divergence notes in
/// lib.rs (BigInt throws, NaN/Infinity become `null`).
fn capture_result(ctx: &Ctx<'_>, name: &str) -> (String, String) {
    let globals = ctx.globals();

    // Does the variable exist and is it set to something serializable?
    let value: Value = match globals.get(name) {
        Ok(v) => v,
        // Not present in the global object => unset.
        Err(_) => return (String::new(), String::new()),
    };

    if value.is_undefined() || value.is_null() {
        // Treat `undefined` as "unset" (Python treats a missing name the same way).
        // `null` is a legitimate JSON value, so serialize it.
        if value.is_undefined() {
            return (String::new(), String::new());
        }
    }

    let json = match ctx.json_stringify(value) {
        // QuickJS returns Some(String) for serializable values, None for things
        // JSON.stringify drops (undefined, functions, symbols).
        Ok(Some(s)) => s.to_string().unwrap_or_default(),
        Ok(None) => String::new(),
        Err(e) => {
            // e.g. a BigInt, or a value with a throwing toJSON.
            let err = format!("result is not JSON-serializable: {e}");
            // Consume even on failure so it can't leak forward.
            discard_result(ctx, name);
            return (String::new(), err);
        }
    };

    // Consume the variable so a later run that doesn't set it reports no result.
    discard_result(ctx, name);
    (json, String::new())
}

/// Delete the result variable from the global object without reading it. Mirrors
/// `python::discard_result`. Best-effort: failure is ignored.
fn discard_result(ctx: &Ctx<'_>, name: &str) {
    // `delete globalThis[name]` is the cleanest cross-engine way to drop it.
    let _ = ctx.eval::<(), _>(format!("delete globalThis[{name:?}];"));
}

// =============================================================================
// Snapshot / restore state
// =============================================================================
//
// SPIKE: SEMANTIC GAP — read this carefully.
//
// The Python guest implements snapshot/restore by pickling (`dill.dumps`) the live
// `_eryx_user_globals` dict: it serializes *live objects* — functions, closures,
// class instances — and reconstructs them on restore. This is the crux of session
// persistence for the Python sandbox.
//
// JavaScript / QuickJS has NO equivalent of pickle. There is no standard way to
// serialize a live function, a closure over captured variables, a class instance
// with its prototype chain, or a pending Promise. The realistic options are:
//
//   A) Wizer-style *memory snapshots*: snapshot the entire linear-memory image of
//      the WASM instance after warm-up (this is already how eryx pre-initializes
//      the Python stdlib). This captures the true live heap, including JS objects,
//      but is an all-or-nothing image, not a per-session `list<u8>` you can ship
//      between hosts. This is almost certainly the right answer for JS.
//
//   B) Logical JSON snapshot: walk the global object and `JSON.stringify` the
//      plain-data own-properties. This loses functions, classes, closures,
//      prototypes, Maps/Sets/Dates (unless special-cased), and cyclic refs. It is
//      a *best-effort data-only* snapshot, strictly weaker than Python's pickle.
//
// Below is a sketch of (B) so the WIT contract (`snapshot-state -> list<u8>`,
// `restore-state(list<u8>)`) is satisfiable, with a loud TODO. A production JS
// guest would lean on (A) for real fidelity.

/// SPIKE: Best-effort logical snapshot — JSON of the plain-data global properties.
/// See the module-level gap note. Functions/classes/closures are silently dropped.
pub fn snapshot_state() -> Result<Vec<u8>, String> {
    ENGINE.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let engine = borrow
            .as_mut()
            .ok_or_else(|| "JS engine not initialized".to_string())?;

        engine.context.with(|ctx| {
            // Collect own enumerable, JSON-serializable globals, skipping the
            // intrinsics we installed (console, eryx) and JS built-ins. This is the
            // moral equivalent of python.rs's `_eryx_exclude` set.
            //
            // TODO: This drops every non-data value. Prefer Wizer memory snapshots
            // (option A) for a faithful port.
            let json: rquickjs::String = ctx
                .eval(
                    r#"
                    (() => {
                        const skip = new Set(['console', 'eryx', 'globalThis']);
                        const out = {};
                        for (const k of Object.getOwnPropertyNames(globalThis)) {
                            if (skip.has(k)) continue;
                            const v = globalThis[k];
                            const t = typeof v;
                            if (t === 'function' || t === 'symbol' || t === 'undefined') continue;
                            try { JSON.stringify(v); out[k] = v; } catch {}
                        }
                        return JSON.stringify(out);
                    })()
                    "#,
                )
                .map_err(|e| format!("snapshot serialization failed: {e}"))?;

            let s = json.to_string().map_err(|e| format!("{e}"))?;
            Ok(s.into_bytes())
        })
    })
}

/// SPIKE: Restore the best-effort logical snapshot produced by `snapshot_state`.
/// Re-assigns the plain-data properties onto the global object. See gap note.
pub fn restore_state(data: &[u8]) -> Result<(), String> {
    let json = std::str::from_utf8(data).map_err(|e| format!("snapshot is not UTF-8: {e}"))?;

    ENGINE.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let engine = borrow
            .as_mut()
            .ok_or_else(|| "JS engine not initialized".to_string())?;

        engine.context.with(|ctx| {
            // Parse defensively inside JS and copy keys onto globalThis. We pass the
            // snapshot through a global temp to avoid string-injecting user data
            // into eval (the same class of bug python.rs avoids with PyDict_SetItem).
            let globals = ctx.globals();
            globals
                .set("__eryx_restore_blob", json)
                .map_err(|e| format!("{e}"))?;

            ctx.eval::<(), _>(
                r#"
                (() => {
                    const data = JSON.parse(__eryx_restore_blob);
                    for (const k of Object.keys(data)) {
                        globalThis[k] = data[k];
                    }
                    delete globalThis.__eryx_restore_blob;
                })();
                "#,
            )
            .map_err(|e| format!("snapshot restore failed: {e}"))?;
            Ok(())
        })
    })
}

/// Clear all user-defined state, keeping the installed intrinsics. Mirrors
/// `python::clear_state` (which keeps callback infra + builtins).
pub fn clear_state() {
    ENGINE.with(|cell| {
        if let Some(engine) = cell.borrow().as_ref() {
            engine.context.with(|ctx| {
                // Delete every own global property except our intrinsics and the JS
                // built-ins that live on the global object.
                let _ = ctx.eval::<(), _>(
                    r#"
                    (() => {
                        const keep = new Set(['console', 'eryx', 'globalThis']);
                        for (const k of Object.getOwnPropertyNames(globalThis)) {
                            if (keep.has(k)) continue;
                            const d = Object.getOwnPropertyDescriptor(globalThis, k);
                            // Don't touch non-configurable built-ins (Object, Array, ...).
                            if (d && d.configurable) {
                                try { delete globalThis[k]; } catch {}
                            }
                        }
                    })();
                    "#,
                );
            });
        }
    });
}

/// SPIKE: Resume a suspended async export after a host callback completes. This is
/// the JS analog of `python::call_python_callback` -> `_eryx_async.resume`. The
/// real implementation would resolve the stashed Promise (see `install_eryx_intrinsics`)
/// and re-`pump_jobs`, returning either a new WAIT code or EXIT. Stubbed.
pub fn resume_async(_event0: u32, _event1: u32, _event2: u32) -> u32 {
    // TODO: resolve the pending Promise tied to event1 (the subtask id), then
    // pump_jobs; return WAIT|set<<4 if still pending, else 0 (EXIT).
    0
}

// `Func` is imported to document the closure-registration pattern; silence unused.
#[allow(unused_imports)]
use Func as _FuncMarker;
