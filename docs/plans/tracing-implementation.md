# Plan: Implement Tracing in eryx-wasm-runtime

## Background

The eryx sandbox supports execution tracing via the `report-trace` WIT import, which allows the Python guest to report line-by-line execution events to the host. This enables:
- Debugging and step-through execution
- Progress visualization
- Execution time profiling

The old componentize-py based runtime (`runtime.py`) implemented this via `sys.settrace()`. When we moved to the new `eryx-wasm-runtime` architecture, tracing was not ported.

## Current State

- **WIT interface**: `report-trace: func(lineno: u32, event-json: string, context-json: string)` is defined
- **Host side**: `wasm.rs:283` implements `report_trace()` which sends events to the trace channel
- **Guest side**: `eryx-wasm-runtime` does NOT call `report-trace` or set up `sys.settrace()`

## Implementation Plan

### Phase 1: Add report-trace WIT import call

**File**: `crates/eryx-wasm-runtime/src/lib.rs`

Add a function similar to `call_invoke` that calls the `report-trace` import:

```rust
/// Call report-trace import to send a trace event to the host.
fn call_report_trace(lineno: u32, event_json: &str, context_json: &str) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else { return };

        let import_func = match wit.get_import(None, "report-trace") {
            Some(f) => f,
            None => return, // Tracing not available
        };

        let mut cx = EryxCall::new();
        cx.push_u32(lineno);
        cx.push_string(event_json);
        cx.push_string(context_json);

        // Call is synchronous (no async), result is unit type
        let _ = import_func.call(&mut cx);
    });
}
```

### Phase 2: Expose to Python via C extension

**File**: `crates/eryx-wasm-runtime/src/python.rs`

Add a `report_trace` callback similar to the invoke callback:

```rust
// Thread-local callback for report_trace
static REPORT_TRACE_CALLBACK: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

pub type ReportTraceCallback = fn(u32, &str, &str);

pub fn set_report_trace_callback(cb: Option<ReportTraceCallback>) {
    let ptr = cb.map(|f| f as *mut ()).unwrap_or(std::ptr::null_mut());
    REPORT_TRACE_CALLBACK.store(ptr, Ordering::SeqCst);
}

/// Called from Python to report a trace event
pub fn do_report_trace(lineno: u32, event_json: &str, context_json: &str) {
    let ptr = REPORT_TRACE_CALLBACK.load(Ordering::SeqCst);
    if !ptr.is_null() {
        let cb: ReportTraceCallback = unsafe { std::mem::transmute(ptr) };
        cb(lineno, event_json, context_json);
    }
}
```

Then expose via PyMethodDef in the `_eryx` module:

```rust
// In ERYX_METHODS
PyMethodDef {
    ml_name: c"_eryx_report_trace".as_ptr(),
    ml_meth: Some(py_report_trace),
    ml_flags: METH_VARARGS,
    ml_doc: std::ptr::null(),
},
```

### Phase 3: Set up sys.settrace in Python wrapper

**File**: `crates/eryx-wasm-runtime/src/python.rs`

In the `execute()` wrapper code, add trace setup similar to the old runtime.py:

```python
import sys as _sys
import json as _json

def _eryx_trace_func(frame, event, arg):
    filename = frame.f_code.co_filename

    # Only trace user code (compiled as '<user>')
    if filename != '<user>':
        return _eryx_trace_func

    lineno = frame.f_lineno
    func_name = frame.f_code.co_name

    # Skip internal functions
    if func_name.startswith('_') and func_name != '<module>':
        return _eryx_trace_func

    if event == 'line':
        _eryx._eryx_report_trace(lineno, _json.dumps({"type": "line"}), "")
    elif event == 'call':
        _eryx._eryx_report_trace(lineno, _json.dumps({"type": "call", "function": func_name}), "")
    elif event == 'return':
        _eryx._eryx_report_trace(lineno, _json.dumps({"type": "return", "function": func_name}), "")
    elif event == 'exception':
        exc_type, exc_value, _ = arg
        if exc_type is not StopIteration:
            _eryx._eryx_report_trace(lineno, _json.dumps({
                "type": "exception",
                "exception_type": exc_type.__name__ if exc_type else "Unknown",
                "message": str(exc_value) if exc_value else ""
            }), "")

    return _eryx_trace_func

_sys.settrace(_eryx_trace_func)
try:
    # ... execute user code ...
finally:
    _sys.settrace(None)
```

### Phase 4: Wire up callback in with_wit

**File**: `crates/eryx-wasm-runtime/src/lib.rs`

In `with_wit()`, set up the report_trace callback alongside invoke:

```rust
fn with_wit<T>(wit: Wit, f: impl FnOnce() -> T) -> T {
    CURRENT_WIT.with(|cell| {
        let old = cell.borrow_mut().replace(wit);

        python::set_invoke_callback(Some(invoke_callback_wrapper));
        python::set_invoke_async_callback(Some(invoke_async_callback_wrapper));
        python::set_report_trace_callback(Some(report_trace_wrapper));  // NEW

        let result = f();

        python::set_invoke_callback(None);
        python::set_invoke_async_callback(None);
        python::set_report_trace_callback(None);  // NEW

        *cell.borrow_mut() = old;
        result
    })
}

fn report_trace_wrapper(lineno: u32, event_json: &str, context_json: &str) {
    call_report_trace(lineno, event_json, context_json);
}
```

## Testing

After implementation:
1. Remove the `#[ignore]` attributes from trace tests
2. Run `mise run test-all` to verify all trace tests pass
3. Test manually with `cargo run --example trace_events`

## Considerations

### Performance
- `sys.settrace()` has overhead (~30% slowdown typically)
- Consider making tracing opt-in via a flag passed to execute()
- Could add a WIT parameter: `execute(code: string, enable-trace: bool)`

### Async code
- `sys.settrace()` doesn't work well with async/await
- The old runtime only traced synchronous portions
- Callback start/end should be traced explicitly (already done in invoke wrapper)

## Files to modify

1. `crates/eryx-wasm-runtime/src/lib.rs` - Add `call_report_trace()` and wire up callback
2. `crates/eryx-wasm-runtime/src/python.rs` - Add C extension function and trace setup in exec wrapper
3. `crates/eryx/tests/*.rs` - Remove `#[ignore]` from trace tests
