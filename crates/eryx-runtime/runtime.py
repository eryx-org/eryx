"""Eryx Python WASM runtime with async callback support and state persistence.

This module provides the WIT world implementation for executing Python code
in a WebAssembly sandbox with access to async callbacks via the host.

State Persistence:
    The runtime maintains a `_persistent_globals` dict that preserves user-defined
    variables between execute() calls. This enables REPL-style interactive sessions
    where variables, functions, and classes persist across executions.

    State can be serialized via snapshot_state() and restored via restore_state(),
    enabling state transfer between processes or persistence to storage.
"""

import ast
import asyncio
import json
import math
import pickle
import re
import sys
import traceback
import types
from collections import Counter, defaultdict
from datetime import datetime, timedelta
from io import StringIO
from itertools import chain, groupby

import wit_world
from componentize_py_types import Err

# Module-level persistent state storage.
# This dict holds user-defined variables between execute() calls.
_persistent_globals: dict = {}

# Maximum snapshot size (10 MB) to prevent abuse
MAX_SNAPSHOT_SIZE = 10 * 1024 * 1024

# Keys that should never be persisted (internal/security-sensitive)
_EXCLUDED_KEYS = frozenset(
    {
        "__builtins__",
        "invoke",
        "list_callbacks",
        "asyncio",
        "json",
        "math",
        "re",
        "defaultdict",
        "Counter",
        "datetime",
        "timedelta",
        "groupby",
        "chain",
        "os",
        "subprocess",
        "socket",
        "__import__",
        # Internal Python keys
        "__name__",
        "__doc__",
        "__package__",
        "__loader__",
        "__spec__",
        "__annotations__",
        "__builtins__",
        "__file__",
        "__cached__",
    }
)


def _get_base_globals() -> dict:
    """Get the base execution globals with pre-imported modules and blocked items."""
    return {
        "__builtins__": __builtins__,
        # Async support
        "asyncio": asyncio,
        # Common utilities
        "json": json,
        "math": math,
        "re": re,
        "defaultdict": defaultdict,
        "Counter": Counter,
        "datetime": datetime,
        "timedelta": timedelta,
        "groupby": groupby,
        "chain": chain,
        # Blocked for security
        "os": None,
        "subprocess": None,
        "socket": None,
        "__import__": None,  # Prevent dynamic imports
    }


def _get_exec_globals(invoke_func, list_callbacks_func) -> dict:
    """Get the execution globals including persistent state and API functions.

    Args:
        invoke_func: The async invoke function for callbacks
        list_callbacks_func: The list_callbacks introspection function

    Returns:
        A dict suitable for use as globals in exec()
    """
    globals_dict = _get_base_globals()

    # Add Eryx API functions
    globals_dict["invoke"] = invoke_func
    globals_dict["list_callbacks"] = list_callbacks_func

    # Merge in persistent user state
    globals_dict.update(_persistent_globals)

    return globals_dict


def _extract_user_globals(exec_globals: dict) -> dict:
    """Extract user-defined globals from execution globals.

    Filters out base modules, API functions, and internal keys to get
    only the user-defined variables, functions, and classes.

    Args:
        exec_globals: The globals dict after code execution

    Returns:
        A dict containing only user-defined items
    """
    user_globals = {}
    base_keys = (
        set(_get_base_globals().keys()) | _EXCLUDED_KEYS | {"invoke", "list_callbacks"}
    )

    for key, value in exec_globals.items():
        # Skip excluded keys
        if key in base_keys:
            continue
        # Skip private/dunder names
        if key.startswith("_"):
            continue
        # Skip None values (blocked modules)
        if value is None:
            continue
        # Skip module objects (we don't want to persist imported modules)
        if isinstance(value, types.ModuleType):
            continue

        user_globals[key] = value

    return user_globals


def _is_picklable(obj) -> bool:
    """Check if an object can be pickled.

    Args:
        obj: The object to check

    Returns:
        True if the object can be pickled, False otherwise
    """
    try:
        pickle.dumps(obj)
        return True
    except (pickle.PicklingError, TypeError, AttributeError):
        return False


def _filter_picklable(globals_dict: dict) -> tuple[dict, list[str]]:
    """Filter a globals dict to only picklable items.

    Args:
        globals_dict: Dict of global variables

    Returns:
        Tuple of (picklable_dict, list of skipped key names)
    """
    picklable = {}
    skipped = []

    for key, value in globals_dict.items():
        if _is_picklable(value):
            picklable[key] = value
        else:
            skipped.append(key)

    return picklable, skipped


class WitWorld(wit_world.WitWorld):
    async def execute(self, code: str) -> str:
        """Execute Python code with top-level await support and state persistence.

        Variables, functions, and classes defined in one execute() call will
        be available in subsequent calls. For example:

            execute("x = 1")
            execute("print(x)")  # prints "1"

        Supports direct top-level await syntax:
            result = await invoke("get_time", "{}")
            print(result)

        Also supports parallel callback execution:
            results = await asyncio.gather(
                invoke("query", '{"q": "a"}'),
                invoke("query", '{"q": "b"}'),
            )

        Returns:
            Captured stdout. Use print() to output the result.
        """
        global _persistent_globals

        old_stdout = sys.stdout
        old_stderr = sys.stderr

        try:
            # Capture stdout
            sys.stdout = StringIO()
            sys.stderr = StringIO()

            async def invoke(name: str, arguments_json: str = "{}") -> any:
                """Invoke a callback asynchronously via the host.

                Args:
                    name: Name of the callback to invoke (e.g., "http.get", "get_time")
                    arguments_json: JSON-encoded arguments object

                Returns:
                    Parsed JSON result as dict/list/primitive.

                Example:
                    result = await invoke("get_time", "{}")
                    data = await invoke("http.get", '{"url": "https://example.com"}')
                """
                # Report callback start trace event
                wit_world.report_trace(
                    0,
                    json.dumps({"type": "callback_start", "name": name}),
                    arguments_json,
                )

                try:
                    result = await wit_world.invoke(name, arguments_json)
                    parsed = json.loads(result)

                    # Report callback end trace event
                    wit_world.report_trace(
                        0,
                        json.dumps({"type": "callback_end", "name": name}),
                        "",
                    )

                    return parsed
                except Exception as e:
                    # Report callback error
                    wit_world.report_trace(
                        0,
                        json.dumps(
                            {"type": "callback_end", "name": name, "error": str(e)}
                        ),
                        "",
                    )
                    raise

            def list_callbacks() -> list:
                """List all available callbacks for introspection.

                Returns:
                    List of callback info dicts with 'name', 'description',
                    and 'parameters_schema' keys.

                Example:
                    callbacks = list_callbacks()
                    for cb in callbacks:
                        print(f"{cb['name']}: {cb['description']}")
                """
                raw_callbacks = wit_world.list_callbacks()
                return [
                    {
                        "name": cb.name,
                        "description": cb.description,
                        "parameters_schema": json.loads(cb.parameters_schema_json),
                    }
                    for cb in raw_callbacks
                ]

            # Trace function for sys.settrace
            def trace_func(frame, event, arg):
                """Trace function called by Python for each execution event."""
                filename = frame.f_code.co_filename

                # Only trace user code (compiled as "<string>")
                # Skip all internal library code, asyncio internals, etc.
                if filename != "<string>":
                    # Return trace_func to continue tracing (needed to catch
                    # when execution returns to user code), but don't report
                    return trace_func

                lineno = frame.f_lineno
                func_name = frame.f_code.co_name

                # Skip internal functions that start with underscore
                # (except <module> which is the main code block)
                if func_name.startswith("_") and func_name != "<module>":
                    return trace_func

                if event == "line":
                    wit_world.report_trace(
                        lineno,
                        json.dumps({"type": "line"}),
                        "",
                    )
                elif event == "call":
                    wit_world.report_trace(
                        lineno,
                        json.dumps({"type": "call", "function": func_name}),
                        "",
                    )
                elif event == "return":
                    wit_world.report_trace(
                        lineno,
                        json.dumps({"type": "return", "function": func_name}),
                        "",
                    )
                elif event == "exception":
                    exc_type, exc_value, _ = arg
                    # Filter out StopIteration - it's normal async control flow,
                    # not a real exception. When an awaited value returns, Python
                    # internally "throws" a StopIteration with the result.
                    if exc_type is StopIteration:
                        return trace_func
                    wit_world.report_trace(
                        lineno,
                        json.dumps(
                            {
                                "type": "exception",
                                "exception_type": exc_type.__name__
                                if exc_type
                                else "Unknown",
                                "message": str(exc_value) if exc_value else "",
                            }
                        ),
                        "",
                    )

                return trace_func

            # Get execution globals with persistent state
            exec_globals = _get_exec_globals(invoke, list_callbacks)
            exec_locals = {}

            # Compile with top-level await support
            compiled = compile(
                code, "<string>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT
            )

            # Enable tracing
            # Note: sys.settrace doesn't work well with async code, so we only
            # trace synchronous portions. Callback start/end is traced explicitly.
            sys.settrace(trace_func)

            try:
                # CO_COROUTINE flag (0x80) indicates code contains top-level await
                if compiled.co_flags & 0x80:
                    # Create function from coroutine code and await it
                    fn = types.FunctionType(compiled, exec_globals)
                    await fn()
                else:
                    # Regular synchronous code
                    exec(compiled, exec_globals, exec_locals)
            finally:
                # Disable tracing
                sys.settrace(None)

            # Extract and persist user-defined globals
            # For async code, variables are in exec_globals; for sync, in exec_locals
            user_globals = _extract_user_globals(exec_globals)
            user_locals = _extract_user_globals(exec_locals)

            # Merge locals into persistent globals (locals take precedence)
            _persistent_globals.update(user_globals)
            _persistent_globals.update(user_locals)

            output = sys.stdout.getvalue()
            sys.stdout = old_stdout
            sys.stderr = old_stderr

            # Return stdout output (use print() to produce results)
            return output.rstrip("\n") if output else ""

        except Exception as e:
            sys.stdout = old_stdout
            sys.stderr = old_stderr
            sys.settrace(None)
            tb = traceback.format_exc()
            raise Err(f"{type(e).__name__}: {e}\n\nTraceback:\n{tb}")

    async def snapshot_state(self) -> bytes:
        """Capture a snapshot of the current Python session state.

        Returns the serialized state as bytes using pickle. This captures
        all user-defined variables from previous execute() calls.

        Returns:
            Pickled state bytes

        Raises:
            Err: If serialization fails or snapshot is too large
        """
        global _persistent_globals

        try:
            # Filter to only picklable items
            picklable, skipped = _filter_picklable(_persistent_globals)

            # Serialize with pickle
            data = pickle.dumps(picklable, protocol=pickle.HIGHEST_PROTOCOL)

            # Check size limit
            if len(data) > MAX_SNAPSHOT_SIZE:
                raise Err(
                    f"Snapshot too large: {len(data)} bytes "
                    f"(max {MAX_SNAPSHOT_SIZE} bytes)"
                )

            # If some items were skipped, we could log this but still succeed
            # For now, we silently skip unpicklable items

            return data

        except Err:
            raise
        except Exception as e:
            raise Err(f"Failed to snapshot state: {type(e).__name__}: {e}")

    async def restore_state(self, data: bytes) -> None:
        """Restore Python session state from a previously captured snapshot.

        After restore, subsequent execute() calls will have access to all
        variables that were present when the snapshot was taken.

        Args:
            data: Pickled state bytes from snapshot_state()

        Raises:
            Err: If deserialization fails
        """
        global _persistent_globals

        try:
            # Deserialize with pickle
            restored = pickle.loads(data)

            if not isinstance(restored, dict):
                raise Err(
                    f"Invalid snapshot: expected dict, got {type(restored).__name__}"
                )

            # Replace persistent globals with restored state
            _persistent_globals.clear()
            _persistent_globals.update(restored)

        except Err:
            raise
        except Exception as e:
            raise Err(f"Failed to restore state: {type(e).__name__}: {e}")

    async def clear_state(self) -> None:
        """Clear all persistent state from the session.

        After clear, subsequent execute() calls will start with a fresh
        namespace (no user-defined variables from previous calls).
        """
        global _persistent_globals
        _persistent_globals.clear()
