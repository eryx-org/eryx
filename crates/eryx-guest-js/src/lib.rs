//! The eryx JavaScript guest runtime.
//!
//! An implementation of the eryx `sandbox` world that runs JavaScript on
//! QuickJS (quickjs-ng, via `rquickjs`). It links into a component the same
//! way `eryx-guest-python` does, with the same value stack
//! (`eryx-guest-call`), and runs through the same host; see
//! `BUILD_ERYX_JS_RUNTIME` in `eryx-runtime/build.rs` and
//! `mise run build-runtime-javascript`.
//!
//! JS surface:
//! - `await invoke(name, args)` and one global function per host callback
//!   (`await get_time()`, `await http.get({url})`), plus `listCallbacks()`.
//! - `console.log/info/debug` (stdout) and `console.error/warn` (stderr).
//! - Top-level `await`; top-level declarations persist across a session's
//!   executes. Declaring a `let`/`const` again in a later execute throws a
//!   `SyntaxError`, as in Node's REPL; `var` and plain assignment can repeat.
//! - The result variable (default `result`) is JSON-serialized and consumed.
//! - `fetch` and friends over the host's `eryx:net` (`src/fetch.js`,
//!   `src/fetch.rs`). A fetch blocks the guest until its response is read.
//! - `process.env`, with secrets' placeholders, when the sandbox has secrets.
//!
//! - `snapshot-state` serializes the session's top-level variables (global
//!   properties and `let`/`const` bindings) with QuickJS's `JS_WriteObject`;
//!   ones it can't serialize (functions, classes, promises) are skipped by
//!   name. See `js::snapshot_state`.
//!
//! - Trace events: every callback call reports `callback_start` (with its
//!   arguments as context) and `callback_end` (with `error` if it failed),
//!   in the CPython guest's shape. There are no line, call, return or
//!   exception events: QuickJS has no hook for them.
//! - A cancelled `execute` (the Component Model's task-cancelled event)
//!   cancels its in-flight callbacks, rejects their promises and fails with
//!   "execution cancelled".

#![allow(unsafe_code)]
// Same convention as the CPython guest: a broken invariant in FFI glue is a
// trap either way, so panicking with a message is the most useful failure.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod fetch;
mod fresh;
mod js;
mod modules;
mod url;
mod wit;

use wit_dylib_ffi::{ExportFunction, Interpreter, Resource, Wit};

use eryx_guest_call::{EryxCall, Value};

/// Export function indices (must match the order in runtime.wit).
const EXPORT_EXECUTE: usize = 0;
const EXPORT_SNAPSHOT_STATE: usize = 1;
const EXPORT_RESTORE_STATE: usize = 2;
const EXPORT_CLEAR_STATE: usize = 3;
const EXPORT_FINALIZE_PREINIT: usize = 4;
const EXPORT_SET_RESULT_VARIABLE: usize = 5;

/// The `wit-dylib` interpreter for the JS guest.
#[derive(Debug)]
pub struct JsInterpreter;

fn push_err(cx: &mut EryxCall, msg: &str) {
    cx.stack.push(Value::String(msg.to_string()));
    cx.stack.push(Value::ResultDiscriminant(false));
}

impl Interpreter for JsInterpreter {
    type CallCx<'a> = EryxCall;

    fn initialize(wit: Wit) {
        wit::set_wit(wit);
    }

    fn export_start<'a>(_wit: Wit, _func: ExportFunction) -> Box<Self::CallCx<'a>> {
        Box::new(EryxCall::new())
    }

    fn export_call(_wit: Wit, func: ExportFunction, _cx: &mut Self::CallCx<'_>) {
        match func.index() {
            EXPORT_FINALIZE_PREINIT => fresh::forget(),
            other => panic!("unexpected sync export {other}"),
        }
    }

    fn export_async_start(
        _wit: Wit,
        func: ExportFunction,
        mut cx: Box<Self::CallCx<'static>>,
    ) -> u32 {
        use wit_dylib_ffi::Call;

        match func.index() {
            EXPORT_EXECUTE => {
                let code = cx.pop_string().to_string();
                return js::start_execute(func, cx, code);
            }
            // snapshot-state() -> result<state-snapshot, string>
            EXPORT_SNAPSHOT_STATE => match js::snapshot_state() {
                Ok((data, skipped)) => {
                    cx.stack.push(Value::Record(vec![
                        Value::Bytes(data),
                        Value::GenericList(skipped.into_iter().map(Value::String).collect()),
                    ]));
                    cx.stack.push(Value::ResultDiscriminant(true));
                }
                Err(msg) => push_err(&mut cx, &msg),
            },
            // restore-state(data: list<u8>) -> result<_, string>
            EXPORT_RESTORE_STATE => {
                let data = match cx.stack.pop() {
                    Some(Value::Bytes(bytes)) => bytes,
                    other => panic!("expected Bytes for restore-state data, got {other:?}"),
                };
                match js::restore_state(&data) {
                    Ok(()) => cx.stack.push(Value::ResultDiscriminant(true)),
                    Err(msg) => push_err(&mut cx, &msg),
                }
            }
            EXPORT_CLEAR_STATE => js::clear_state(),
            EXPORT_SET_RESULT_VARIABLE => js::set_result_variable(cx.pop_string().to_string()),
            other => panic!("unknown export function index: {other}"),
        }
        func.call_task_return(&mut *cx);
        0
    }

    fn export_async_callback(event0: u32, event1: u32, event2: u32) -> u32 {
        js::resume(event0, event1, event2)
    }

    fn resource_dtor(_ty: Resource, _handle: usize) {}
}

mod export {
    #![allow(missing_docs)]
    #![allow(clippy::not_unsafe_ptr_arg_deref)]
    use super::JsInterpreter;
    wit_dylib_ffi::export!(JsInterpreter);
}
