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
    Promise, Runtime, Value,
};
use wit_dylib_ffi::ExportFunction;

use crate::call::{EryxCall, Value as WitValue};
use crate::wit::{self, CallbackInfo, InvokeStart, PendingInvokeCall};

const PRELUDE: &str = include_str!("prelude.js");

// Component Model callback codes and events.
const CALLBACK_EXIT: u32 = 0;
const CALLBACK_WAIT: u32 = 2;
const EVENT_NONE: u32 = 0;
const EVENT_SUBTASK: u32 = 1;
const SUBTASK_RETURNED: u32 = 2;

struct Engine {
    // Must outlive `context`.
    _runtime: Runtime,
    context: Context,
    /// The object returned by `prelude.js`.
    hooks: Persistent<Object<'static>>,
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
        let context = Context::full(&runtime).expect("failed to create QuickJS context");
        let hooks = context.with(|ctx| {
            let native = Object::new(ctx.clone())?;
            native.set("invoke", Function::new(ctx.clone(), native_invoke)?)?;
            native.set("write", Function::new(ctx.clone(), native_write)?)?;
            let prelude: Function<'_> = ctx.eval(PRELUDE)?;
            let hooks: Object<'_> = prelude.call((native,))?;
            Ok::<_, rquickjs::Error>(Persistent::save(&ctx, hooks))
        });
        let hooks = hooks.unwrap_or_else(|e| panic!("failed to evaluate prelude: {e}"));
        Self {
            _runtime: runtime,
            context,
            hooks,
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
pub(crate) fn start_execute(export: ExportFunction, cx: Box<EryxCall>, code: String) -> u32 {
    TASK.with(|task| *task.borrow_mut() = Some(Task::new(export, cx)));
    with_ctx(|ctx, hooks| {
        install_callbacks(ctx, hooks);

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
        match (event, status) {
            (EVENT_SUBTASK, SUBTASK_RETURNED) => {
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
            // STARTING/STARTED progress events carry nothing for us.
            (EVENT_SUBTASK, _) | (EVENT_NONE, _) => {}
            // Spike: cancellation (and streams/futures) isn't wired up. The host
            // cancels by epoch-interrupting the guest, which never gets here.
            (other, _) => eprintln!("eryx-js: ignoring unexpected async event {other}"),
        }
        poll(ctx, hooks)
    })
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
        ..
    } = task;
    let result_variable = RESULT_VARIABLE.with(|name| name.borrow().clone());

    match outcome {
        Ok(()) => {
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
            cx.stack.push(WitValue::String(msg));
            cx.stack.push(WitValue::ResultDiscriminant(false));
        }
    }

    // Invokes that were started but never awaited (fire-and-forget) are
    // abandoned with the task.
    if let Some(set) = waitable_set {
        for handle in pending.keys() {
            unsafe { wit::waitable_join(*handle, 0) };
        }
        unsafe { wit::waitable_set_drop(set) };
    }
    drop(pending);

    export.call_task_return(&mut *cx);
    CALLBACK_EXIT
}

pub(crate) fn set_result_variable(name: String) {
    RESULT_VARIABLE.with(|slot| *slot.borrow_mut() = name);
}

/// Drop the whole JS context; the next execute starts from a fresh one.
pub(crate) fn clear_state() {
    INSTALLED_CALLBACKS.with(|installed| *installed.borrow_mut() = None);
    ENGINE.with(|engine| *engine.borrow_mut() = None);
}
