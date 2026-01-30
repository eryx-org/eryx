//! Eryx WASM Guest Runtime
//!
//! This crate implements the wit-dylib interpreter interface for the eryx sandbox.
//! It replaces `libcomponentize_py_runtime.so` with a purpose-built runtime that
//! hardcodes the eryx sandbox exports (execute, snapshot-state, restore-state, clear-state).
//!
//! # Architecture
//!
//! wit-dylib generates bindings that call into an "interpreter" via a C FFI interface.
//! This crate implements that interface by:
//!
//! 1. Implementing the `Interpreter` trait from `wit-dylib-ffi`
//! 2. Using the `export!` macro to generate the required `#[no_mangle]` exports
//! 3. Dispatching export calls to hardcoded implementations
//!
//! # Exports
//!
//! - `execute(code: string) -> result<string, string>` - Run Python code
//! - `snapshot-state() -> result<list<u8>, string>` - Pickle globals
//! - `restore-state(data: list<u8>) -> result<_, string>` - Restore globals
//! - `clear-state()` - Clear globals

#![allow(unsafe_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

pub mod python;

use std::alloc::Layout;
use wit_dylib_ffi::{
    Call, Enum, ExportFunction, Flags, Future, Interpreter, List, Record, Resource, Stream, Tuple,
    Variant, Wit, WitOption, WitResult,
};

// =============================================================================
// Component Model async intrinsics
// =============================================================================
//
// These are imported from the "$root" module and provided by wasmtime's
// Component Model async support. They enable suspend/resume of async operations.

#[link(wasm_import_module = "$root")]
unsafe extern "C" {
    /// Create a new waitable set for tracking pending async operations.
    #[link_name = "[waitable-set-new]"]
    pub fn waitable_set_new() -> u32;

    /// Drop a waitable set when no longer needed.
    #[link_name = "[waitable-set-drop]"]
    pub fn waitable_set_drop(set: u32);

    /// Add a waitable (subtask) to a waitable set for polling.
    #[link_name = "[waitable-join]"]
    pub fn waitable_join(waitable: u32, set: u32);

    /// Store a context value (Python object pointer) for async resumption.
    #[link_name = "[context-set-0]"]
    pub fn context_set(ptr: u32);

    /// Retrieve the stored context value.
    #[link_name = "[context-get-0]"]
    pub fn context_get() -> u32;

    /// Drop a completed subtask to release resources.
    #[link_name = "[subtask-drop]"]
    pub fn subtask_drop(task: u32);
}

/// Our call context - holds a stack for passing values between wit-dylib and our code.
#[derive(Debug)]
pub struct EryxCall {
    /// Stack of values being passed.
    /// For simplicity, we use a `Vec<Value>` where `Value` is an enum of possible types.
    stack: Vec<Value>,
    /// Deferred deallocations
    deferred: Vec<(*mut u8, Layout)>,
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
    /// For result<T, E>: true = ok, false = err
    ResultDiscriminant(bool),
    /// For option<T>: true = some, false = none
    OptionDiscriminant(bool),
}

impl EryxCall {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            deferred: Vec::new(),
        }
    }
}

impl Drop for EryxCall {
    fn drop(&mut self) {
        // Clean up all deferred allocations
        for (ptr, layout) in self.deferred.drain(..) {
            if !ptr.is_null() && layout.size() > 0 {
                // Safety: ptr and layout were created together via Box::into_raw
                unsafe {
                    std::alloc::dealloc(ptr, layout);
                }
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
                // Convert to boxed str and get raw pointer
                let boxed = s.into_boxed_str();
                let ptr = Box::into_raw(boxed);
                // Safety: ptr is valid and points to a str
                let layout = Layout::for_value(unsafe { &*ptr });
                // Track for deallocation when EryxCall is dropped
                self.deferred.push((ptr as *mut u8, layout));
                // Safety: ptr remains valid until EryxCall is dropped
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
            Some(Value::OptionDiscriminant(is_some)) => {
                if is_some {
                    1
                } else {
                    0
                }
            }
            other => panic!("expected OptionDiscriminant, got {other:?}"),
        }
    }

    fn pop_result(&mut self, _ty: WitResult) -> u32 {
        match self.stack.pop() {
            Some(Value::ResultDiscriminant(is_ok)) => {
                if is_ok {
                    0
                } else {
                    1
                }
            }
            other => panic!("expected ResultDiscriminant, got {other:?}"),
        }
    }

    fn pop_variant(&mut self, _ty: Variant) -> u32 {
        self.pop_u32()
    }

    fn pop_record(&mut self, _ty: Record) {
        // Records are flattened - fields are already on the stack
    }

    fn pop_tuple(&mut self, _ty: Tuple) {
        // Tuples are flattened - elements are already on the stack
    }

    fn pop_list(&mut self, _ty: List) -> usize {
        match self.stack.pop() {
            Some(Value::Bytes(bytes)) => {
                let len = bytes.len();
                // Push bytes back for iteration
                for b in bytes.into_iter().rev() {
                    self.stack.push(Value::U8(b));
                }
                len
            }
            other => panic!("expected Bytes for list, got {other:?}"),
        }
    }

    fn pop_iter_next(&mut self, _ty: List) {
        // Called for each element during iteration - element should already be ready
    }

    fn pop_iter(&mut self, _ty: List) {
        // Called when iteration is complete
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

    fn push_record(&mut self, _ty: Record) {
        // Records are flattened - fields will be pushed individually
    }

    fn push_tuple(&mut self, _ty: Tuple) {
        // Tuples are flattened - elements will be pushed individually
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

    fn push_list(&mut self, _ty: List, _capacity: usize) {
        // Start collecting list elements
        self.stack.push(Value::Bytes(Vec::new()));
    }

    fn list_append(&mut self, _ty: List) {
        // Pop the element and append to the list
        let elem = self.stack.pop();
        if let Some(Value::Bytes(bytes)) = self.stack.last_mut()
            && let Some(Value::U8(b)) = elem
        {
            bytes.push(b);
        }
    }
}

/// Our interpreter implementation.
#[derive(Debug)]
pub struct EryxInterpreter;

/// Export function indices (must match the order in runtime.wit)
const EXPORT_EXECUTE: usize = 0;
const EXPORT_SNAPSHOT_STATE: usize = 1;
const EXPORT_RESTORE_STATE: usize = 2;
const EXPORT_CLEAR_STATE: usize = 3;
const EXPORT_FINALIZE_PREINIT: usize = 4;

/// Track whether we've set up callbacks in Python
static CALLBACKS_INITIALIZED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// =============================================================================
// Invoke callback mechanism
// =============================================================================
//
// This allows Python code to call host functions via the invoke() function.
// The mechanism works as follows:
// 1. Python calls invoke(name, **kwargs) which serializes args to JSON
// 2. invoke() calls _eryx._eryx_invoke(name, args_json) (C extension)
// 3. _eryx_invoke calls python::do_invoke() which uses the INVOKE_CALLBACK
// 4. The callback (set by with_wit) calls the WIT invoke import
// 5. Result JSON is returned back through the chain

// Thread-local storage for the current Wit handle during export execution.
std::thread_local! {
    static CURRENT_WIT: std::cell::RefCell<Option<Wit>> = const { std::cell::RefCell::new(None) };
}

/// Execute a function with the Wit handle available for callbacks.
/// Sets up the invoke callbacks so Python can call host functions.
fn with_wit<T>(wit: Wit, f: impl FnOnce() -> T) -> T {
    CURRENT_WIT.with(|cell| {
        let old = cell.borrow_mut().replace(wit);

        // Set up the Python invoke callbacks (both sync and async)
        python::set_invoke_callback(Some(invoke_callback_wrapper));
        python::set_invoke_async_callback(Some(invoke_async_callback_wrapper));
        python::set_report_trace_callback(Some(report_trace_callback_wrapper));

        let result = f();

        // Clear the Python callbacks
        python::set_invoke_callback(None);
        python::set_invoke_async_callback(None);
        python::set_report_trace_callback(None);

        *cell.borrow_mut() = old;
        result
    })
}

/// Wrapper function that matches the InvokeCallback signature (synchronous).
fn invoke_callback_wrapper(name: &str, args_json: &str) -> Result<String, String> {
    call_invoke(name, args_json)
}

/// Wrapper function that matches the InvokeAsyncCallback signature.
fn invoke_async_callback_wrapper(name: &str, args_json: &str) -> Result<InvokeResult, String> {
    call_invoke_async(name, args_json)
}

/// Result of calling an async invoke - either immediate result or pending
#[derive(Debug)]
pub enum InvokeResult {
    /// Immediate success with JSON result
    Ok(String),
    /// Immediate error with error message
    Err(String),
    /// Pending - need to wait for callback. Contains (waitable_id, promise_id)
    Pending(u32, u32),
}

// =============================================================================
// Pending async import tracking
// =============================================================================
//
// When an async import is pending, we need to store state so that when the
// subtask completes, we can lift the result from the buffer.

/// Type of async import, used to determine how to lift the result.
///
/// Note: TCP/TLS operations now use fiber-based async (`call_import_sync` with `func_wrap_async`
/// on the host), so they complete synchronously from the guest's perspective and don't need
/// to be tracked as pending imports. Only `invoke` remains async with Component Model async.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportType {
    /// invoke: result<string, string>
    Invoke,
}

/// Stored state for a pending async import call.
struct PendingImportState {
    /// Type of import (determines result lifting logic)
    import_type: ImportType,
    /// Function to lift the result from buffer onto call stack
    async_lift_impl: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void),
    /// Buffer where the result is stored
    buffer: *mut u8,
    /// Keep the call context alive so the buffer isn't deallocated.
    /// wit-dylib calls `cx.defer_deallocate(buffer, layout)` which means
    /// the buffer is freed when the cx drops. We need to keep it alive.
    _cx: Box<EryxCall>,
}

// Safety: WASM is single-threaded
unsafe impl Send for PendingImportState {}
unsafe impl Sync for PendingImportState {}

// Map of subtask -> pending import state
std::thread_local! {
    static PENDING_IMPORTS: std::cell::RefCell<std::collections::HashMap<u32, PendingImportState>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Call the invoke import with the given callback name and JSON arguments.
/// Returns InvokeResult which can be immediate (Ok/Err) or Pending.
fn call_invoke_async(name: &str, args_json: &str) -> Result<InvokeResult, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "invoke() called outside of execute context".to_string())?;

        // Get the invoke import (async in WIT, but wasmtime 40+ uses plain name)
        let import_func = wit
            .get_import(None, "invoke")
            .ok_or_else(|| "invoke import not found".to_string())?;

        // Create a boxed call context so we can store it if the call is pending.
        // wit-dylib registers the buffer for deferred deallocation on the cx,
        // so we must keep the cx alive until we've lifted the result.
        let mut cx = Box::new(EryxCall::new());

        // Push arguments onto stack - wit-dylib pops in reverse order,
        // so push args_json first, then name
        cx.push_string(args_json.to_string());
        cx.push_string(name.to_string());

        // Call the async import
        // Safety: we're in a valid execution context
        let pending = unsafe { import_func.call_import_async(&mut *cx) };

        if let Some(pending_call) = pending {
            // The operation is pending - we need to wait for a callback
            // Store the lift function, buffer, and cx so we can get the result when it completes.
            // Keeping cx alive prevents the buffer from being deallocated.
            let async_lift_impl = import_func.async_import_lift_impl().unwrap();
            PENDING_IMPORTS.with(|cell| {
                cell.borrow_mut().insert(
                    pending_call.subtask,
                    PendingImportState {
                        import_type: ImportType::Invoke,
                        async_lift_impl,
                        buffer: pending_call.buffer,
                        _cx: cx,
                    },
                );
            });
            // The subtask is the waitable, and we use 0 as the promise placeholder
            return Ok(InvokeResult::Pending(pending_call.subtask, 0));
        }

        // Result is on the stack: result<string, string>
        // Pop the discriminant first (true = ok, false = err)
        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => {
                return Err(format!("unexpected result discriminant: {other:?}"));
            }
        };

        // Pop the string value
        let value = match cx.stack.pop() {
            Some(Value::String(s)) => s,
            other => {
                return Err(format!("unexpected result value: {other:?}"));
            }
        };

        if is_ok {
            Ok(InvokeResult::Ok(value))
        } else {
            Ok(InvokeResult::Err(value))
        }
    })
}

/// Synchronous invoke wrapper for backwards compatibility.
/// Returns error if the operation is pending (requires async).
fn call_invoke(name: &str, args_json: &str) -> Result<String, String> {
    match call_invoke_async(name, args_json) {
        Ok(InvokeResult::Ok(result)) => Ok(result),
        Ok(InvokeResult::Err(error)) => Err(error),
        Ok(InvokeResult::Pending(_, _)) => Err(
            "callback requires async execution which is not supported in synchronous mode"
                .to_string(),
        ),
        Err(e) => Err(e),
    }
}

/// Call list-callbacks import to get available callbacks from the host.
fn call_list_callbacks(wit: Wit) -> Vec<python::CallbackInfo> {
    // Get the list-callbacks import function
    let import_func = match wit.get_import(None, "list-callbacks") {
        Some(f) => f,
        None => {
            return Vec::new();
        }
    };

    // Create a call context to receive the result
    let mut cx = EryxCall::new();

    // Call the import (synchronous)
    import_func.call_import_sync(&mut cx);

    // Parse the stack contents into callback info.
    // The actual format depends on how wasmtime and wit-dylib interact.
    // From observation, the stack contains flattened record fields.
    // We'll collect all string values and try to group them.
    let mut callbacks = Vec::new();
    let mut strings: Vec<String> = Vec::new();

    // Collect all string values from the stack, skipping empty Bytes
    for value in cx.stack.drain(..) {
        match value {
            Value::String(s) => strings.push(s),
            Value::Bytes(b) if !b.is_empty() => {
                // Non-empty bytes might be a string
                if let Ok(s) = String::from_utf8(b) {
                    strings.push(s);
                }
            }
            _ => {} // Skip empty Bytes and other types
        }
    }

    // Group strings into callbacks.
    // Each callback has: name, description, (optional parameters_schema_json)
    // From observation, we get pairs of (name, description) when schema is empty
    if strings.len() >= 2 {
        // Assume pairs of (name, description)
        for chunk in strings.chunks(2) {
            if chunk.len() >= 2 {
                callbacks.push(python::CallbackInfo {
                    name: chunk[0].clone(),
                    description: chunk[1].clone(),
                    parameters_schema_json: String::new(), // Schema not available in this format
                });
            }
        }
    }

    callbacks
}

/// Call report-trace import to send a trace event to the host.
/// This is a synchronous call with no return value.
fn call_report_trace(lineno: u32, event_json: &str, context_json: &str) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else {
            return; // No WIT context - tracing not available
        };

        // Get the report-trace import function
        let import_func = match wit.get_import(None, "report-trace") {
            Some(f) => f,
            None => return, // Tracing not available
        };

        // Create a call context and push arguments
        let mut cx = EryxCall::new();

        // Push arguments in reverse order (wit-dylib pops in reverse)
        cx.push_string(context_json.to_string());
        cx.push_string(event_json.to_string());
        cx.push_u32(lineno);

        // Call the import (synchronous, no return value)
        import_func.call_import_sync(&mut cx);
    });
}

/// Wrapper function that matches the ReportTraceCallback signature.
fn report_trace_callback_wrapper(lineno: u32, event_json: &str, context_json: &str) {
    call_report_trace(lineno, event_json, context_json);
}

// =============================================================================
// Network import wrappers (TCP and TLS)
// =============================================================================
//
// These functions call the WIT network interface imports.

/// Result of calling an async network operation.
#[derive(Debug)]
pub enum NetResult<T> {
    /// Immediate success with value.
    Ok(T),
    /// Immediate error with error variant discriminant and optional message.
    Err(u32, Option<String>),
    /// Pending - need to wait for callback. Contains (waitable_id, promise_id).
    Pending(u32, u32),
}

// -----------------------------------------------------------------------------
// TCP operations
// -----------------------------------------------------------------------------

/// Call the TCP connect import (synchronous - blocks until complete).
fn call_tcp_connect(host: &str, port: u16) -> Result<NetResult<u32>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TCP connect called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some("eryx:net/tcp@0.1.0"), "connect")
            .ok_or_else(|| "tcp.connect import not found - networking not enabled".to_string())?;

        let mut cx = EryxCall::new();

        // Push arguments: port (u16), then host (string) - reverse order for wit-dylib
        cx.push_u16(port);
        cx.push_string(host.to_string());

        // Synchronous call - blocks until the host completes the operation
        import_func.call_import_sync(&mut cx);

        // Result: result<tcp-handle, tcp-error>
        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };

        if is_ok {
            let handle = match cx.stack.pop() {
                Some(Value::U32(h)) => h,
                other => return Err(format!("unexpected handle value: {other:?}")),
            };
            Ok(NetResult::Ok(handle))
        } else {
            let discr = match cx.stack.pop() {
                Some(Value::U32(d)) => d,
                other => return Err(format!("unexpected error discriminant: {other:?}")),
            };
            let payload = cx.stack.pop().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(NetResult::Err(discr, payload))
        }
    })
}

/// Call the TCP read import (synchronous - blocks until complete).
fn call_tcp_read(handle: u32, len: u32) -> Result<NetResult<Vec<u8>>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TCP read called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some("eryx:net/tcp@0.1.0"), "read")
            .ok_or_else(|| "tcp.read import not found - networking not enabled".to_string())?;

        let mut cx = EryxCall::new();

        cx.push_u32(len);
        cx.push_u32(handle);

        // Synchronous call - blocks until the host completes the operation
        import_func.call_import_sync(&mut cx);

        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };

        if is_ok {
            let bytes = match cx.stack.pop() {
                Some(Value::Bytes(b)) => b,
                other => return Err(format!("unexpected bytes value: {other:?}")),
            };
            Ok(NetResult::Ok(bytes))
        } else {
            let discr = match cx.stack.pop() {
                Some(Value::U32(d)) => d,
                other => return Err(format!("unexpected error discriminant: {other:?}")),
            };
            let payload = cx.stack.pop().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(NetResult::Err(discr, payload))
        }
    })
}

/// Call the TCP write import (synchronous - blocks until complete).
fn call_tcp_write(handle: u32, data: &[u8]) -> Result<NetResult<u32>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TCP write called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some("eryx:net/tcp@0.1.0"), "write")
            .ok_or_else(|| "tcp.write import not found - networking not enabled".to_string())?;

        let mut cx = EryxCall::new();

        cx.stack.push(Value::Bytes(data.to_vec()));
        cx.push_u32(handle);

        // Synchronous call - blocks until the host completes the operation
        import_func.call_import_sync(&mut cx);

        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };

        if is_ok {
            let written = match cx.stack.pop() {
                Some(Value::U32(n)) => n,
                other => return Err(format!("unexpected written value: {other:?}")),
            };
            Ok(NetResult::Ok(written))
        } else {
            let discr = match cx.stack.pop() {
                Some(Value::U32(d)) => d,
                other => return Err(format!("unexpected error discriminant: {other:?}")),
            };
            let payload = cx.stack.pop().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(NetResult::Err(discr, payload))
        }
    })
}

/// Call the TCP close import (synchronous).
fn call_tcp_close(handle: u32) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else {
            return;
        };

        let import_func = match wit.get_import(Some("eryx:net/tcp@0.1.0"), "close") {
            Some(f) => f,
            None => return,
        };

        let mut cx = EryxCall::new();
        cx.push_u32(handle);
        import_func.call_import_sync(&mut cx);
    });
}

/// TCP error discriminant to error name mapping.
fn tcp_error_name(discr: u32) -> &'static str {
    match discr {
        0 => "connection-refused",
        1 => "connection-reset",
        2 => "timed-out",
        3 => "host-not-found",
        4 => "io-error",
        5 => "not-permitted",
        6 => "invalid-handle",
        _ => "unknown-error",
    }
}

// -----------------------------------------------------------------------------
// TLS operations
// -----------------------------------------------------------------------------

/// Call the TLS upgrade import (synchronous - blocks until complete).
fn call_tls_upgrade(tcp_handle: u32, hostname: &str) -> Result<NetResult<u32>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TLS upgrade called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some("eryx:net/tls@0.1.0"), "upgrade")
            .ok_or_else(|| "tls.upgrade import not found - networking not enabled".to_string())?;

        let mut cx = EryxCall::new();

        // Push arguments: hostname (string), then tcp_handle (u32) - reverse order
        cx.push_string(hostname.to_string());
        cx.push_u32(tcp_handle);

        // Synchronous call - blocks until the host completes the operation
        import_func.call_import_sync(&mut cx);

        // Result: result<tls-handle, tls-error>
        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };

        if is_ok {
            let handle = match cx.stack.pop() {
                Some(Value::U32(h)) => h,
                other => return Err(format!("unexpected handle value: {other:?}")),
            };
            Ok(NetResult::Ok(handle))
        } else {
            // TLS error is a variant - first discriminant tells us which
            let discr = match cx.stack.pop() {
                Some(Value::U32(d)) => d,
                other => return Err(format!("unexpected error discriminant: {other:?}")),
            };
            let payload = cx.stack.pop().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(NetResult::Err(discr, payload))
        }
    })
}

/// Call the TLS read import (synchronous - blocks until complete).
fn call_tls_read(handle: u32, len: u32) -> Result<NetResult<Vec<u8>>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TLS read called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some("eryx:net/tls@0.1.0"), "read")
            .ok_or_else(|| "tls.read import not found - networking not enabled".to_string())?;

        let mut cx = EryxCall::new();

        cx.push_u32(len);
        cx.push_u32(handle);

        // Synchronous call - blocks until the host completes the operation
        import_func.call_import_sync(&mut cx);

        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };

        if is_ok {
            let bytes = match cx.stack.pop() {
                Some(Value::Bytes(b)) => b,
                other => return Err(format!("unexpected bytes value: {other:?}")),
            };
            Ok(NetResult::Ok(bytes))
        } else {
            let discr = match cx.stack.pop() {
                Some(Value::U32(d)) => d,
                other => return Err(format!("unexpected error discriminant: {other:?}")),
            };
            let payload = cx.stack.pop().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(NetResult::Err(discr, payload))
        }
    })
}

/// Call the TLS write import (synchronous - blocks until complete).
fn call_tls_write(handle: u32, data: &[u8]) -> Result<NetResult<u32>, String> {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let wit = wit
            .as_ref()
            .ok_or_else(|| "TLS write called outside of execute context".to_string())?;

        let import_func = wit
            .get_import(Some("eryx:net/tls@0.1.0"), "write")
            .ok_or_else(|| "tls.write import not found - networking not enabled".to_string())?;

        let mut cx = EryxCall::new();

        cx.stack.push(Value::Bytes(data.to_vec()));
        cx.push_u32(handle);

        // Synchronous call - blocks until the host completes the operation
        import_func.call_import_sync(&mut cx);

        let is_ok = match cx.stack.pop() {
            Some(Value::ResultDiscriminant(v)) => v,
            other => return Err(format!("unexpected result discriminant: {other:?}")),
        };

        if is_ok {
            let written = match cx.stack.pop() {
                Some(Value::U32(n)) => n,
                other => return Err(format!("unexpected written value: {other:?}")),
            };
            Ok(NetResult::Ok(written))
        } else {
            let discr = match cx.stack.pop() {
                Some(Value::U32(d)) => d,
                other => return Err(format!("unexpected error discriminant: {other:?}")),
            };
            let payload = cx.stack.pop().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            });
            Ok(NetResult::Err(discr, payload))
        }
    })
}

/// Call the TLS close import (synchronous).
fn call_tls_close(handle: u32) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else {
            return;
        };

        let import_func = match wit.get_import(Some("eryx:net/tls@0.1.0"), "close") {
            Some(f) => f,
            None => return,
        };

        let mut cx = EryxCall::new();
        cx.push_u32(handle);
        import_func.call_import_sync(&mut cx);
    });
}

/// TLS error discriminant to error name mapping.
/// Note: TLS errors have different structure - 0 is tcp(tcp-error), others are TLS-specific.
fn tls_error_name(discr: u32) -> &'static str {
    match discr {
        0 => "tcp-error", // Wraps a tcp-error
        1 => "handshake-failed",
        2 => "certificate-error",
        3 => "invalid-handle",
        _ => "unknown-error",
    }
}

// Public network API used by python.rs pyfunction implementations

/// Value type for network results returned to Python.
#[derive(Debug)]
pub enum NetResultValue {
    /// Handle or bytes written count (u32).
    Handle(u32),
    /// Bytes read.
    Bytes(Vec<u8>),
    /// Error message.
    Error(String),
    /// Pending operation (waitable_id, promise_id).
    Pending(u32, u32),
}

/// Alias for backwards compatibility with existing code.
pub type TlsResultValue = NetResultValue;

// -----------------------------------------------------------------------------
// Public TCP API
// -----------------------------------------------------------------------------

/// Connect to a host:port over TCP.
/// Returns Ok((status, value)) where:
/// - status 0: success, value is handle (u32)
/// - status 1: error, value is error message (String)
/// - status 2: pending, value is (waitable_id, promise_id)
pub fn do_tcp_connect(host: &str, port: u16) -> Result<(i32, NetResultValue), String> {
    match call_tcp_connect(host, port)? {
        NetResult::Ok(handle) => Ok((0, NetResultValue::Handle(handle))),
        NetResult::Err(discr, payload) => {
            let error_name = tcp_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Read from a TCP connection.
///
/// Returns `Ok((status, value))` where:
/// - status 0: success, value is bytes (`Vec<u8>`)
/// - status 1: error, value is error message (`String`)
/// - status 2: pending, value is (waitable_id, promise_id) (`(u32, u32)`)
pub fn do_tcp_read(handle: u32, len: u32) -> Result<(i32, NetResultValue), String> {
    match call_tcp_read(handle, len)? {
        NetResult::Ok(bytes) => Ok((0, NetResultValue::Bytes(bytes))),
        NetResult::Err(discr, payload) => {
            let error_name = tcp_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Write to a TCP connection.
/// Returns Ok((status, value)) where:
/// - status 0: success, value is bytes written (u32)
/// - status 1: error, value is error message (String)
/// - status 2: pending, value is (waitable_id, promise_id)
pub fn do_tcp_write(handle: u32, data: &[u8]) -> Result<(i32, NetResultValue), String> {
    match call_tcp_write(handle, data)? {
        NetResult::Ok(written) => Ok((0, NetResultValue::Handle(written))),
        NetResult::Err(discr, payload) => {
            let error_name = tcp_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Close a TCP connection (synchronous, no return value).
pub fn do_tcp_close(handle: u32) {
    call_tcp_close(handle);
}

// -----------------------------------------------------------------------------
// Public TLS API
// -----------------------------------------------------------------------------

/// Upgrade a TCP connection to TLS.
/// Returns Ok((status, value)) where:
/// - status 0: success, value is TLS handle (u32)
/// - status 1: error, value is error message (String)
/// - status 2: pending, value is (waitable_id, promise_id)
pub fn do_tls_upgrade(tcp_handle: u32, hostname: &str) -> Result<(i32, NetResultValue), String> {
    match call_tls_upgrade(tcp_handle, hostname)? {
        NetResult::Ok(handle) => Ok((0, NetResultValue::Handle(handle))),
        NetResult::Err(discr, payload) => {
            let error_name = tls_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Read from a TLS connection.
///
/// Returns Ok((status, value)) where:
/// - status 0: success, value is bytes (`Vec<u8>`)
/// - status 1: error, value is error message (`String`)
/// - status 2: pending, value is (waitable_id, promise_id) (`(u32, u32)`)
pub fn do_tls_read(handle: u32, len: u32) -> Result<(i32, NetResultValue), String> {
    match call_tls_read(handle, len)? {
        NetResult::Ok(bytes) => Ok((0, NetResultValue::Bytes(bytes))),
        NetResult::Err(discr, payload) => {
            let error_name = tls_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Write to a TLS connection.
/// Returns Ok((status, value)) where:
/// - status 0: success, value is bytes written (u32)
/// - status 1: error, value is error message (String)
/// - status 2: pending, value is (waitable_id, promise_id)
pub fn do_tls_write(handle: u32, data: &[u8]) -> Result<(i32, NetResultValue), String> {
    match call_tls_write(handle, data)? {
        NetResult::Ok(written) => Ok((0, NetResultValue::Handle(written))),
        NetResult::Err(discr, payload) => {
            let error_name = tls_error_name(discr);
            let msg = match payload {
                Some(p) => format!("{error_name}: {p}"),
                None => error_name.to_string(),
            };
            Ok((1, NetResultValue::Error(msg)))
        }
        NetResult::Pending(waitable, promise) => {
            Ok((2, NetResultValue::Pending(waitable, promise)))
        }
    }
}

/// Close a TLS connection (synchronous, no return value).
pub fn do_tls_close(handle: u32) {
    call_tls_close(handle);
}

/// Initialize callbacks in Python if not already done.
fn ensure_callbacks_initialized(wit: Wit) {
    use std::sync::atomic::Ordering;

    if CALLBACKS_INITIALIZED.swap(true, Ordering::SeqCst) {
        return; // Already initialized
    }

    let callbacks = call_list_callbacks(wit);

    if let Err(_e) = python::setup_callbacks(&callbacks) {
        // Callback setup failed - continue without callbacks
    }
}

// =============================================================================
// Async state storage
// =============================================================================
//
// When an async export is pending (waiting for a callback), we store the call
// context and task_return function so we can resume and complete when the
// callback finishes.

/// Type for the task_return callback function.
type TaskReturnFn = unsafe extern "C" fn(*mut std::ffi::c_void);

/// Stored state for a pending async export.
struct PendingAsyncState {
    cx: Box<EryxCall>,
    task_return: Option<TaskReturnFn>,
    /// The Wit handle needed for subsequent callbacks.
    /// We need to restore this when resuming async execution.
    wit: Wit,
}

// Thread-local storage for pending async state.
// WASM is single-threaded, so only one export can be pending at a time.
std::thread_local! {
    static PENDING_ASYNC_STATE: std::cell::RefCell<Option<PendingAsyncState>> =
        const { std::cell::RefCell::new(None) };
}

/// Result of handling an export call.
enum HandleExportResult {
    /// Export completed - result is on the stack.
    Complete,
    /// Export is pending - waiting for async callback.
    /// Contains the raw callback code (WAIT | waitable_set << 4) to return to the host.
    Pending(u32),
}

/// Handle an export call by dispatching to the appropriate Python function.
/// Returns whether the export completed or is pending.
fn handle_export(wit: Wit, func_index: usize, cx: &mut EryxCall) -> HandleExportResult {
    match func_index {
        EXPORT_EXECUTE => {
            // execute(code: string) -> result<string, string>
            let code = cx.pop_string().to_string();

            // Ensure callbacks are set up before first execute
            ensure_callbacks_initialized(wit);

            // Execute Python with Wit handle available for callbacks
            let result = with_wit(wit, || python::execute_python(&code));

            match result {
                python::ExecuteResult::Complete(output) => {
                    // Push record fields in REVERSE order for LIFO stack
                    // WIT defines: execute-output { stdout, stderr }
                    // wit-dylib pops fields in definition order, so push stderr first, then stdout
                    cx.push_string(output.stderr);
                    cx.push_string(output.stdout);
                    cx.stack.push(Value::ResultDiscriminant(true));
                    HandleExportResult::Complete
                }
                python::ExecuteResult::Error(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                    HandleExportResult::Complete
                }
                python::ExecuteResult::Pending(callback_code) => {
                    // Execution is suspended, waiting for async callback
                    HandleExportResult::Pending(callback_code)
                }
            }
        }
        EXPORT_SNAPSHOT_STATE => {
            // snapshot-state() -> result<list<u8>, string>
            match python::snapshot_state() {
                Ok(state) => {
                    cx.stack.push(Value::Bytes(state));
                    cx.stack.push(Value::ResultDiscriminant(true));
                }
                Err(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                }
            }
            HandleExportResult::Complete
        }
        EXPORT_RESTORE_STATE => {
            // restore-state(data: list<u8>) -> result<_, string>
            let data = match cx.stack.pop() {
                Some(Value::Bytes(bytes)) => bytes,
                other => panic!("expected Bytes for restore_state data, got {other:?}"),
            };

            match python::restore_state(&data) {
                Ok(()) => {
                    cx.stack.push(Value::ResultDiscriminant(true));
                }
                Err(error) => {
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                }
            }
            HandleExportResult::Complete
        }
        EXPORT_CLEAR_STATE => {
            // clear-state() - no return value
            python::clear_state();
            HandleExportResult::Complete
        }
        EXPORT_FINALIZE_PREINIT => {
            // finalize-preinit() - no return value
            // Reset WASI state after all imports are done during pre-init
            python::finalize_preinit();
            HandleExportResult::Complete
        }
        _ => {
            panic!("unknown export function index: {}", func_index);
        }
    }
}

impl Interpreter for EryxInterpreter {
    type CallCx<'a> = EryxCall;

    fn initialize(_wit: Wit) {
        python::initialize_python();
    }

    fn export_start<'a>(_wit: Wit, _func: ExportFunction) -> Box<Self::CallCx<'a>> {
        Box::new(EryxCall::new())
    }

    fn export_call(wit: Wit, func: ExportFunction, cx: &mut Self::CallCx<'_>) {
        // Synchronous exports don't support pending - must complete
        let result = handle_export(wit, func.index(), cx);
        if let HandleExportResult::Pending(_) = result {
            panic!("synchronous export cannot return pending");
        }
    }

    fn export_async_start(
        wit: Wit,
        func: ExportFunction,
        mut cx: Box<Self::CallCx<'static>>,
    ) -> u32 {
        let result = handle_export(wit, func.index(), &mut cx);

        match result {
            HandleExportResult::Complete => {
                // Call task_return to signal async completion.
                if let Some(task_return) = func.task_return() {
                    unsafe {
                        task_return(Box::into_raw(cx).cast());
                    }
                }
                0
            }
            HandleExportResult::Pending(callback_code) => {
                // Store state for later resumption when callback completes
                PENDING_ASYNC_STATE.with(|cell| {
                    *cell.borrow_mut() = Some(PendingAsyncState {
                        cx,
                        task_return: func.task_return(),
                        wit,
                    });
                });
                // Return the callback code (WAIT | waitable_set << 4) to the host
                callback_code
            }
        }
    }

    fn export_async_callback(event0: u32, event1: u32, event2: u32) -> u32 {
        // Event types from Component Model (used by _eryx_async)
        const EVENT_SUBTASK: u32 = 1;
        // Status types
        const STATUS_RETURNED: u32 = 2;

        // Get the Wit handle from pending state (without taking ownership yet).
        // We need this to restore CURRENT_WIT and callbacks so subsequent invoke() calls work.
        let wit = PENDING_ASYNC_STATE.with(|cell| cell.borrow().as_ref().map(|s| s.wit));
        let wit = match wit {
            Some(w) => w,
            None => return 0, // No pending state - shouldn't happen
        };

        // If this is a SUBTASK event with RETURNED status, lift the async import result
        // before calling Python's callback so promise_get_result can access it
        if event0 == EVENT_SUBTASK && event2 == STATUS_RETURNED {
            let subtask = event1;
            if let Some(pending_state) =
                PENDING_IMPORTS.with(|cell| cell.borrow_mut().remove(&subtask))
            {
                // Create a call context to receive the lifted result
                let mut cx = EryxCall::new();

                // Lift the result from the buffer onto the call stack
                // Safety: async_lift_impl and buffer were set by call_import_async
                unsafe {
                    (pending_state.async_lift_impl)(
                        (&raw mut cx).cast(),
                        pending_state.buffer.cast(),
                    );
                }

                // Handle result based on import type.
                // Note: Only `invoke` uses Component Model async. TCP/TLS operations
                // now use fiber-based async and complete synchronously from the guest's
                // perspective, so they don't appear as pending imports.
                match pending_state.import_type {
                    ImportType::Invoke => {
                        // result<string, string>
                        let is_ok = match cx.stack.pop() {
                            Some(Value::ResultDiscriminant(v)) => v,
                            _ => true,
                        };
                        let result_value = match cx.stack.pop() {
                            Some(Value::String(s)) => s,
                            _ => String::new(),
                        };
                        let result_json = if is_ok {
                            format!(r#"{{"ok": true, "value": {}}}"#, result_value)
                        } else {
                            // Error message needs to be JSON-encoded (with quotes and escaping)
                            let escaped = python::escape_json_string(&result_value);
                            format!(r#"{{"ok": false, "error": "{}"}}"#, escaped)
                        };
                        python::set_async_import_result(subtask, &result_json);
                    }
                }
            }
        }

        // Call Python's callback function to resume execution.
        // Restore CURRENT_WIT and callbacks so any subsequent invoke() calls work.
        let callback_code = with_wit(wit, || python::call_python_callback(event0, event1, event2));
        let code = python::callback_code::get_code(callback_code);

        if code == python::callback_code::WAIT {
            // Still pending - return the full callback code (WAIT | waitable_set << 4)
            return callback_code;
        }

        // Execution complete (EXIT) - capture output and call task_return
        PENDING_ASYNC_STATE.with(|cell| {
            if let Some(state) = cell.borrow_mut().take() {
                let mut cx = state.cx;

                // Re-capture stdout now that the async execution is complete
                // The original teardown ran before we suspended, so it captured nothing
                python::recapture_stdout();

                // Check if there was an uncaught Python exception during the callback
                if let Some(error) = python::take_last_callback_error() {
                    // Push error result
                    cx.push_string(error);
                    cx.stack.push(Value::ResultDiscriminant(false));
                } else {
                    // Get the output from Python (both stdout and stderr)
                    let stdout = unsafe {
                        python::get_python_variable_string("_eryx_output").unwrap_or_default()
                    };
                    let stderr = unsafe {
                        python::get_python_variable_string("_eryx_errors").unwrap_or_default()
                    };

                    // Push record fields in REVERSE order for LIFO stack (stderr first, then stdout)
                    cx.push_string(stderr.trim_end_matches('\n').to_string());
                    cx.push_string(stdout.trim_end_matches('\n').to_string());
                    cx.stack.push(Value::ResultDiscriminant(true));
                }

                // Call task_return
                if let Some(task_return) = state.task_return {
                    unsafe {
                        task_return(Box::into_raw(cx).cast());
                    }
                }
            }
        });

        0
    }

    fn resource_dtor(_ty: Resource, _handle: usize) {
        // No resources to clean up for now
    }
}

// Export the interpreter interface
mod export {
    #![allow(missing_docs)]
    #![allow(clippy::not_unsafe_ptr_arg_deref)]
    use super::EryxInterpreter;
    wit_dylib_ffi::export!(EryxInterpreter);
}
