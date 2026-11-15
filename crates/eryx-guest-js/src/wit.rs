//! Component Model intrinsics and the host imports the JS guest uses.
//!
//! Same imports and marshalling as the CPython guest (`eryx-wasm-runtime`),
//! minus tracing and networking, which the spike doesn't expose to JS.

use std::cell::Cell;

use wit_dylib_ffi::Wit;

use crate::call::{EryxCall, Value};

#[link(wasm_import_module = "$root")]
unsafe extern "C" {
    #[link_name = "[waitable-set-new]"]
    pub(crate) fn waitable_set_new() -> u32;
    #[link_name = "[waitable-set-drop]"]
    pub(crate) fn waitable_set_drop(set: u32);
    #[link_name = "[waitable-join]"]
    pub(crate) fn waitable_join(waitable: u32, set: u32);
    #[link_name = "[context-set-0]"]
    pub(crate) fn context_set(ptr: u32);
    #[link_name = "[context-get-0]"]
    pub(crate) fn context_get() -> u32;
    #[link_name = "[subtask-drop]"]
    pub(crate) fn subtask_drop(task: u32);
}

thread_local! {
    static WIT: Cell<Option<Wit>> = const { Cell::new(None) };
}

pub(crate) fn set_wit(wit: Wit) {
    WIT.with(|cell| cell.set(Some(wit)));
}

fn wit() -> Wit {
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

fn pop_invoke_result(cx: &mut EryxCall) -> Result<String, String> {
    match (cx.stack.pop(), cx.stack.pop()) {
        (Some(Value::ResultDiscriminant(true)), Some(Value::String(json))) => Ok(json),
        (Some(Value::ResultDiscriminant(false)), Some(Value::String(error))) => Err(error),
        other => Err(format!("malformed invoke result: {other:?}")),
    }
}
