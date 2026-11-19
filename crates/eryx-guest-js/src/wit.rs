//! Component Model intrinsics and the host imports the JS guest uses.
//!
//! Same imports and marshalling as the CPython guest (`eryx-guest-python`).

use std::cell::Cell;

use wit_dylib_ffi::Wit;

use eryx_guest_call::{EryxCall, Value};

#[link(wasm_import_module = "$root")]
unsafe extern "C" {
    #[link_name = "[waitable-set-new]"]
    pub(crate) fn waitable_set_new() -> u32;
    #[link_name = "[waitable-set-drop]"]
    pub(crate) fn waitable_set_drop(set: u32);
    /// The next event in `set` without blocking: its code, with the
    /// waitable's handle and status written to `payload`.
    #[link_name = "[waitable-set-poll]"]
    pub(crate) fn waitable_set_poll(set: u32, payload: *mut [u32; 2]) -> u32;
    #[link_name = "[waitable-join]"]
    pub(crate) fn waitable_join(waitable: u32, set: u32);
    #[link_name = "[context-set-0]"]
    pub(crate) fn context_set(ptr: u32);
    #[link_name = "[context-get-0]"]
    pub(crate) fn context_get() -> u32;
    #[link_name = "[subtask-drop]"]
    pub(crate) fn subtask_drop(task: u32);
    /// Cancel an in-flight subtask, blocking until the host has stopped it,
    /// and return its terminal status. It must not be in a waitable set.
    #[link_name = "[subtask-cancel]"]
    pub(crate) fn subtask_cancel(task: u32) -> u32;
}

thread_local! {
    static WIT: Cell<Option<Wit>> = const { Cell::new(None) };
}

pub(crate) fn set_wit(wit: Wit) {
    WIT.with(|cell| cell.set(Some(wit)));
}

pub(crate) fn wit() -> Wit {
    WIT.with(Cell::get).expect("wit-dylib not initialized")
}

/// A host callback, as reported by `list-callbacks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallbackInfo {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters_schema_json: String,
}

/// `list-callbacks: func() -> list<callback-info>`
pub(crate) fn list_callbacks() -> Vec<CallbackInfo> {
    let Some(import) = wit().get_import(None, "list-callbacks") else {
        return Vec::new();
    };
    let mut cx = EryxCall::new();
    import.call_import_sync(&mut cx);
    let Some(Value::GenericList(items)) = cx.stack.pop() else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| match item {
            Value::Record(fields) => match <[Value; 3]>::try_from(fields) {
                Ok(
                    [
                        Value::String(name),
                        Value::String(description),
                        Value::String(schema),
                    ],
                ) => Some(CallbackInfo {
                    name,
                    description,
                    parameters_schema_json: schema,
                }),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// `report-output: func(stream-id: u32, data: list<u8>)`
pub(crate) fn report_output(stream: u32, data: &[u8]) {
    let Some(import) = wit().get_import(None, "report-output") else {
        return;
    };
    let mut cx = EryxCall::new();
    // Reverse declaration order; the list must stay last (see the CPython guest).
    cx.stack.push(Value::Bytes(data.to_vec()));
    cx.stack.push(Value::U32(stream));
    import.call_import_sync(&mut cx);
}

/// `report-trace: func(lineno: u32, event-json: string, context-json: string)`
pub(crate) fn report_trace(lineno: u32, event_json: &str, context_json: &str) {
    let Some(import) = wit().get_import(None, "report-trace") else {
        return;
    };
    let mut cx = EryxCall::new();
    // Reverse declaration order, as for the CPython guest.
    cx.stack.push(Value::String(context_json.to_string()));
    cx.stack.push(Value::String(event_json.to_string()));
    cx.stack.push(Value::U32(lineno));
    import.call_import_sync(&mut cx);
}

/// An `invoke` the host hasn't finished yet.
pub(crate) struct PendingInvokeCall {
    pub(crate) subtask: u32,
    buffer: *mut u8,
    /// Owns `buffer` (wit-dylib defers its deallocation to the call context).
    cx: Box<EryxCall>,
}

pub(crate) enum InvokeStart {
    Done(Result<String, String>),
    Pending(PendingInvokeCall),
}

/// Start `invoke: async func(name: string, arguments-json: string) -> result<string, string>`.
pub(crate) fn invoke_start(name: &str, args_json: &str) -> Result<InvokeStart, String> {
    let import = wit()
        .get_import(None, "invoke")
        .ok_or_else(|| "invoke import not found".to_string())?;
    let mut cx = Box::new(EryxCall::new());
    cx.stack.push(Value::String(args_json.to_string()));
    cx.stack.push(Value::String(name.to_string()));
    // SAFETY: called from inside an async export, with a fresh call context.
    match unsafe { import.call_import_async(&mut *cx) } {
        None => Ok(InvokeStart::Done(pop_invoke_result(&mut cx))),
        Some(pending) => Ok(InvokeStart::Pending(PendingInvokeCall {
            subtask: pending.subtask,
            buffer: pending.buffer,
            cx,
        })),
    }
}

/// Lift the result of an `invoke` whose subtask has returned.
pub(crate) fn invoke_finish(mut call: PendingInvokeCall) -> Result<String, String> {
    let import = wit()
        .get_import(None, "invoke")
        .expect("invoke import exists if a call is pending");
    // SAFETY: the subtask returned, so the host has written the result to `buffer`.
    unsafe { import.lift_import_async_result(&mut *call.cx, call.buffer) };
    pop_invoke_result(&mut call.cx)
}

/// A `sleep` the host hasn't finished yet.
pub(crate) struct PendingSleep {
    pub(crate) subtask: u32,
    buffer: *mut u8,
    /// Owns `buffer`, as for an invoke.
    cx: Box<EryxCall>,
}

/// Start `sleep: async func(duration-ns: u64)`. `None` if the host finished
/// it at once.
pub(crate) fn sleep_start(duration_ns: u64) -> Option<PendingSleep> {
    let import = wit()
        .get_import(None, "sleep")
        .expect("the sandbox world imports sleep");
    let mut cx = Box::new(EryxCall::new());
    cx.stack.push(Value::U64(duration_ns));
    // SAFETY: called from inside an async export, with a fresh call context.
    let pending = unsafe { import.call_import_async(&mut *cx) }?;
    Some(PendingSleep {
        subtask: pending.subtask,
        buffer: pending.buffer,
        cx,
    })
}

/// Free a `sleep` whose subtask has returned.
pub(crate) fn sleep_finish(mut call: PendingSleep) {
    let import = wit()
        .get_import(None, "sleep")
        .expect("sleep import exists if a call is pending");
    // SAFETY: the subtask returned, so the host has finished with `buffer`.
    unsafe { import.lift_import_async_result(&mut *call.cx, call.buffer) };
}

fn pop_invoke_result(cx: &mut EryxCall) -> Result<String, String> {
    match (cx.stack.pop(), cx.stack.pop()) {
        (Some(Value::ResultDiscriminant(true)), Some(Value::String(json))) => Ok(json),
        (Some(Value::ResultDiscriminant(false)), Some(Value::String(error))) => Err(error),
        other => Err(format!("malformed invoke result: {other:?}")),
    }
}
