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

/// Our call context - holds a stack for passing values between wit-dylib and our code.
#[derive(Debug)]
pub struct EryxCall {
    /// Stack of values being passed.
    /// For simplicity, we use a Vec<Value> where Value is an enum of possible types.
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
        // This is tricky - we need to return a reference to a string on the stack
        // For now, leak the string (we'll fix this properly later)
        match self.stack.pop() {
            Some(Value::String(s)) => {
                let leaked: &'static str = Box::leak(s.into_boxed_str());
                leaked
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

impl Interpreter for EryxInterpreter {
    type CallCx<'a> = EryxCall;

    fn initialize(_wit: Wit) {
        eprintln!("eryx-wasm-runtime: initialize called");
        python::initialize_python();
    }

    fn export_start<'a>(_wit: Wit, func: ExportFunction) -> Box<Self::CallCx<'a>> {
        eprintln!("eryx-wasm-runtime: export_start for func {}", func.index());
        Box::new(EryxCall::new())
    }

    fn export_call(_wit: Wit, func: ExportFunction, cx: &mut Self::CallCx<'_>) {
        eprintln!("eryx-wasm-runtime: export_call for func {}", func.index());

        match func.index() {
            EXPORT_EXECUTE => {
                // execute(code: string) -> result<string, string>
                let code = cx.pop_string().to_string();
                eprintln!("eryx-wasm-runtime: execute called with code: {code}");

                match python::execute_python(&code) {
                    Ok(output) => {
                        cx.push_string(output);
                        cx.stack.push(Value::ResultDiscriminant(true)); // is_ok = true
                    }
                    Err(error) => {
                        cx.push_string(error);
                        cx.stack.push(Value::ResultDiscriminant(false)); // is_ok = false
                    }
                }
            }
            EXPORT_SNAPSHOT_STATE => {
                // snapshot-state() -> result<list<u8>, string>
                eprintln!("eryx-wasm-runtime: snapshot_state called");

                // TODO: Actually pickle Python globals
                let state = vec![0u8; 0]; // Empty state for now

                cx.stack.push(Value::Bytes(state));
                cx.stack.push(Value::ResultDiscriminant(true));
            }
            EXPORT_RESTORE_STATE => {
                // restore-state(data: list<u8>) -> result<_, string>
                eprintln!("eryx-wasm-runtime: restore_state called");

                // TODO: Actually restore Python globals
                cx.stack.push(Value::ResultDiscriminant(true));
            }
            EXPORT_CLEAR_STATE => {
                // clear-state()
                eprintln!("eryx-wasm-runtime: clear_state called");

                // TODO: Actually clear Python globals
            }
            _ => {
                panic!("unknown export function index: {}", func.index());
            }
        }
    }

    fn export_async_start(
        _wit: Wit,
        func: ExportFunction,
        mut cx: Box<Self::CallCx<'static>>,
    ) -> u32 {
        eprintln!(
            "eryx-wasm-runtime: export_async_start for func {}",
            func.index()
        );

        // For now, handle async exports synchronously
        // TODO: Implement proper async support with CPython
        match func.index() {
            EXPORT_EXECUTE => {
                let code = cx.pop_string().to_string();
                eprintln!("eryx-wasm-runtime: async execute called with code: {code}");

                match python::execute_python(&code) {
                    Ok(output) => {
                        cx.push_string(output);
                        cx.stack.push(Value::ResultDiscriminant(true));
                    }
                    Err(error) => {
                        cx.push_string(error);
                        cx.stack.push(Value::ResultDiscriminant(false));
                    }
                }
            }
            EXPORT_SNAPSHOT_STATE => {
                eprintln!("eryx-wasm-runtime: async snapshot_state called");
                let state = vec![0u8; 0];
                cx.stack.push(Value::Bytes(state));
                cx.stack.push(Value::ResultDiscriminant(true));
            }
            EXPORT_RESTORE_STATE => {
                eprintln!("eryx-wasm-runtime: async restore_state called");
                cx.stack.push(Value::ResultDiscriminant(true));
            }
            EXPORT_CLEAR_STATE => {
                eprintln!("eryx-wasm-runtime: async clear_state called");
            }
            _ => {
                panic!("unknown export function index: {}", func.index());
            }
        }

        // Call task_return to signal completion and return the result
        // This is required for async exports to properly return their values
        if let Some(task_return) = func.task_return() {
            eprintln!("eryx-wasm-runtime: calling task_return");
            unsafe {
                let cx_ptr: *mut EryxCall = Box::into_raw(cx);
                task_return(cx_ptr.cast());
                // Note: task_return takes ownership, cx is now invalid
            }
        } else {
            eprintln!("eryx-wasm-runtime: no task_return function available");
            // Drop cx normally if no task_return
            drop(cx);
        }

        // Return 0 to indicate synchronous completion (no pending async work)
        0
    }

    fn export_async_callback(_event0: u32, _event1: u32, _event2: u32) -> u32 {
        eprintln!("eryx-wasm-runtime: export_async_callback called");
        // TODO: Handle async callbacks properly
        0
    }

    fn resource_dtor(_ty: Resource, _handle: usize) {
        eprintln!("eryx-wasm-runtime: resource_dtor called");
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
