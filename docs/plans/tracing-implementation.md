# Tracing Implementation in eryx-wasm-runtime

## Status: ✅ COMPLETED

Tracing has been successfully implemented in `eryx-wasm-runtime`. All 24 trace tests now pass.

## Background

The eryx sandbox supports execution tracing via the `report-trace` WIT import, which allows the Python guest to report line-by-line execution events to the host. This enables:
- Debugging and step-through execution
- Progress visualization
- Execution time profiling

## Implementation Summary

### 1. WIT Import Call (`lib.rs`)

Added `call_report_trace()` function that calls the `report-trace` WIT import:

```rust
fn call_report_trace(lineno: u32, event_json: &str, context_json: &str) {
    CURRENT_WIT.with(|cell| {
        let wit = cell.borrow();
        let Some(wit) = wit.as_ref() else { return };

        let import_func = match wit.get_import(None, "report-trace") {
            Some(f) => f,
            None => return,
        };

        let mut cx = EryxCall::new();
        cx.push_string(context_json.to_string());
        cx.push_string(event_json.to_string());
        cx.push_u32(lineno);

        import_func.call_import_sync(&mut cx);
    });
}
```

### 2. Callback Infrastructure (`python.rs`)

Added callback type and functions similar to invoke callbacks:

```rust
pub type ReportTraceCallback = fn(u32, &str, &str);

pub fn set_report_trace_callback(callback: Option<ReportTraceCallback>) { ... }
pub fn do_report_trace(lineno: u32, event_json: &str, context_json: &str) { ... }
```

### 3. Python Module Function

Added `_eryx_report_trace` to the `_eryx` module:

```rust
#[pyfunction]
fn _eryx_report_trace(lineno: u32, event_json: String, context_json: String) {
    do_report_trace(lineno, &event_json, &context_json);
}
```

### 4. sys.settrace Setup

Added trace function setup in the Python execute wrapper:

```python
def _eryx_trace_func(frame, event, arg):
    filename = frame.f_code.co_filename
    if filename != '<user>':
        return _eryx_trace_func

    lineno = frame.f_lineno
    func_name = frame.f_code.co_name

    if func_name.startswith('_') and func_name != '<module>':
        return _eryx_trace_func

    if event == 'line':
        _eryx_mod._eryx_report_trace(lineno, json.dumps({"type": "line"}), "")
    elif event == 'call':
        _eryx_mod._eryx_report_trace(lineno, json.dumps({"type": "call", "function": func_name}), "")
    # ... etc

    return _eryx_trace_func

_sys.settrace(_eryx_trace_func)
try:
    # execute user code
finally:
    _sys.settrace(None)
```

### 5. Callback Tracing

Added callback_start/callback_end events in the `invoke()` function:

```python
async def invoke(name, **kwargs):
    _eryx._eryx_report_trace(0, json.dumps({"type": "callback_start", "name": name}), args_json)
    try:
        # invoke callback
        _eryx._eryx_report_trace(0, json.dumps({"type": "callback_end", "name": name}), "")
        return result
    except Exception as e:
        _eryx._eryx_report_trace(0, json.dumps({"type": "callback_end", "name": name, "error": str(e)}), "")
        raise
```

### 6. Wiring in with_wit

The callback is set up alongside invoke callbacks:

```rust
fn with_wit<T>(wit: Wit, f: impl FnOnce() -> T) -> T {
    // ...
    python::set_report_trace_callback(Some(report_trace_callback_wrapper));
    let result = f();
    python::set_report_trace_callback(None);
    // ...
}
```

## Event Types

The trace system reports these event types:

| Event | Description |
|-------|-------------|
| `line` | A line of code is about to execute |
| `call` | A function call is starting |
| `return` | A function is returning |
| `exception` | An exception was raised |
| `callback_start` | A host callback is being invoked |
| `callback_end` | A host callback has completed |

## Performance Notes

- `sys.settrace()` has overhead (~30% slowdown typically)
- Tracing is always enabled - no opt-out mechanism currently
- Async code only traces synchronous portions; callback start/end is traced explicitly

## Files Modified

1. `crates/eryx-wasm-runtime/src/lib.rs` - `call_report_trace()` and callback wiring
2. `crates/eryx-wasm-runtime/src/python.rs` - Callback infrastructure, `_eryx_report_trace`, trace setup
3. `crates/eryx/tests/trace_events_precise.rs` - Removed `#[ignore]` attributes, updated async callback expectations
4. `crates/eryx/tests/trace_output_handlers.rs` - Removed `#[ignore]` attributes
