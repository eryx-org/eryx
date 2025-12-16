# Compatibility shim for componentize-py's runtime interface.
#
# This module provides the interface expected by componentize_py_async_support
# from componentize-py but routes calls through eryx's _eryx native module.

import _eryx
from componentize_py_types import Err, Ok


# Re-export intrinsic functions with expected names
def waitable_set_new():
    """Create a new waitable set for tracking pending operations."""
    return _eryx.waitable_set_new_()


def waitable_set_drop(set_handle):
    """Drop a waitable set when no longer needed."""
    _eryx.waitable_set_drop_(set_handle)


def waitable_join(waitable, set_handle):
    """Add a waitable to a waitable set for polling."""
    _eryx.waitable_join_(waitable, set_handle)


def context_set(value):
    """Store a context value for async resumption."""
    _eryx.context_set_(value)


def context_get():
    """Retrieve the stored context value."""
    return _eryx.context_get_()


def subtask_drop(task):
    """Drop a completed subtask to release resources."""
    _eryx.subtask_drop_(task)


def invoke_async(name, args_json):
    """Async-aware invoke that returns Ok/Err or pending tuple.

    Returns:
        - Ok(result_json) if completed immediately with success
        - Err(error_json) if completed immediately with error
        - Err((waitable_id, promise_id)) if pending (for async handling)
    """
    result_type, value = _eryx._eryx_invoke_async(name, args_json)
    if result_type == 0:  # Ok
        return Ok(value)
    elif result_type == 1:  # Err
        return Err(value)
    else:  # Pending
        return Err(value)  # value is (waitable, promise) tuple


# Placeholder implementations for functions we don't support yet
def call_task_return(export_index, borrows, result):
    """Signal that an async export has completed.

    Note: This is handled internally by wit-dylib in our implementation.
    """
    # In our implementation, task_return is called from Rust in export_async_start
    pass


def promise_get_result(future_result, promise):
    """Get result from a completed promise.

    Reads the actual result from the _eryx_async_import_result global
    which was set by the Rust layer when the async import completed.

    Args:
        future_result: The status code from the callback (event2)
        promise: The promise ID (currently unused)

    Returns:
        The actual result JSON string from the async import
    """
    import json

    import __main__

    # Read the result that was stored by Rust in export_async_callback
    # It's stored in __main__ since that's where PyRun_SimpleString executes
    result_json = getattr(__main__, "_eryx_async_import_result", None)
    if result_json is None:
        # No result stored, this shouldn't happen
        raise RuntimeError("No async import result available")

    # Parse the result JSON and return the appropriate value
    result = json.loads(result_json)
    if result.get("ok", False):
        # Return the value as a JSON string (invoke() will parse it)
        value = result.get("value", "")
        if isinstance(value, str):
            return value
        else:
            return json.dumps(value)
    else:
        raise RuntimeError(result.get("error", "Unknown error"))


# Future read/write functions - placeholders for now
def future_read(type_, handle):
    """Read from a future."""
    raise NotImplementedError("future_read not yet implemented")


def future_write(type_, handle, value):
    """Write to a future."""
    raise NotImplementedError("future_write not yet implemented")


def future_drop_readable(type_, handle):
    """Drop a readable future handle."""
    pass  # No-op for now


def future_drop_writable(type_, handle):
    """Drop a writable future handle."""
    pass  # No-op for now
