"""Eryx Python WASM runtime with async callback support.

This module provides the WIT world implementation for executing Python code
in a WebAssembly sandbox with access to async callbacks via the host.
"""

import ast
import asyncio
import json
import math
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


class WitWorld(wit_world.WitWorld):
    async def execute(self, code: str) -> str:
        """Execute Python code with top-level await support.

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
        old_stdout = sys.stdout
        old_stderr = sys.stderr

        # Track current line for tracing
        current_preamble_lines = 0

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
                lineno = frame.f_lineno

                # Determine if we're in preamble code
                is_preamble = frame.f_code.co_filename != "<string>"

                if event == "line":
                    wit_world.report_trace(
                        lineno,
                        json.dumps({"type": "line", "is_preamble": is_preamble}),
                        "",
                    )
                elif event == "call":
                    func_name = frame.f_code.co_name
                    wit_world.report_trace(
                        lineno,
                        json.dumps(
                            {
                                "type": "call",
                                "function": func_name,
                                "is_preamble": is_preamble,
                            }
                        ),
                        "",
                    )
                elif event == "return":
                    func_name = frame.f_code.co_name
                    wit_world.report_trace(
                        lineno,
                        json.dumps(
                            {
                                "type": "return",
                                "function": func_name,
                                "is_preamble": is_preamble,
                            }
                        ),
                        "",
                    )
                elif event == "exception":
                    exc_type, exc_value, _ = arg
                    wit_world.report_trace(
                        lineno,
                        json.dumps(
                            {
                                "type": "exception",
                                "exception_type": exc_type.__name__
                                if exc_type
                                else "Unknown",
                                "message": str(exc_value) if exc_value else "",
                                "is_preamble": is_preamble,
                            }
                        ),
                        "",
                    )

                return trace_func

            # Execution environment with pre-imported modules
            exec_globals = {
                "__builtins__": __builtins__,
                # Eryx API
                "invoke": invoke,
                "list_callbacks": list_callbacks,
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
            exec_locals = {}

            # Compile with top-level await support
            compiled = compile(
                code, "<string>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT
            )

            # Enable tracing
            # Note: sys.settrace doesn't work well with async code, so we only
            # trace synchronous portions. Callback start/end is traced explicitly.
            # Uncomment if sync tracing is desired:
            # sys.settrace(trace_func)

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
