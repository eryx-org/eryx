//! The QuickJS engine and the Promise <-> Component Model async bridge.
//!
//! The bridge follows the shape of componentize-qjs's `task.rs`:
//!
//! - `invoke()` returns a Promise. If the host defers the call, its resolvers
//!   are parked in [`Task::pending`] keyed by subtask handle, and the subtask
//!   is joined to the task's waitable set.
//! - [`poll`] drains the QuickJS job queue, then looks at the completion
//!   promise of the evaluated script: settled means `task.return` + `EXIT`;
//!   still pending with parked invokes means `WAIT` on the waitable set.
//! - While waiting, the whole [`Task`] is moved into the Component Model
//!   context slot (`context.set`), and [`resume`] takes it back.
//!
//! Nothing here knows about fuel or suspension: a callback that suspends
//! poisons fuel on the host, the guest traps wherever it is next metered, and
//! the instance is discarded.

use std::cell::RefCell;
use std::collections::HashMap;

use rquickjs::context::EvalOptions;
use rquickjs::promise::PromiseState;
use rquickjs::{
    Array, CatchResultExt, CaughtError, Context, Ctx, Exception, Function, Object, Persistent,
    Promise, Runtime, Value, qjs,
};
use wit_dylib_ffi::ExportFunction;

use crate::wit::{self, CallbackInfo, InvokeStart, PendingInvokeCall};
use eryx_guest_call::{EryxCall, Value as WitValue};

const PRELUDE: &str = include_str!("prelude.js");

// Component Model callback codes and events.
const CALLBACK_EXIT: u32 = 0;
const CALLBACK_WAIT: u32 = 2;
const EVENT_NONE: u32 = 0;
const EVENT_SUBTASK: u32 = 1;
const EVENT_TASK_CANCELLED: u32 = 6;
const SUBTASK_RETURNED: u32 = 2;

/// Why a cancelled `execute` failed.
const CANCELLED: &str = "execution cancelled";

// Fields drop in order, and each must go before the next: QuickJS asserts
// that no object is alive when the runtime is freed.
struct Engine {
    /// The object returned by `prelude.js`.
    hooks: Persistent<Object<'static>>,
    context: Context,
    _runtime: Runtime,
}

/// An in-flight `execute`.
struct Task {
    export: ExportFunction,
    cx: Box<EryxCall>,
    /// The promise returned by evaluating the script; `None` only while the
    /// script's synchronous part is still being evaluated.
    completion: Option<Persistent<Promise<'static>>>,
    pending: HashMap<u32, PendingInvoke>,
    waitable_set: Option<u32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// What the script's `import` declarations bind, kept for snapshots
    /// once it succeeds.
    imports: Vec<crate::modules::ImportRecord>,
}

struct PendingInvoke {
    call: PendingInvokeCall,
    resolve: Persistent<Function<'static>>,
    reject: Persistent<Function<'static>>,
}

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
    static TASK: RefCell<Option<Task>> = const { RefCell::new(None) };
    static RESULT_VARIABLE: RefCell<String> = RefCell::new("result".to_string());
    static INSTALLED_CALLBACKS: RefCell<Option<Vec<CallbackInfo>>> = const { RefCell::new(None) };
}

impl Engine {
    fn new() -> Self {
        let runtime = Runtime::new().expect("failed to create QuickJS runtime");
        // Imports resolve against the sandbox's environment (src/modules.rs).
        runtime.set_loader(crate::modules::EnvResolver, crate::modules::EnvLoader);
        let context = Context::full(&runtime).expect("failed to create QuickJS context");
        let hooks = context.with(|ctx| {
            let native = Object::new(ctx.clone())?;
            native.set("invoke", Function::new(ctx.clone(), native_invoke)?)?;
            native.set("write", Function::new(ctx.clone(), native_write)?)?;
            native.set("trace", Function::new(ctx.clone(), native_trace)?)?;
            native.set(
                "evalScript",
                Function::new(ctx.clone(), native_eval_script)?,
            )?;
            let prelude: Function<'_> = ctx.eval(PRELUDE)?;
            let hooks: Object<'_> = prelude.call((native,))?;
            crate::fetch::install(&ctx)?;
            crate::web::install(&ctx)?;
            crate::modules::install(&ctx)?;
            Ok::<_, rquickjs::Error>(Persistent::save(&ctx, hooks))
        });
        let hooks = hooks.unwrap_or_else(|e| panic!("failed to evaluate prelude: {e}"));
        Self {
            hooks,
            context,
            _runtime: runtime,
        }
    }
}

/// Run `f` inside the (lazily created) QuickJS context.
fn with_ctx<R>(f: impl for<'js> FnOnce(&Ctx<'js>, &Object<'js>) -> R) -> R {
    let (context, hooks) = ENGINE.with(|engine| {
        let mut engine = engine.borrow_mut();
        let engine = engine.get_or_insert_with(Engine::new);
        (engine.context.clone(), engine.hooks.clone())
    });
    context.with(|ctx| {
        let hooks = hooks
            .restore(&ctx)
            .expect("prelude hooks belong to this runtime");
        f(&ctx, &hooks)
    })
}

fn with_task<R>(f: impl FnOnce(&mut Task) -> R) -> Option<R> {
    TASK.with(|task| task.borrow_mut().as_mut().map(f))
}

fn call_hook<'js, R: rquickjs::FromJs<'js>>(
    hooks: &Object<'js>,
    name: &str,
    args: impl rquickjs::function::IntoArgs<'js>,
) -> rquickjs::Result<R> {
    hooks.get::<_, Function<'js>>(name)?.call(args)
}

/// `__eryx.invoke(name, argsJson)`: start a host `invoke` and return a Promise
/// for its JSON result.
fn native_invoke<'js>(
    ctx: Ctx<'js>,
    name: String,
    args_json: String,
) -> rquickjs::Result<Promise<'js>> {
    if with_task(|_| ()).is_none() {
        return Err(Exception::throw_message(
            &ctx,
            "invoke() can only be called while code is executing",
        ));
    }
    let (promise, resolve, reject) = ctx.promise()?;
    match wit::invoke_start(&name, &args_json) {
        Err(msg) => return Err(Exception::throw_message(&ctx, &msg)),
        Ok(InvokeStart::Done(result)) => settle(&ctx, &resolve, &reject, result)?,
        Ok(InvokeStart::Pending(call)) => {
            let pending = PendingInvoke {
                call,
                resolve: Persistent::save(&ctx, resolve),
                reject: Persistent::save(&ctx, reject),
            };
            with_task(|task| task.register(pending));
        }
    }
    Ok(promise)
}

/// `__eryx.write(stream, text)`: buffer output for `execute-output` and stream
/// it to the host as it happens. 0 = stdout, 1 = stderr.
fn native_write(stream: u32, text: String) {
    let bytes = text.into_bytes();
    wit::report_output(stream, &bytes);
    with_task(|task| {
        let buf = if stream == 1 {
            &mut task.stderr
        } else {
            &mut task.stdout
        };
        buf.extend_from_slice(&bytes);
    });
}

/// `__eryx.trace(lineno, eventJson, contextJson)`: a `report-trace` event.
fn native_trace(lineno: u32, event_json: String, context_json: String) {
    wit::report_trace(lineno, &event_json, &context_json);
}

/// `__eryx.evalScript(source)`: run `source` as global code, whose
/// `let`/`const` declarations persist (indirect eval's don't).
fn native_eval_script(ctx: Ctx<'_>, source: String) -> rquickjs::Result<()> {
    ctx.eval::<(), _>(source)
}

fn settle<'js>(
    ctx: &Ctx<'js>,
    resolve: &Function<'js>,
    reject: &Function<'js>,
    result: Result<String, String>,
) -> rquickjs::Result<()> {
    match result {
        Ok(json) => resolve.call((json,)),
        Err(msg) => {
            let error = Exception::from_message(ctx.clone(), &msg)?;
            reject.call((error,))
        }
    }
}

impl Task {
    fn new(export: ExportFunction, cx: Box<EryxCall>) -> Self {
        Self {
            export,
            cx,
            completion: None,
            pending: HashMap::new(),
            waitable_set: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            imports: Vec::new(),
        }
    }

    fn register(&mut self, pending: PendingInvoke) {
        let set = *self
            .waitable_set
            .get_or_insert_with(|| unsafe { wit::waitable_set_new() });
        let handle = pending.call.subtask;
        unsafe { wit::waitable_join(handle, set) };
        self.pending.insert(handle, pending);
    }

    fn take(&mut self, handle: u32) -> Option<PendingInvoke> {
        let pending = self.pending.remove(&handle)?;
        unsafe { wit::waitable_join(handle, 0) };
        Some(pending)
    }
}

/// Render a caught error the way `console.error` would, via the prelude.
fn format_caught<'js>(ctx: &Ctx<'js>, hooks: &Object<'js>, err: CaughtError<'js>) -> String {
    let value = match err {
        CaughtError::Exception(e) => e.into_value(),
        CaughtError::Value(v) => v,
        CaughtError::Error(e) => return e.to_string(),
    };
    format_value(ctx, hooks, value)
}

fn format_value<'js>(ctx: &Ctx<'js>, hooks: &Object<'js>, value: Value<'js>) -> String {
    call_hook::<String>(hooks, "formatError", (value,))
        .catch(ctx)
        .unwrap_or_else(|e| format!("uncaught exception (and formatting it failed: {e})"))
}

/// Tell the prelude about the host's current callbacks, if they changed.
fn install_callbacks<'js>(ctx: &Ctx<'js>, hooks: &Object<'js>) {
    let callbacks = wit::list_callbacks();
    let unchanged =
        INSTALLED_CALLBACKS.with(|installed| installed.borrow().as_ref() == Some(&callbacks));
    if unchanged {
        return;
    }
    let installed = (|| {
        let list = Array::new(ctx.clone())?;
        for (i, cb) in callbacks.iter().enumerate() {
            let obj = Object::new(ctx.clone())?;
            obj.set("name", cb.name.as_str())?;
            obj.set("description", cb.description.as_str())?;
            obj.set("parameters_schema_json", cb.parameters_schema_json.as_str())?;
            list.set(i, obj)?;
        }
        call_hook::<()>(hooks, "installCallbacks", (list,))
    })()
    .catch(ctx);
    match installed {
        Ok(()) => INSTALLED_CALLBACKS.with(|installed| *installed.borrow_mut() = Some(callbacks)),
        Err(e) => eprintln!("eryx-js: failed to install callbacks: {e}"),
    }
}

/// Start an `execute(code)` export. Returns a Component Model callback code.
/// First line of framed code: `#eryx-preamble:<len>`, then `<len>` bytes of
/// host preamble, then the user code. The host frames every execution; only
/// one leading header is split off. Must match the host's `frame_code`.
const PREAMBLE_HEADER: &str = "#eryx-preamble:";

/// Split framed `code` into `(preamble, user_code)`. Code without a
/// well-formed header is all user code.
fn split_preamble(code: &str) -> (&str, &str) {
    code.strip_prefix(PREAMBLE_HEADER)
        .and_then(|rest| {
            let (len, rest) = rest.split_once('\n')?;
            let len: usize = len.parse().ok()?;
            Some((rest.get(..len)?, rest.get(len..)?))
        })
        .unwrap_or(("", code))
}

pub(crate) fn start_execute(export: ExportFunction, cx: Box<EryxCall>, code: String) -> u32 {
    // The host's JavaScript preamble (the secrets' `process.env`) runs first.
    let (preamble, code) = split_preamble(&code);
    // Leading `import` declarations become dynamic imports (src/modules.rs).
    let (code, imports) = crate::modules::rewrite_imports(code);
    let (preamble, code) = (preamble.to_string(), code.into_owned());
    let mut task = Task::new(export, cx);
    task.imports = imports;
    TASK.with(|slot| *slot.borrow_mut() = Some(task));
    with_ctx(|ctx, hooks| {
        crate::fresh::reseed(ctx).expect("failed to reseed the QuickJS context");
        install_callbacks(ctx, hooks);
        if let Err(e) = ctx.eval::<(), _>(preamble).catch(ctx) {
            let msg = format_caught(ctx, hooks, e);
            return finish(hooks, Err(msg));
        }
        // After the preamble, which sets `process`: what's there now is the
        // host's, not session state.
        if let Err(e) = call_hook::<()>(hooks, "markHostGlobals", ()).catch(ctx) {
            eprintln!("eryx-js: failed to record the host's globals: {e}");
        }
        if let Err(e) = call_hook::<()>(hooks, "noteCode", (code.as_str(),)).catch(ctx) {
            eprintln!("eryx-js: failed to note the code's names: {e}");
        }

        let mut options = EvalOptions::default();
        // Sloppy global code with top-level await: `var`/`let`/`const`/
        // `function` declarations all persist across executes.
        options.strict = false;
        options.promise = true;
        options.filename = Some("<eryx>".to_string());

        match ctx
            .eval_with_options::<Promise<'_>, _>(code, options)
            .catch(ctx)
        {
            Ok(promise) => {
                let promise = Persistent::save(ctx, promise);
                with_task(|task| task.completion = Some(promise));
                poll(ctx, hooks)
            }
            // Syntax errors are thrown synchronously rather than rejecting.
            Err(e) => {
                let msg = format_caught(ctx, hooks, e);
                finish(hooks, Err(msg))
            }
        }
    })
}

/// What a Component Model event means for the suspended `execute`.
#[derive(Debug, PartialEq, Eq)]
enum AsyncEvent {
    /// A parked `invoke` returned.
    Returned,
    /// Nothing to do: no event, or a subtask's STARTING/STARTED progress.
    Nothing,
    /// The host cancelled the `execute`.
    Cancelled,
    /// Streams and futures, which aren't wired up.
    Unexpected,
}

impl AsyncEvent {
    fn of(event: u32, status: u32) -> Self {
        match (event, status) {
            (EVENT_SUBTASK, SUBTASK_RETURNED) => Self::Returned,
            (EVENT_SUBTASK, _) | (EVENT_NONE, _) => Self::Nothing,
            (EVENT_TASK_CANCELLED, _) => Self::Cancelled,
            _ => Self::Unexpected,
        }
    }
}

/// Handle a Component Model event for the suspended `execute`.
pub(crate) fn resume(event: u32, handle: u32, status: u32) -> u32 {
    let ptr = unsafe { wit::context_get() };
    assert_ne!(ptr, 0, "async callback without a suspended task");
    unsafe { wit::context_set(0) };
    // SAFETY: `suspend` stored this pointer with `Box::into_raw`, and the host
    // hands it back exactly once.
    let task = unsafe { *Box::from_raw(ptr as usize as *mut Task) };
    TASK.with(|slot| *slot.borrow_mut() = Some(task));

    with_ctx(|ctx, hooks| {
        match AsyncEvent::of(event, status) {
            AsyncEvent::Returned => {
                let pending = with_task(|task| task.take(handle)).flatten();
                if let Some(pending) = pending {
                    let result = wit::invoke_finish(pending.call);
                    unsafe { wit::subtask_drop(handle) };
                    let settled = (|| {
                        let resolve = pending.resolve.restore(ctx)?;
                        let reject = pending.reject.restore(ctx)?;
                        settle(ctx, &resolve, &reject, result)
                    })();
                    if let Err(e) = settled {
                        eprintln!("eryx-js: failed to settle invoke promise: {e}");
                    }
                }
            }
            AsyncEvent::Nothing => {}
            AsyncEvent::Cancelled => return cancel(ctx, hooks),
            AsyncEvent::Unexpected => {
                eprintln!("eryx-js: ignoring unexpected async event {event}");
            }
        }
        poll(ctx, hooks)
    })
}

/// The host cancelled the `execute`: stop every parked `invoke`, reject its
/// promise (so the script's `catch`/`finally` run, as a cancelled asyncio
/// task's do), and finish as a script failure.
fn cancel<'js>(ctx: &Ctx<'js>, hooks: &Object<'js>) -> u32 {
    let pending: Vec<_> = with_task(|task| {
        let handles: Vec<u32> = task.pending.keys().copied().collect();
        handles
            .into_iter()
            .filter_map(|handle| task.take(handle))
            .collect()
    })
    .unwrap_or_default();
    for PendingInvoke { call, reject, .. } in pending {
        // `take` removed it from the waitable set.
        cancel_subtask(call);
        let rejected = (|| {
            let reject = reject.restore(ctx)?;
            reject.call::<_, ()>((Exception::from_message(ctx.clone(), CANCELLED)?,))
        })();
        if let Err(e) = rejected {
            eprintln!("eryx-js: failed to reject a cancelled invoke: {e}");
        }
    }
    while ctx.execute_pending_job() {}
    finish(hooks, Err(CANCELLED.to_string()))
}

/// Cancel an in-flight `invoke`, which must not be in a waitable set, and
/// drop its subtask, as the CPython guest's `_cancel_subtask` does. The
/// guest stops waiting; the host still runs the callback to the end.
fn cancel_subtask(call: PendingInvokeCall) {
    let handle = call.subtask;
    // SAFETY: a live subtask handle of this task, out of any waitable set.
    let status = unsafe { wit::subtask_cancel(handle) };
    if status == SUBTASK_RETURNED {
        // The host finished first: lift its result to free it, unused.
        let _ = wit::invoke_finish(call);
    }
    unsafe { wit::subtask_drop(handle) };
}

/// Drain the job queue, then either finish the task or suspend it.
fn poll<'js>(ctx: &Ctx<'js>, hooks: &Object<'js>) -> u32 {
    while ctx.execute_pending_job() {}

    let (completion, has_pending) = with_task(|task| {
        let completion = task.completion.clone().expect("completion promise set");
        (completion, !task.pending.is_empty())
    })
    .expect("poll without an active task");
    let promise = completion.restore(ctx).expect("completion promise restore");

    match promise.state() {
        PromiseState::Resolved => finish(hooks, Ok(())),
        PromiseState::Rejected => {
            // `result` rethrows the rejection reason so `catch` can pick it up.
            let _ = promise.result::<Value<'_>>();
            let msg = format_value(ctx, hooks, ctx.catch());
            finish(hooks, Err(msg))
        }
        PromiseState::Pending if has_pending => suspend(),
        PromiseState::Pending => finish(
            hooks,
            Err(
                "execution never completed: the script awaited a promise that nothing will resolve"
                    .to_string(),
            ),
        ),
    }
}

fn suspend() -> u32 {
    let task = TASK
        .with(|task| task.borrow_mut().take())
        .expect("suspend without an active task");
    let set = task
        .waitable_set
        .expect("pending invokes imply a waitable set");
    let ptr = Box::into_raw(Box::new(task)) as usize;
    unsafe { wit::context_set(u32::try_from(ptr).expect("wasm32 pointer")) };
    CALLBACK_WAIT | (set << 4)
}

/// Lower the `execute` result, call `task.return`, and tear the task down.
fn finish(hooks: &Object<'_>, outcome: Result<(), String>) -> u32 {
    let task = TASK
        .with(|task| task.borrow_mut().take())
        .expect("finish without an active task");
    let Task {
        export,
        mut cx,
        pending,
        waitable_set,
        stdout,
        stderr,
        imports,
        ..
    } = task;
    let result_variable = RESULT_VARIABLE.with(|name| name.borrow().clone());

    match outcome {
        Ok(()) => {
            if let Err(e) = commit_imports(hooks, &imports) {
                eprintln!("eryx-js: failed to record the script's imports: {e}");
            }
            // `captureResult` returns `[json, error]`.
            let (result, result_error) =
                match call_hook::<Vec<String>>(hooks, "captureResult", (result_variable,)) {
                    Ok(pair) => match <[String; 2]>::try_from(pair) {
                        Ok([result, error]) => (result, error),
                        Err(_) => (String::new(), "malformed result capture".to_string()),
                    },
                    Err(e) => (String::new(), format!("result capture failed: {e}")),
                };
            // execute-output { stdout, stderr, result-json, result-error }
            cx.stack.push(WitValue::Record(vec![
                WitValue::Bytes(stdout),
                WitValue::Bytes(stderr),
                WitValue::String(result),
                WitValue::String(result_error),
            ]));
            cx.stack.push(WitValue::ResultDiscriminant(true));
        }
        Err(msg) => {
            // Consume the result variable so it can't leak into a later run.
            let _ = call_hook::<()>(hooks, "discardResult", (result_variable,));
            let _ = call_hook::<()>(hooks, "discardImports", ());
            // execute-error { message, exit-code, stdout, stderr }; a JS script
            // fails by throwing, so it has no exit code.
            cx.stack.push(WitValue::Record(vec![
                WitValue::String(msg),
                WitValue::OptionDiscriminant(false),
                WitValue::Bytes(stdout),
                WitValue::Bytes(stderr),
            ]));
            cx.stack.push(WitValue::ResultDiscriminant(false));
        }
    }

    // Invokes still in flight (never awaited, or started while a cancelled
    // execute unwound) are cancelled and released with the task, so their
    // handles don't outlive it.
    for (handle, pending) in pending {
        unsafe { wit::waitable_join(handle, 0) };
        cancel_subtask(pending.call);
    }
    if let Some(set) = waitable_set {
        unsafe { wit::waitable_set_drop(set) };
    }

    export.call_task_return(&mut *cx);
    CALLBACK_EXIT
}

/// Record imports that ran, for [`snapshot_state`].
fn commit_imports(
    hooks: &Object<'_>,
    imports: &[crate::modules::ImportRecord],
) -> rquickjs::Result<()> {
    if imports.is_empty() {
        return Ok(());
    }
    let ctx = hooks.ctx();
    let records = imports
        .iter()
        .map(|record| record.to_js(ctx))
        .collect::<rquickjs::Result<Vec<_>>>()?;
    call_hook::<()>(hooks, "commitImports", (records,))
}

pub(crate) fn set_result_variable(name: String) {
    RESULT_VARIABLE.with(|slot| *slot.borrow_mut() = name);
}

/// Version of the snapshot layout below. Bump it whenever the layout or the
/// prelude's entry shape changes. Layout 2 added the imports entry; layout
/// 1 snapshots (the same entries without it) still restore.
const STATE_LAYOUT: u32 = 2;
const OLDEST_STATE_LAYOUT: u32 = 1;

/// Start of a `restore-state` error for a snapshot in another state format,
/// followed by the snapshot's format, a tab, and the one this runtime reads.
/// Must match the host's `INCOMPATIBLE_STATE_PREFIX`.
const INCOMPATIBLE_STATE_PREFIX: &str = "eryx:incompatible-state-format:";

/// Largest snapshot the guest writes or reads, as the host's
/// `MAX_SNAPSHOT_SIZE`, so a big session fails here with a clear error
/// instead of being copied out.
const MAX_STATE_BYTES: usize = 10 * 1024 * 1024;

/// The format id on a snapshot's first line. `JS_WriteObject`'s encoding is
/// only read back by the same QuickJS, so the version is part of it.
fn state_format() -> String {
    state_format_for(STATE_LAYOUT)
}

fn state_format_for(layout: u32) -> String {
    // SAFETY: a static NUL-terminated version string.
    let version = unsafe { std::ffi::CStr::from_ptr(qjs::JS_GetVersion()) };
    format!(
        "eryx-js-state/{layout} quickjs-ng@{}",
        version.to_string_lossy()
    )
}

/// The pending exception's message, for an FFI call that failed.
fn caught_message<'js>(ctx: &Ctx<'js>, hooks: &Object<'js>) -> String {
    format_value(ctx, hooks, ctx.catch())
}

/// `JS_WriteObject` with object references (shared and cyclic objects), and
/// without bytecode, so any function in the graph fails the write.
fn write_object<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Option<Vec<u8>> {
    let raw = ctx.as_raw().as_ptr();
    let mut size: qjs::size_t = 0;
    // SAFETY: `value` is alive in `ctx`; the buffer is freed below.
    let ptr = unsafe {
        qjs::JS_WriteObject(
            raw,
            &mut size,
            value.as_raw(),
            qjs::JS_WRITE_OBJ_REFERENCE as i32,
        )
    };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: QuickJS returned `size` bytes at `ptr`, allocated in `ctx`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, size as usize) }.to_vec();
    unsafe { qjs::js_free(raw, ptr.cast()) };
    Some(bytes)
}

/// Serialize the session's top-level variables: the format id, a newline,
/// then the `JS_WriteObject` encoding of the `[name, kind, value]` entries
/// from the prelude's `stateEntries`. Values `JS_WriteObject` can't encode
/// (functions, classes, promises, and anything holding one) are left out and
/// named in the returned list.
pub(crate) fn snapshot_state() -> Result<(Vec<u8>, Vec<String>), String> {
    with_ctx(|ctx, hooks| {
        let entries = call_hook::<Vec<Array<'_>>>(hooks, "stateEntries", ())
            .catch(ctx)
            .map_err(|e| format!("failed to collect the session's variables: {e}"))?;
        let kept = Array::new(ctx.clone()).map_err(|e| e.to_string())?;
        let mut skipped = Vec::new();
        for entry in entries {
            let name: String = entry.get(0).map_err(|e| e.to_string())?;
            let value: Value<'_> = entry.get(2).map_err(|e| e.to_string())?;
            if write_object(ctx, &value).is_some() {
                kept.set(kept.len(), entry).map_err(|e| e.to_string())?;
            } else {
                // Not a failure of the snapshot: the name is reported instead.
                let _ = ctx.catch();
                skipped.push(name);
            }
        }
        let encoded = write_object(ctx, &kept.into_value()).ok_or_else(|| {
            format!(
                "failed to serialize the session: {}",
                caught_message(ctx, hooks)
            )
        })?;
        let mut data = format!("{}\n", state_format()).into_bytes();
        data.extend_from_slice(&encoded);
        if data.len() > MAX_STATE_BYTES {
            return Err(format!(
                "Snapshot too large: {} bytes (max {MAX_STATE_BYTES} bytes)",
                data.len()
            ));
        }
        Ok((data, skipped))
    })
}

/// Replace the session with one restored from [`snapshot_state`]'s data.
/// Another format id is refused with [`INCOMPATIBLE_STATE_PREFIX`]. The data
/// is read without bytecode (or SharedArrayBuffers), so a crafted snapshot
/// can only produce plain values; the prelude checks their shape. The
/// session's imports run again (from the environment) before its variables
/// are defined, so a reassigned import keeps its new value. On failure,
/// including a failed import, the session is left empty.
pub(crate) fn restore_state(data: &[u8]) -> Result<(), String> {
    let format = state_format();
    let (found, encoded) = match data.iter().position(|&b| b == b'\n') {
        Some(i) => (String::from_utf8_lossy(&data[..i]), &data[i + 1..]),
        // No format line: not a snapshot of any known format.
        None => (std::borrow::Cow::Borrowed(""), data),
    };
    if !(OLDEST_STATE_LAYOUT..=STATE_LAYOUT).any(|layout| found == state_format_for(layout)) {
        return Err(format!("{INCOMPATIBLE_STATE_PREFIX}{found}\t{format}"));
    }
    // Never written, so never restored: reading is bounded like writing.
    if data.len() > MAX_STATE_BYTES {
        return Err(format!(
            "Snapshot too large: {} bytes (max {MAX_STATE_BYTES} bytes)",
            data.len()
        ));
    }
    clear_state();
    let restored = with_ctx(|ctx, hooks| {
        // SAFETY: `encoded` outlives the call, which copies what it keeps.
        let raw = unsafe {
            qjs::JS_ReadObject(
                ctx.as_raw().as_ptr(),
                encoded.as_ptr(),
                encoded.len() as qjs::size_t,
                qjs::JS_READ_OBJ_REFERENCE as i32,
            )
        };
        // SAFETY: `raw` is an owned value (or the exception marker) in `ctx`.
        if unsafe { qjs::JS_IsException(raw) } {
            return Err(format!(
                "failed to read the snapshot: {}",
                caught_message(ctx, hooks)
            ));
        }
        let entries = unsafe { Value::from_raw(ctx.clone(), raw) };
        let failed = |e: rquickjs::CaughtError<'_>| format!("failed to restore the snapshot: {e}");
        let records = call_hook::<Vec<Object<'_>>>(hooks, "validateEntries", (entries.clone(),))
            .catch(ctx)
            .map_err(failed)?;
        let mut imports = Vec::with_capacity(records.len());
        for record in &records {
            let record = crate::modules::ImportRecord::from_js(record)
                .catch(ctx)
                .map_err(failed)?;
            crate::modules::run_import(ctx, &record.statement()).map_err(|e| {
                format!(
                    "failed to restore the snapshot: import from {:?}: {e}",
                    record.specifier
                )
            })?;
            imports.push(record);
        }
        commit_imports(hooks, &imports).catch(ctx).map_err(failed)?;
        call_hook::<()>(hooks, "applyEntries", (entries,))
            .catch(ctx)
            .map_err(failed)
    });
    if restored.is_err() {
        clear_state();
    }
    restored
}

/// Drop the whole JS context; the next execute starts from a fresh one.
pub(crate) fn clear_state() {
    INSTALLED_CALLBACKS.with(|installed| *installed.borrow_mut() = None);
    ENGINE.with(|engine| *engine.borrow_mut() = None);
    crate::fresh::forget();
}

#[cfg(test)]
mod tests {
    use super::{AsyncEvent, split_preamble};

    /// The host can't cancel an export task from outside (the cancel path is
    /// only reachable through the Component Model's task-cancelled event),
    /// so this is the event dispatch only.
    #[test]
    fn classifies_async_events() {
        assert_eq!(AsyncEvent::of(1, 2), AsyncEvent::Returned);
        assert_eq!(AsyncEvent::of(1, 1), AsyncEvent::Nothing);
        assert_eq!(AsyncEvent::of(0, 0), AsyncEvent::Nothing);
        assert_eq!(AsyncEvent::of(6, 0), AsyncEvent::Cancelled);
        assert_eq!(AsyncEvent::of(2, 0), AsyncEvent::Unexpected);
    }

    #[test]
    fn splits_one_framing_header() {
        assert_eq!(split_preamble("#eryx-preamble:0\nx"), ("", "x"));
        assert_eq!(split_preamble("#eryx-preamble:2\nabcd"), ("ab", "cd"));
        // Only one header is split off, and unframed code is all user code.
        assert_eq!(
            split_preamble("#eryx-preamble:0\n#eryx-preamble:1\nx"),
            ("", "#eryx-preamble:1\nx")
        );
        assert_eq!(split_preamble("let x = 1"), ("", "let x = 1"));
    }
}
