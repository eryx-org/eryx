# Skipped Tests

This document tracks tests that are currently skipped and what needs to be done to get them passing.

## Summary

| Category | Tests Skipped | Status |
|----------|---------------|--------|
| Session State | 0 | ✅ Fixed |
| Link Test | 0 | ✅ Fixed |
| Tracing | 0 | ✅ Implemented |
| **Total** | **0** | |

All previously skipped tests are now passing!

---

## ~~Session State Tests (4 tests)~~ ✅ FIXED

These tests are now passing after fixes in:
- `crates/eryx/src/session/executor.rs` - `reset()` now properly mounts Python stdlib
- `crates/eryx-wasm-runtime/src/python.rs` - `clear_state()` and `snapshot_state()` now preserve internal runtime infrastructure (`_DummySocket`, etc.)

---

## ~~Link Test (1 test)~~ ✅ FIXED

The `test_link_runtime` test now includes all required libraries (libpython, libc++, etc.) and passes.

---

## ~~Tracing Tests (24 tests)~~ ✅ IMPLEMENTED

Tracing is now implemented in `eryx-wasm-runtime`. The implementation:

1. Added `call_report_trace()` in `lib.rs` to call the WIT `report-trace` import
2. Added callback infrastructure in `python.rs` (`ReportTraceCallback`, `do_report_trace()`)
3. Added `_eryx_report_trace` function to the `_eryx` Python module
4. Set up `sys.settrace()` in the execute wrapper to capture line/call/return/exception events
5. Added callback_start/callback_end tracing in the `invoke()` function

All 24 tracing tests now pass (13 in `trace_events_precise.rs`, 11 in `trace_output_handlers.rs`).
