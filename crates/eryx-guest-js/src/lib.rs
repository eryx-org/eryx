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
//! Not implemented yet: `snapshot-state`/`restore-state` (#823), trace events
//! (#824), and subtask cancellation, which the host does by interrupting the
//! guest instead.

#![allow(unsafe_code)]
// Same convention as the CPython guest: a broken invariant in FFI glue is a
// trap either way, so panicking with a message is the most useful failure.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod fetch;
mod fresh;
mod js;
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
            EXPORT_SNAPSHOT_STATE => {
                push_err(
                    &mut cx,
                    "snapshot-state is not supported by the JS guest yet",
                );
            }
            EXPORT_RESTORE_STATE => {
                let _ = cx.stack.pop();
                push_err(
                    &mut cx,
                    "restore-state is not supported by the JS guest yet",
                );
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
