//! The wit-dylib value stack.
//!
//! Copied verbatim from `eryx-wasm-runtime` (the CPython guest). It only
//! marshals WIT values and knows nothing about the embedded language, so a
//! production JS guest should lift it into a crate shared by both guests.

use std::alloc::Layout;
use wit_dylib_ffi::{
    Call, Enum, Flags, Future, List, Record, Resource, Stream, Tuple, Type, Variant, WitOption,
    WitResult,
};

/// Our call context - holds a stack for passing values between wit-dylib and our code.
#[derive(Debug)]
pub struct EryxCall {
    /// Stack of values being passed.
    /// For simplicity, we use a `Vec<Value>` where `Value` is an enum of possible types.
    pub(crate) stack: Vec<Value>,
    /// Deferred deallocations
    deferred: Vec<(*mut u8, Layout)>,
    /// Iterators for list iteration via pop_list/pop_list_iter_next/pop_list_iter.
    iterators: Vec<std::vec::IntoIter<Value>>,
}

/// A value on the call stack.
#[derive(Debug, Clone)]
pub(crate) enum Value {
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
    /// A record with its fields collected in definition order.
    Record(Vec<Value>),
    /// A generic list (non-u8) with collected elements.
    GenericList(Vec<Value>),
    /// A tuple with its elements collected in definition order.
    Tuple(Vec<Value>),
    /// For result<T, E>: true = ok, false = err
    ResultDiscriminant(bool),
    /// For option<T>: true = some, false = none
    OptionDiscriminant(bool),
}

impl EryxCall {
    pub(crate) fn new() -> Self {
        Self {
            stack: Vec::new(),
            deferred: Vec::new(),
            iterators: Vec::new(),
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
        // Pop a Record value and push its fields back onto the stack in reverse order
        // so they can be popped in definition order (LIFO).
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
        // Pop a Tuple value and push its elements back onto the stack in reverse order
        // so they can be popped in definition order (LIFO).
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
        // For byte lists, pop the value and return a raw pointer for zero-copy transfer.
        // The Vec is moved into a deferred allocation so the pointer stays valid until
        // the EryxCall is dropped — same pattern as pop_string.
        if matches!(ty.ty(), Type::U8)
            && matches!(self.stack.last(), Some(Value::Bytes(_)))
            && let Some(Value::Bytes(bytes)) = self.stack.pop()
        {
            let len = bytes.len();
            let boxed = bytes.into_boxed_slice();
            let raw = Box::into_raw(boxed);
            let ptr = unsafe { (*raw).as_ptr() };
            let layout = Layout::for_value(unsafe { &*raw });
            self.deferred.push((raw as *mut u8, layout));
            return Some((ptr, len));
        }
        None
    }

    fn pop_list(&mut self, _ty: List) -> usize {
        match self.stack.pop() {
            Some(Value::Bytes(bytes)) => {
                let len = bytes.len();
                // Push bytes as an iterator for pop_list_iter_next
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
                // Push items as an iterator for pop_list_iter_next
                self.iterators.push(items.into_iter());
                len
            }
            other => panic!("expected Bytes or GenericList for list, got {other:?}"),
        }
    }

    fn pop_list_iter_next(&mut self, _ty: List) {
        // Pop next item from the current iterator and push it onto the stack.
        if let Some(iter) = self.iterators.last_mut()
            && let Some(value) = iter.next()
        {
            self.stack.push(value);
        }
    }

    fn pop_list_iter(&mut self, _ty: List) {
        // Iteration complete - remove the iterator.
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
        // Collect the top N values from the stack (the record's fields) into a Record.
        let n = ty.fields().len();
        let start = self.stack.len() - n;
        let fields = self.stack.drain(start..).collect();
        self.stack.push(Value::Record(fields));
    }

    fn push_tuple(&mut self, ty: Tuple) {
        // Collect the top N values from the stack (the tuple's elements) into a Tuple.
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
        // For byte lists, take ownership and push as Bytes.
        if matches!(ty.ty(), Type::U8) {
            let bytes = unsafe { Vec::from_raw_parts(ptr, len, len) };
            self.stack.push(Value::Bytes(bytes));
            return true;
        }
        false
    }

    fn push_list(&mut self, ty: List, _capacity: usize) {
        // Start collecting list elements.
        // Use Bytes for list<u8>, GenericList for everything else.
        if matches!(ty.ty(), Type::U8) {
            self.stack.push(Value::Bytes(Vec::new()));
        } else {
            self.stack.push(Value::GenericList(Vec::new()));
        }
    }

    fn list_append(&mut self, _ty: List) {
        // Pop the element and append to the list being built.
        let elem = self.stack.pop().expect("list_append: missing element");
        match self.stack.last_mut() {
            Some(Value::Bytes(bytes)) => {
                if let Value::U8(b) = elem {
                    bytes.push(b);
                } else {
                    panic!("list_append: expected U8 element for Bytes list, got {elem:?}");
                }
            }
            Some(Value::GenericList(items)) => {
                items.push(elem);
            }
            other => {
                panic!("list_append: expected Bytes or GenericList at stack top, got {other:?}")
            }
        }
    }
}
