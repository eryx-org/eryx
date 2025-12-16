# Skipped Tests

This document tracks tests that are currently skipped and what needs to be done to get them passing.

## Summary

| Category | Tests Skipped | Tracking |
|----------|---------------|----------|
| Tracing | 24 | [tracing-implementation.md](./tracing-implementation.md) |
| **Total** | **24** | |

---

## ~~Session State Tests (4 tests)~~ ✅ FIXED

These tests are now passing after fixes in:
- `crates/eryx/src/session/executor.rs` - `reset()` now properly mounts Python stdlib
- `crates/eryx-wasm-runtime/src/python.rs` - `clear_state()` and `snapshot_state()` now preserve internal runtime infrastructure (`_DummySocket`, etc.)

---

## ~~Link Test (1 test)~~ ✅ FIXED

The `test_link_runtime` test now includes all required libraries (libpython, libc++, etc.) and passes.

---

## Tracing Tests (24 tests)

See [tracing-implementation.md](./tracing-implementation.md) for the full implementation plan.

### Quick Summary

Tracing is not implemented in `eryx-wasm-runtime`. The old componentize-py runtime had tracing via `sys.settrace()` and `wit_world.report_trace()`, but this was never ported to the new Rust-based runtime.

### Affected Tests

**trace_events_precise.rs** (13 tests):
- `test_trace_simple_assignment`
- `test_trace_multiple_statements`
- `test_trace_function_call`
- `test_trace_two_function_calls`
- `test_trace_callback_invocation`
- `test_trace_multiple_callbacks`
- `test_trace_callback_with_args`
- `test_trace_loop`
- `test_trace_conditional_true_branch`
- `test_trace_conditional_false_branch`
- `test_trace_function_multiple_statements`
- `test_trace_print`
- `test_trace_events_in_result`

**trace_output_handlers.rs** (11 tests):
- `test_trace_handler_receives_line_events`
- `test_trace_handler_receives_call_and_return_events`
- `test_trace_handler_receives_callback_events`
- `test_trace_handler_callback_duration_tracked`
- `test_trace_events_in_result`
- `test_trace_handler_exception_event`
- `test_both_handlers_together`
- `test_handlers_with_error`
- `test_sandbox_reuse_with_handlers`
- `test_trace_event_line_numbers`
- `test_trace_events_order`

### Fix Required

Implement tracing in `eryx-wasm-runtime` as described in [tracing-implementation.md](./tracing-implementation.md).
