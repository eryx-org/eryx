//! Eryx WASM Guest Runtime — QuickJS edition (SPIKE)
//!
//! This crate is a *prototype* showing what a second guest language (JavaScript,
//! via QuickJS) looks like under eryx's `wit-dylib` interpreter interface. It is
//! the structural twin of `crates/eryx-wasm-runtime` (the CPython guest): same WIT
//! contract, same `Interpreter` trait, same export-dispatch shape — only the
//! embedded engine differs (QuickJS through `rquickjs` instead of CPython through
//! PyO3).
//!
//! # What this proves
//!
//! The `Interpreter`/`Call` FFI surface is genuinely language-agnostic. The Python
//! guest's `EryxCall` stack machine, `with_wit` callback scoping, and export
//! routing port over essentially verbatim. The interesting work is all in
//! `quickjs.rs` (the engine wrapper, the analog of `python.rs`).
//!
//! # What this does NOT do
//!
//! - It is NOT compiled to a WASM component. The real build needs QuickJS compiled
//!   with the WASI-SDK clang toolchain and linked via `wit-dylib`, which is out of
//!   scope for the spike. We type-check against the native target instead.
//! - The Component Model *async* machinery (waitable sets, subtasks, `task_return`)
//!   is reproduced structurally but the async `invoke` suspend/resume bridge is
//!   stubbed — JS has no asyncio, so the real port drives Promises directly off the
//!   host job queue (see `quickjs::resume_async`).
//!
//! # Exports (identical to the Python guest's `runtime.wit`)
//!
//! - `execute(code: string) -> result<execute-output, string>`
//! - `snapshot-state() -> result<list<u8>, string>`
//! - `restore-state(data: list<u8>) -> result<_, string>`
//! - `clear-state()`
//! - `finalize-preinit()`
//! - `set-result-variable(name: string)`

#![allow(unsafe_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

pub mod quickjs;

use std::alloc::Layout;
use wit_dylib_ffi::{
    Call, Enum, ExportFunction, Flags, Future, Interpreter, List, Record, Resource, Stream, Tuple,
    Type, Variant, Wit, WitOption, WitResult,
};

// =============================================================================
// Call stack machine
// =============================================================================
//
// SPIKE: This is a near-verbatim copy of the Python guest's `EryxCall` + `Value`.
// It is interpreter-agnostic — it only marshals WIT values between wit-dylib and
// our code — so a real refactor would lift it into a shared crate used by both the
// Python and JS guests rather than duplicating it. Kept inline here so the spike is
// self-contained and reads as a single drop-in twin of `eryx-wasm-runtime`.

/// Our call context — a stack for passing values between wit-dylib and our code.
#[derive(Debug)]
pub struct EryxCall {
    stack: Vec<Value>,
    deferred: Vec<(*mut u8, Layout)>,
    iterators: Vec<std::vec::IntoIter<Value>>,
}

/// A value on the call stack.
#[derive(Debug, Clone)]
enum Value {
    Bool(bool),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    S8(i8),
    S16(i16),
    S32(i32),
    S64(i64),
    F32(f32),
    F64(f64),
    Char(char),
    String(String),
    Bytes(Vec<u8>),
    Record(Vec<Value>),
    GenericList(Vec<Value>),
    Tuple(Vec<Value>),
    ResultDiscriminant(bool),
    OptionDiscriminant(bool),
}

impl EryxCall {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            deferred: Vec::new(),
            iterators: Vec::new(),
        }
    }
}

impl Drop for EryxCall {
    fn drop(&mut self) {
        for (ptr, layout) in self.deferred.drain(..) {
            if !ptr.is_null() && layout.size() > 0 {
                // Safety: ptr and layout were created together via Box::into_raw.
                unsafe { std::alloc::dealloc(ptr, layout) }
            }
        }
    }
}

impl Call for EryxCall {
    unsafe fn defer_deallocate(&mut self, ptr: *mut u8, layout: Layout) {
        self.deferred.push((ptr, layout));
    }

    fn pop_bool(&mut self) -> bool {
        match self.stack.pop() {
            Some(Value::Bool(v)) => v,
            other => panic!("expected Bool, got {other:?}"),
        }
    }
    fn pop_u8(&mut self) -> u8 {
        match self.stack.pop() {
            Some(Value::U8(v)) => v,
            other => panic!("expected U8, got {other:?}"),
        }
    }
    fn pop_u16(&mut self) -> u16 {
        match self.stack.pop() {
            Some(Value::U16(v)) => v,
            other => panic!("expected U16, got {other:?}"),
        }
    }
    fn pop_u32(&mut self) -> u32 {
        match self.stack.pop() {
            Some(Value::U32(v)) => v,
            other => panic!("expected U32, got {other:?}"),
        }
    }
    fn pop_u64(&mut self) -> u64 {
        match self.stack.pop() {
            Some(Value::U64(v)) => v,
            other => panic!("expected U64, got {other:?}"),
        }
    }
    fn pop_s8(&mut self) -> i8 {
        match self.stack.pop() {
            Some(Value::S8(v)) => v,
            other => panic!("expected S8, got {other:?}"),
        }
    }
    fn pop_s16(&mut self) -> i16 {
        match self.stack.pop() {
            Some(Value::S16(v)) => v,
            other => panic!("expected S16, got {other:?}"),
        }
    }
    fn pop_s32(&mut self) -> i32 {
        match self.stack.pop() {
            Some(Value::S32(v)) => v,
            other => panic!("expected S32, got {other:?}"),
        }
    }
    fn pop_s64(&mut self) -> i64 {
        match self.stack.pop() {
            Some(Value::S64(v)) => v,
            other => panic!("expected S64, got {other:?}"),
        }
    }
    fn pop_f32(&mut self) -> f32 {
        match self.stack.pop() {
            Some(Value::F32(v)) => v,
            other => panic!("expected F32, got {other:?}"),
        }
    }
    fn pop_f64(&mut self) -> f64 {
        match self.stack.pop() {
            Some(Value::F64(v)) => v,
            other => panic!("expected F64, got {other:?}"),
        }
    }
    fn pop_char(&mut self) -> char {
        match self.stack.pop() {
            Some(Value::Char(v)) => v,
            other => panic!("expected Char, got {other:?}"),
        }
    }

    fn pop_string(&mut self) -> &str {
        match self.stack.pop() {
            Some(Value::String(s)) => {
                let boxed = s.into_boxed_str();
                let ptr = Box::into_raw(boxed);
                // Safety: ptr is valid and points to a str.
                let layout = Layout::for_value(unsafe { &*ptr });
                self.deferred.push((ptr as *mut u8, layout));
                // Safety: ptr remains valid until EryxCall is dropped.
                unsafe { &*ptr }
            }
            other => panic!("expected String, got {other:?}"),
        }
    }

    fn pop_borrow(&mut self, _ty: Resource) -> u32 {
        self.pop_u32()
    }
    fn pop_own(&mut self, _ty: Resource) -> u32 {
        self.pop_u32()
    }
    fn pop_enum(&mut self, _ty: Enum) -> u32 {
        self.pop_u32()
    }
    fn pop_flags(&mut self, _ty: Flags) -> u32 {
        self.pop_u32()
    }
    fn pop_future(&mut self, _ty: Future) -> u32 {
        self.pop_u32()
    }
    fn pop_stream(&mut self, _ty: Stream) -> u32 {
        self.pop_u32()
    }

    fn pop_option(&mut self, _ty: WitOption) -> u32 {
        match self.stack.pop() {
            Some(Value::OptionDiscriminant(is_some)) => u32::from(is_some),
            other => panic!("expected OptionDiscriminant, got {other:?}"),
        }
    }
    fn pop_result(&mut self, _ty: WitResult) -> u32 {
        match self.stack.pop() {
            Some(Value::ResultDiscriminant(is_ok)) => u32::from(!is_ok),
            other => panic!("expected ResultDiscriminant, got {other:?}"),
        }
    }
    fn pop_variant(&mut self, _ty: Variant) -> u32 {
        self.pop_u32()
    }

    fn pop_record(&mut self, _ty: Record) {
        match self.stack.pop() {
            Some(Value::Record(fields)) => {
                for field in fields.into_iter().rev() {
                    self.stack.push(field);
                }
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    fn pop_tuple(&mut self, _ty: Tuple) {
        match self.stack.pop() {
            Some(Value::Tuple(elements)) => {
                for elem in elements.into_iter().rev() {
                    self.stack.push(elem);
                }
            }
            other => panic!("expected Tuple, got {other:?}"),
        }
    }

    unsafe fn maybe_pop_list(&mut self, ty: List) -> Option<(*const u8, usize)> {
        if matches!(ty.ty(), Type::U8)
            && let Some(Value::Bytes(bytes)) = self.stack.last()
        {
            return Some((bytes.as_ptr(), bytes.len()));
        }
        None
    }

    fn pop_list(&mut self, _ty: List) -> usize {
        match self.stack.pop() {
            Some(Value::Bytes(bytes)) => {
                let len = bytes.len();
                self.iterators.push(
                    bytes
                        .into_iter()
                        .map(Value::U8)
                        .collect::<Vec<_>>()
                        .into_iter(),
                );
                len
            }
            Some(Value::GenericList(items)) => {
                let len = items.len();
                self.iterators.push(items.into_iter());
                len
            }
            other => panic!("expected Bytes or GenericList for list, got {other:?}"),
        }
    }

    fn pop_iter_next(&mut self, _ty: List) {
        if let Some(iter) = self.iterators.last_mut()
            && let Some(value) = iter.next()
        {
            self.stack.push(value);
        }
    }

    fn pop_iter(&mut self, _ty: List) {
        self.iterators.pop();
    }

    fn push_bool(&mut self, val: bool) {
        self.stack.push(Value::Bool(val));
    }
    fn push_char(&mut self, val: char) {
        self.stack.push(Value::Char(val));
    }
    fn push_u8(&mut self, val: u8) {
        self.stack.push(Value::U8(val));
    }
    fn push_s8(&mut self, val: i8) {
        self.stack.push(Value::S8(val));
    }
    fn push_u16(&mut self, val: u16) {
        self.stack.push(Value::U16(val));
    }
    fn push_s16(&mut self, val: i16) {
        self.stack.push(Value::S16(val));
    }
    fn push_u32(&mut self, val: u32) {
        self.stack.push(Value::U32(val));
    }
    fn push_s32(&mut self, val: i32) {
        self.stack.push(Value::S32(val));
    }
    fn push_u64(&mut self, val: u64) {
        self.stack.push(Value::U64(val));
    }
    fn push_s64(&mut self, val: i64) {
        self.stack.push(Value::S64(val));
    }
    fn push_f32(&mut self, val: f32) {
        self.stack.push(Value::F32(val));
    }
    fn push_f64(&mut self, val: f64) {
        self.stack.push(Value::F64(val));
    }
    fn push_string(&mut self, val: String) {
        self.stack.push(Value::String(val));
    }

    fn push_record(&mut self, ty: Record) {
        let n = ty.fields().len();
        let start = self.stack.len() - n;
        let fields = self.stack.drain(start..).collect();
        self.stack.push(Value::Record(fields));
    }

    fn push_tuple(&mut self, ty: Tuple) {
        let n = ty.types().len();
        let start = self.stack.len() - n;
        let elements = self.stack.drain(start..).collect();
        self.stack.push(Value::Tuple(elements));
    }

    fn push_flags(&mut self, _ty: Flags, bits: u32) {
        self.stack.push(Value::U32(bits));
    }
    fn push_enum(&mut self, _ty: Enum, discr: u32) {
        self.stack.push(Value::U32(discr));
    }
    fn push_borrow(&mut self, _ty: Resource, handle: u32) {
        self.stack.push(Value::U32(handle));
    }
    fn push_own(&mut self, _ty: Resource, handle: u32) {
        self.stack.push(Value::U32(handle));
    }
    fn push_future(&mut self, _ty: Future, handle: u32) {
        self.stack.push(Value::U32(handle));
    }
    fn push_stream(&mut self, _ty: Stream, handle: u32) {
        self.stack.push(Value::U32(handle));
    }
    fn push_variant(&mut self, _ty: Variant, discr: u32) {
        self.stack.push(Value::U32(discr));
    }
    fn push_option(&mut self, _ty: WitOption, is_some: bool) {
        self.stack.push(Value::OptionDiscriminant(is_some));
    }
    fn push_result(&mut self, _ty: WitResult, is_err: bool) {
        self.stack.push(Value::ResultDiscriminant(!is_err));
    }

    unsafe fn push_raw_list(&mut self, ty: List, ptr: *mut u8, len: usize) -> bool {
        if matches!(ty.ty(), Type::U8) {
            // Safety: ptr/len come from wit-dylib's owned byte buffer.
            let bytes = unsafe { Vec::from_raw_parts(ptr, len, len) };
            self.stack.push(Value::Bytes(bytes));
            return true;
        }
        false
    }

    fn push_list(&mut self, ty: List, _capacity: usize) {
        if matches!(ty.ty(), Type::U8) {
            self.stack.push(Value::Bytes(Vec::new()));
        } else {
            self.stack.push(Value::GenericList(Vec::new()));
        }
    }

    fn list_append(&mut self, _ty: List) {
        let elem = self.stack.pop().expect("list_append: missing element");
        match self.stack.last_mut() {
            Some(Value::Bytes(bytes)) => {
                if let Value::U8(b) = elem {
                    bytes.push(b);
                } else {
                    panic!("list_append: expected U8 element for Bytes list, got {elem:?}");
                }
            }
            Some(Value::GenericList(items)) => items.push(elem),
            other => panic!("list_append: expected Bytes or GenericList at top, got {other:?}"),
        }
    }
}

// =============================================================================
// Export routing
// =============================================================================

/// Export function indices (must match the order in runtime.wit). Identical to
/// the Python guest — the WIT contract is shared.
const EXPORT_EXECUTE: usize = 0;
const EXPORT_SNAPSHOT_STATE: usize = 1;
const EXPORT_RESTORE_STATE: usize = 2;
const EXPORT_CLEAR_STATE: usize = 3;
const EXPORT_FINALIZE_PREINIT: usize = 4;
const EXPORT_SET_RESULT_VARIABLE: usize = 5;

// Thread-local storage for the current Wit handle during export execution, so the
// JS-side intrinsics (console.log, eryx.invoke) can reach the host imports. Direct
// analog of the Python guest's `CURRENT_WIT`.
std::thread_local! {
    static CURRENT_WIT: std::cell::RefCell<Option<Wit>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` with the Wit handle available and the JS callbacks wired up. Mirrors
/// `eryx-wasm-runtime`'s `with_wit`.
fn with_wit<T>(wit: Wit, f: impl FnOnce() -> T) -> T {
    CURRENT_WIT.with(|cell| {
        let old = cell.borrow_mut().replace(wit);

        quickjs::set_report_output_callback(Some(report_output_callback_wrapper));
        quickjs::set_invoke_callback(Some(invoke_callback_wrapper));

        let result = f();

        quickjs::set_report_output_callback(None);
        quickjs::set_invoke_callback(None);

        *cell.borrow_mut() = old;
        result
    })
}

/// Forward a JS `console.*` write to the host `report-output` import.
fn report_output_callback_wrapper(stream: u32, data: &str) {
    call_report_output(stream, data);
}

/// Forward a JS `eryx.invoke` call to the host `invoke` import (synchronous path).
fn invoke_callback_wrapper(name: &str, args_json: &str) -> Result<String, String> {
    call_invoke(name, args_json)
}

/// Call the `report-output` import (synchronous, no return). Same shape as the
/// Python guest's `call_report_output`.
fn call_report_output(stream: u32, data: &str) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else { return };
        let Some(import_func) = wit.get_import(None, "report-output") else {
            return;
        };

        let mut cx = EryxCall::new();
        // Push args in reverse declaration order (wit-dylib pops in reverse).
        cx.push_string(data.to_string());
        cx.push_u32(stream);
        import_func.call_import_sync(&mut cx);
    });
}

/// Call the `invoke` import synchronously and return the JSON result or an error.
///
/// SPIKE: This uses the *synchronous* lowering only. The Python guest also has an
/// async path (`call_invoke_async` -> `InvokeResult::Pending`) hooked into the
/// Component Model async protocol; porting that to JS is the main async TODO (see
/// `quickjs::resume_async`).
fn call_invoke(name: &str, args_json: &str) -> Result<String, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "invoke() called outside of execute context".to_string())?;
        let import_func = wit
            .get_import(None, "invoke")
            .ok_or_else(|| "invoke import not found".to_string())?;

        let mut cx = EryxCall::new();
        cx.push_string(args_json.to_string());
        cx.push_string(name.to_string());
        import_func.call_import_sync(&mut cx);

        // Result: result<string, string>.
        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };
        let value = match cx.stack.pop() {
            Some(Value::String(s)) => s,
            other => return Err(format!("unexpected result value: {other:?}")),
        };
        if is_ok { Ok(value) } else { Err(value) }
    })
}

/// Dispatch an export call. Direct analog of the Python guest's `handle_export`,
/// but synchronous-only: the JS async suspend path is not wired up in the spike, so
/// `execute` always completes (it pumps the job queue to drain Promises).
fn handle_export(wit: Wit, func_index: usize, cx: &mut EryxCall) {
    match func_index {
        EXPORT_EXECUTE => {
            // execute(code: string) -> result<execute-output, string>
            let code = cx.pop_string().to_string();

            // TODO: The Python guest re-syncs host callbacks here via
            // `list-callbacks` + `setup_callbacks` so user code can call them by
            // name. The JS equivalent would build matching `eryx.<name>(...)`
            // wrappers on the global object. Omitted from the spike.

            let result = with_wit(wit, || quickjs::execute_js(&code));

            match result {
                quickjs::ExecuteResult::Complete(output) => {
                    // WIT execute-output { stdout, stderr, result-json, result-error }.
                    cx.stack.push(Value::Record(vec![
                        Value::String(output.stdout),
                        Value::String(output.stderr),
                        Value::String(output.result),
                        Value::String(output.result_error),
                    ]));
                    cx.stack.push(Value::ResultDiscriminant(true));
                }
                quickjs::ExecuteResult::Error(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                }
                quickjs::ExecuteResult::Pending(_callback_code) => {
                    // SPIKE: the async suspend bridge isn't implemented; execute_js
                    // never returns Pending today. A real async export would store
                    // pending state and return the callback code (see the Python
                    // guest's export_async_start). Treat as an error for now.
                    cx.push_string("async suspension not implemented in JS spike".to_string());
                    cx.stack.push(Value::ResultDiscriminant(false));
                }
            }
        }
        EXPORT_SNAPSHOT_STATE => {
            // snapshot-state() -> result<list<u8>, string>
            match quickjs::snapshot_state() {
                Ok(state) => {
                    cx.stack.push(Value::Bytes(state));
                    cx.stack.push(Value::ResultDiscriminant(true));
                }
                Err(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                }
            }
        }
        EXPORT_RESTORE_STATE => {
            // restore-state(data: list<u8>) -> result<_, string>
            let data = match cx.stack.pop() {
                Some(Value::Bytes(bytes)) => bytes,
                other => panic!("expected Bytes for restore_state data, got {other:?}"),
            };
            match quickjs::restore_state(&data) {
                Ok(()) => cx.stack.push(Value::ResultDiscriminant(true)),
                Err(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                }
            }
        }
        EXPORT_CLEAR_STATE => quickjs::clear_state(),
        EXPORT_FINALIZE_PREINIT => {
            // finalize-preinit(): reset WASI state before the memory snapshot.
            // TODO: identical concern to the Python guest — the real JS build must
            // also reset the WASI adapter / wasi-libc preopens here so file handles
            // don't get baked into the Wizer snapshot. Stubbed in the spike.
        }
        EXPORT_SET_RESULT_VARIABLE => {
            let name = cx.pop_string().to_string();
            quickjs::set_result_variable_name(&name);
        }
        _ => panic!("unknown export function index: {func_index}"),
    }
}

/// Our interpreter implementation.
#[derive(Debug)]
pub struct EryxJsInterpreter;

impl Interpreter for EryxJsInterpreter {
    type CallCx<'a> = EryxCall;

    fn initialize(_wit: Wit) {
        quickjs::initialize_js();
    }

    fn export_start<'a>(_wit: Wit, _func: ExportFunction) -> Box<Self::CallCx<'a>> {
        Box::new(EryxCall::new())
    }

    fn export_call(wit: Wit, func: ExportFunction, cx: &mut Self::CallCx<'_>) {
        handle_export(wit, func.index(), cx);
    }

    fn export_async_start(
        wit: Wit,
        func: ExportFunction,
        mut cx: Box<Self::CallCx<'static>>,
    ) -> u32 {
        // All exports are declared `async` in the WIT, but the JS guest completes
        // them synchronously (it pumps the job queue inline). So we always run to
        // completion and call `task_return` immediately.
        //
        // SPIKE: A real async `execute` that awaits a host `invoke` would return a
        // WAIT callback code here instead and stash `cx` + `func.task_return()`,
        // exactly as the Python guest does. See module docs and `quickjs::resume_async`.
        handle_export(wit, func.index(), &mut cx);

        if let Some(task_return) = func.task_return() {
            // Safety: hand ownership of the call cx back to the host's task_return.
            unsafe { task_return(Box::into_raw(cx).cast()) }
        }
        0
    }

    fn export_async_callback(event0: u32, event1: u32, event2: u32) -> u32 {
        // SPIKE: No async export ever suspends in the spike, so this is never
        // meaningfully invoked. A real impl resolves the pending JS Promise tied to
        // the completed subtask and re-pumps the job queue.
        quickjs::resume_async(event0, event1, event2)
    }

    fn resource_dtor(_ty: Resource, _handle: usize) {
        // No resources to clean up.
    }
}

// Export the interpreter interface.
mod export {
    #![allow(missing_docs)]
    #![allow(clippy::not_unsafe_ptr_arg_deref)]
    use super::EryxJsInterpreter;
    wit_dylib_ffi::export!(EryxJsInterpreter);
}
