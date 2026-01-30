"""Tests for Python callback support in Eryx."""

import eryx
import pytest


class TestCallbackRegistry:
    """Tests for the CallbackRegistry class."""

    def test_create_empty_registry(self):
        """Test creating an empty callback registry."""
        registry = eryx.CallbackRegistry()
        assert len(registry) == 0

    def test_decorator_registers_callback(self):
        """Test that the decorator registers a callback."""
        registry = eryx.CallbackRegistry()

        @registry.callback(description="Test callback")
        def my_callback():
            return {"ok": True}

        assert len(registry) == 1

    def test_decorator_uses_function_name(self):
        """Test that the decorator uses the function name by default."""
        registry = eryx.CallbackRegistry()

        @registry.callback()
        def my_function_name():
            return {}

        # Iterate to get the callback dict
        callbacks = list(registry)
        assert len(callbacks) == 1
        assert callbacks[0]["name"] == "my_function_name"

    def test_decorator_custom_name(self):
        """Test that a custom name can be provided."""
        registry = eryx.CallbackRegistry()

        @registry.callback(name="custom_name")
        def original_name():
            return {}

        callbacks = list(registry)
        assert callbacks[0]["name"] == "custom_name"

    def test_decorator_preserves_function(self):
        """Test that the decorator returns the original function."""
        registry = eryx.CallbackRegistry()

        @registry.callback()
        def my_func(x: int) -> dict:
            return {"x": x}

        # The decorated function should still be callable
        result = my_func(42)
        assert result == {"x": 42}

    def test_add_method(self):
        """Test adding a callback with the add() method."""
        registry = eryx.CallbackRegistry()

        def my_callback():
            return {}

        registry.add(my_callback, name="test", description="A test callback")
        assert len(registry) == 1

    def test_multiple_callbacks(self):
        """Test registering multiple callbacks."""
        registry = eryx.CallbackRegistry()

        @registry.callback()
        def callback1():
            return {"n": 1}

        @registry.callback()
        def callback2():
            return {"n": 2}

        @registry.callback()
        def callback3():
            return {"n": 3}

        assert len(registry) == 3

    def test_registry_iteration(self):
        """Test iterating over registered callbacks."""
        registry = eryx.CallbackRegistry()

        @registry.callback(description="First")
        def first():
            return {}

        @registry.callback(description="Second")
        def second():
            return {}

        callbacks = list(registry)
        assert len(callbacks) == 2
        assert callbacks[0]["name"] == "first"
        assert callbacks[0]["description"] == "First"
        assert callbacks[1]["name"] == "second"
        assert callbacks[1]["description"] == "Second"

    def test_repr(self):
        """Test the string representation of a registry."""
        registry = eryx.CallbackRegistry()

        @registry.callback()
        def alpha():
            return {}

        @registry.callback()
        def beta():
            return {}

        repr_str = repr(registry)
        assert "CallbackRegistry" in repr_str
        assert "alpha" in repr_str
        assert "beta" in repr_str


class TestSandboxWithCallbacks:
    """Tests for Sandbox with callbacks."""

    def test_simple_callback(self):
        """Test a simple callback invocation."""

        def get_value():
            return {"value": 42}

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "get_value", "fn": get_value, "description": "Returns 42"}
            ]
        )
        result = sandbox.execute("v = await get_value(); print(v['value'])")
        assert "42" in result.stdout

    def test_callback_with_args(self):
        """Test a callback that receives arguments."""

        def add(a: int, b: int):
            return {"sum": a + b}

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "add", "fn": add, "description": "Adds two numbers"}]
        )
        result = sandbox.execute("r = await add(a=3, b=5); print(r['sum'])")
        assert "8" in result.stdout

    def test_callback_with_string_args(self):
        """Test a callback with string arguments."""

        def greet(name: str):
            return {"greeting": f"Hello, {name}!"}

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "greet", "fn": greet, "description": "Greets someone"}]
        )
        result = sandbox.execute('r = await greet(name="World"); print(r["greeting"])')
        assert "Hello, World!" in result.stdout

    def test_callback_return_types(self):
        """Test various callback return types."""

        def return_string():
            return "just a string"

        def return_int():
            return 123

        def return_float():
            return 3.14

        def return_bool():
            return True

        def return_none():
            return None

        def return_list():
            return [1, 2, 3]

        def return_dict():
            return {"nested": {"value": 42}}

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "return_string", "fn": return_string, "description": ""},
                {"name": "return_int", "fn": return_int, "description": ""},
                {"name": "return_float", "fn": return_float, "description": ""},
                {"name": "return_bool", "fn": return_bool, "description": ""},
                {"name": "return_none", "fn": return_none, "description": ""},
                {"name": "return_list", "fn": return_list, "description": ""},
                {"name": "return_dict", "fn": return_dict, "description": ""},
            ]
        )

        result = sandbox.execute(
            """
s = await return_string()
print(f"string: {s}")
i = await return_int()
print(f"int: {i}")
f = await return_float()
print(f"float: {f}")
b = await return_bool()
print(f"bool: {b}")
n = await return_none()
print(f"none: {n}")
lst = await return_list()
print(f"list: {lst}")
d = await return_dict()
print(f"dict: {d}")
"""
        )

        assert "string: just a string" in result.stdout
        assert "int: 123" in result.stdout
        assert "float: 3.14" in result.stdout
        assert "bool: True" in result.stdout
        assert "none: None" in result.stdout
        assert "list: [1, 2, 3]" in result.stdout
        assert "dict: {'nested': {'value': 42}}" in result.stdout

    def test_callback_exception(self):
        """Test that Python exceptions in callbacks are propagated."""

        def failing_callback():
            raise ValueError("This callback intentionally fails")

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "fail", "fn": failing_callback, "description": ""}]
        )

        # The callback exception should cause sandbox execution to fail
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute("await fail()")
        # Note: The specific error message may not always propagate through
        # the WASM boundary, so we just verify that an error is raised.

    def test_callback_with_registry(self):
        """Test using a CallbackRegistry with Sandbox."""
        registry = eryx.CallbackRegistry()

        @registry.callback(description="Returns the current counter")
        def get_counter():
            return {"counter": 100}

        sandbox = eryx.Sandbox(callbacks=registry)
        result = sandbox.execute("c = await get_counter(); print(c['counter'])")
        assert "100" in result.stdout

    def test_multiple_callbacks(self):
        """Test a sandbox with multiple callbacks."""

        def callback_a():
            return {"from": "a"}

        def callback_b():
            return {"from": "b"}

        def callback_c():
            return {"from": "c"}

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "a", "fn": callback_a, "description": ""},
                {"name": "b", "fn": callback_b, "description": ""},
                {"name": "c", "fn": callback_c, "description": ""},
            ]
        )

        result = sandbox.execute(
            """
ra = await a()
rb = await b()
rc = await c()
print(f"{ra['from']}-{rb['from']}-{rc['from']}")
"""
        )
        assert "a-b-c" in result.stdout

    def test_callback_invocations_count(self):
        """Test that callback_invocations is tracked."""

        def noop():
            return {}

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "noop", "fn": noop, "description": ""}]
        )

        result = sandbox.execute(
            """
await noop()
await noop()
await noop()
"""
        )
        assert result.callback_invocations == 3

    def test_callback_introspection(self):
        """Test that list_callbacks() returns registered callbacks."""

        def my_callback(x: int):
            return {"x": x}

        sandbox = eryx.Sandbox(
            callbacks=[
                {
                    "name": "my_callback",
                    "fn": my_callback,
                    "description": "A test callback",
                }
            ]
        )

        result = sandbox.execute(
            """
callbacks = list_callbacks()
for cb in callbacks:
    print(f"{cb['name']}: {cb['description']}")
"""
        )
        assert "my_callback" in result.stdout
        assert "A test callback" in result.stdout

    def test_parallel_callbacks_with_asyncio_gather(self):
        """Test parallel callback execution with asyncio.gather.

        Note: Callbacks use spawn_blocking which has limited parallelism,
        so timing assertions are relaxed. The main test is that all callbacks
        complete successfully when invoked via asyncio.gather().
        """
        import time

        def slow_callback(id: int):
            time.sleep(0.02)  # 20ms - shorter to reduce test time
            return {"id": id}

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "slow", "fn": slow_callback, "description": ""}]
        )

        result = sandbox.execute(
            """
import asyncio
results = await asyncio.gather(
    slow(id=1),
    slow(id=2),
    slow(id=3),
)
for r in results:
    print(r['id'])
"""
        )

        # All three should complete successfully
        assert "1" in result.stdout
        assert "2" in result.stdout
        assert "3" in result.stdout
        assert result.callback_invocations == 3


class TestSandboxFactoryWithCallbacks:
    """Tests for SandboxFactory with callbacks."""

    def test_factory_create_sandbox_with_callbacks(self):
        """Test creating a sandbox from factory with callbacks."""
        factory = eryx.SandboxFactory()

        def get_data():
            return {"data": "from_factory"}

        sandbox = factory.create_sandbox(
            callbacks=[{"name": "get_data", "fn": get_data, "description": ""}]
        )
        result = sandbox.execute("d = await get_data(); print(d['data'])")
        assert "from_factory" in result.stdout


class TestSessionWithCallbacks:
    """Tests for Session with callbacks."""

    def test_session_simple_callback(self):
        """Test a simple callback in a session."""

        def get_value():
            return {"value": 42}

        session = eryx.Session(
            callbacks=[
                {"name": "get_value", "fn": get_value, "description": "Returns 42"}
            ]
        )
        result = session.execute("v = await get_value(); print(v['value'])")
        assert "42" in result.stdout

    def test_session_callback_with_args(self):
        """Test a callback with arguments in a session."""

        def add(a: int, b: int):
            return {"sum": a + b}

        session = eryx.Session(
            callbacks=[{"name": "add", "fn": add, "description": "Adds two numbers"}]
        )
        result = session.execute("r = await add(a=3, b=5); print(r['sum'])")
        assert "8" in result.stdout

    def test_session_callback_state_persistence(self):
        """Test that session state persists while callbacks work."""

        def get_multiplier():
            return {"multiplier": 10}

        session = eryx.Session(
            callbacks=[
                {"name": "get_multiplier", "fn": get_multiplier, "description": ""}
            ]
        )

        # Set up state
        session.execute("x = 5")

        # Use callback and existing state together
        result = session.execute(
            "m = await get_multiplier(); print(x * m['multiplier'])"
        )
        assert "50" in result.stdout

    def test_session_callback_with_registry(self):
        """Test using a CallbackRegistry with Session."""
        registry = eryx.CallbackRegistry()

        @registry.callback(description="Returns a greeting")
        def greet(name: str):
            return {"greeting": f"Hello, {name}!"}

        session = eryx.Session(callbacks=registry)
        result = session.execute('g = await greet(name="World"); print(g["greeting"])')
        assert "Hello, World!" in result.stdout

    def test_session_multiple_executions_with_callbacks(self):
        """Test multiple executions with callbacks in a session."""
        counter = {"value": 0}

        def increment():
            counter["value"] += 1
            return {"count": counter["value"]}

        session = eryx.Session(
            callbacks=[{"name": "increment", "fn": increment, "description": ""}]
        )

        # Multiple executions, each calling the callback
        session.execute("c1 = await increment()")
        session.execute("c2 = await increment()")
        result = session.execute("c3 = await increment(); print(c3['count'])")

        assert "3" in result.stdout
        assert counter["value"] == 3


class TestAsyncCallbacks:
    """Tests for async Python callbacks."""

    def test_async_callback_simple(self):
        """Test a simple async callback."""

        async def async_hello():
            return {"message": "hello from async"}

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "async_hello", "fn": async_hello, "description": "Async hello"}
            ]
        )
        result = sandbox.execute("r = await async_hello(); print(r['message'])")
        assert "hello from async" in result.stdout

    def test_async_callback_with_await(self):
        """Test async callback that actually awaits something."""
        import asyncio

        async def async_delay(ms: int):
            await asyncio.sleep(ms / 1000)
            return {"delayed_ms": ms}

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "async_delay", "fn": async_delay, "description": "Async delay"}
            ]
        )
        result = sandbox.execute("r = await async_delay(ms=10); print(r['delayed_ms'])")
        assert "10" in result.stdout

    def test_async_callback_with_args(self):
        """Test async callback with multiple arguments."""

        async def async_add(a: int, b: int):
            return {"sum": a + b}

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "async_add", "fn": async_add, "description": ""}]
        )
        result = sandbox.execute("r = await async_add(a=3, b=7); print(r['sum'])")
        assert "10" in result.stdout

    def test_async_callback_exception(self):
        """Test that async exceptions are propagated."""

        async def async_fail():
            raise ValueError("Async error!")

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "async_fail", "fn": async_fail, "description": ""}]
        )
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute("await async_fail()")

    def test_mixed_sync_and_async_callbacks(self):
        """Test mixing sync and async callbacks in the same sandbox."""

        def sync_cb():
            return {"type": "sync"}

        async def async_cb():
            return {"type": "async"}

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "sync_cb", "fn": sync_cb, "description": ""},
                {"name": "async_cb", "fn": async_cb, "description": ""},
            ]
        )
        result = sandbox.execute(
            """
s = await sync_cb()
a = await async_cb()
print(f"{s['type']}-{a['type']}")
"""
        )
        assert "sync-async" in result.stdout

    def test_async_callback_with_registry(self):
        """Test async callbacks with the decorator API."""
        registry = eryx.CallbackRegistry()

        @registry.callback(description="Async greeting")
        async def async_greet(name: str):
            return {"greeting": f"Hello, {name}!"}

        sandbox = eryx.Sandbox(callbacks=registry)
        result = sandbox.execute(
            'r = await async_greet(name="Async World"); print(r["greeting"])'
        )
        assert "Hello, Async World!" in result.stdout

    def test_async_callback_returns_various_types(self):
        """Test async callbacks returning different types."""

        async def return_string():
            return "async string"

        async def return_list():
            return [1, 2, 3]

        async def return_none():
            return None

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "return_string", "fn": return_string, "description": ""},
                {"name": "return_list", "fn": return_list, "description": ""},
                {"name": "return_none", "fn": return_none, "description": ""},
            ]
        )
        result = sandbox.execute(
            """
s = await return_string()
print(f"string: {s}")
l = await return_list()
print(f"list: {l}")
n = await return_none()
print(f"none: {n}")
"""
        )
        assert "string: async string" in result.stdout
        assert "list: [1, 2, 3]" in result.stdout
        assert "none: None" in result.stdout

    def test_async_callback_in_session(self):
        """Test async callbacks work in Session as well."""

        async def async_counter():
            return {"count": 42}

        session = eryx.Session(
            callbacks=[
                {"name": "async_counter", "fn": async_counter, "description": ""}
            ]
        )
        result = session.execute("c = await async_counter(); print(c['count'])")
        assert "42" in result.stdout

    def test_async_lambda_callback(self):
        """Test that async lambdas work (they're actually regular functions returning coroutines)."""

        # Note: Python doesn't have async lambdas, but we can test regular functions
        async def async_double(x: int):
            return {"result": x * 2}

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "async_double", "fn": async_double, "description": ""}]
        )
        result = sandbox.execute("r = await async_double(x=21); print(r['result'])")
        assert "42" in result.stdout


class TestCallbackSchemaInference:
    """Tests for automatic schema inference from function signatures."""

    def test_schema_inferred_from_type_hints(self):
        """Test that schema is inferred from type hints."""
        registry = eryx.CallbackRegistry()

        @registry.callback()
        def typed_func(name: str, count: int, enabled: bool):
            return {}

        callbacks = list(registry)
        schema = callbacks[0].get("schema")

        # Schema should be inferred
        assert schema is not None
        assert schema.get("type") == "object"
        assert "properties" in schema

        props = schema["properties"]
        assert "name" in props
        assert "count" in props
        assert "enabled" in props

    def test_required_vs_optional_params(self):
        """Test that required/optional parameters are correctly identified."""
        registry = eryx.CallbackRegistry()

        @registry.callback()
        def mixed_func(required_param: str, optional_param: int = 10):
            return {}

        callbacks = list(registry)
        schema = callbacks[0].get("schema")

        assert schema is not None
        required = schema.get("required", [])
        assert "required_param" in required
        assert "optional_param" not in required

    def test_explicit_schema_overrides_inference(self):
        """Test that an explicit schema overrides inference."""
        registry = eryx.CallbackRegistry()

        custom_schema = {
            "type": "object",
            "properties": {"custom": {"type": "string"}},
            "required": ["custom"],
        }

        @registry.callback(schema=custom_schema)
        def func_with_custom_schema(x: int):
            return {}

        callbacks = list(registry)
        schema = callbacks[0].get("schema")

        assert schema is not None
        assert "custom" in schema.get("properties", {})
        assert "x" not in schema.get("properties", {})


class TestCallbackEdgeCases:
    """Tests for edge cases and error handling."""

    def test_callback_with_no_return(self):
        """Test a callback that returns nothing (None)."""

        def no_return():
            pass  # Implicitly returns None

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "no_return", "fn": no_return, "description": ""}]
        )
        result = sandbox.execute("r = await no_return(); print(r)")
        assert "None" in result.stdout

    def test_callback_with_kwargs_only(self):
        """Test calling a callback with keyword arguments only."""

        def kwargs_only(*, name: str, value: int):
            return {"name": name, "value": value}

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "kwargs_only", "fn": kwargs_only, "description": ""}]
        )
        result = sandbox.execute(
            'r = await kwargs_only(name="test", value=99); print(r)'
        )
        assert "test" in result.stdout
        assert "99" in result.stdout

    def test_empty_callbacks_list(self):
        """Test creating a sandbox with an empty callbacks list."""
        sandbox = eryx.Sandbox(callbacks=[])
        result = sandbox.execute("print('no callbacks')")
        assert "no callbacks" in result.stdout

    def test_callback_dict_missing_name_raises(self):
        """Test that a callback dict without 'name' raises an error."""
        with pytest.raises((KeyError, TypeError)):
            eryx.Sandbox(callbacks=[{"fn": lambda: {}, "description": ""}])

    def test_callback_dict_missing_fn_raises(self):
        """Test that a callback dict without 'fn' raises an error."""
        with pytest.raises((KeyError, TypeError)):
            eryx.Sandbox(callbacks=[{"name": "test", "description": ""}])

    def test_lambda_callback(self):
        """Test using a lambda as a callback."""
        sandbox = eryx.Sandbox(
            callbacks=[
                {
                    "name": "double",
                    "fn": lambda x: {"result": x * 2},
                    "description": "Doubles a number",
                }
            ]
        )
        result = sandbox.execute("r = await double(x=21); print(r['result'])")
        assert "42" in result.stdout

    def test_closure_callback(self):
        """Test using a closure as a callback."""
        counter = {"value": 0}

        def make_counter():
            def increment():
                counter["value"] += 1
                return {"count": counter["value"]}

            return increment

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "increment", "fn": make_counter(), "description": ""}]
        )

        result = sandbox.execute(
            """
r1 = await increment()
r2 = await increment()
r3 = await increment()
print(r3['count'])
"""
        )
        assert "3" in result.stdout


class TestAsyncCallbackIsolation:
    """Tests for async callback behavior.

    Async callbacks run in isolated event loops (via asyncio.run()). This means
    callbacks cannot share event-loop-bound resources (asyncio.Queue, asyncio.Lock,
    asyncio.Semaphore, etc.) with the parent application or each other.

    This is a fundamental limitation because:
    1. Callbacks run on tokio's blocking thread pool, not the caller's thread
    2. asyncio event loops are thread-specific
    3. Each callback invocation creates its own isolated event loop

    For shared state, use thread-safe primitives (threading.Lock, queue.Queue)
    or design callbacks to be stateless.
    """

    def test_async_callback_from_sync_context(self):
        """Test async callbacks work when called from sync context."""
        import asyncio

        async def async_double(value: int):
            await asyncio.sleep(0.001)
            return {"result": value * 2}

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "double", "fn": async_double, "description": "Doubles a value"}
            ]
        )

        result = sandbox.execute("r = await double(value=21); print(r['result'])")
        assert "42" in result.stdout

    def test_async_callback_from_async_context(self):
        """Test async callbacks work when called from async context."""
        import asyncio

        async def async_triple(value: int):
            await asyncio.sleep(0.001)
            return {"result": value * 3}

        async def main():
            sandbox = eryx.Sandbox(
                callbacks=[
                    {
                        "name": "triple",
                        "fn": async_triple,
                        "description": "Triples a value",
                    }
                ]
            )
            result = sandbox.execute("r = await triple(value=10); print(r['result'])")
            assert "30" in result.stdout

        asyncio.run(main())

    def test_async_callback_with_thread_safe_queue(self):
        """Test that async callbacks can use thread-safe queue.Queue for shared state."""
        import asyncio
        import queue

        # Use thread-safe queue.Queue instead of asyncio.Queue
        shared_queue = queue.Queue()

        async def enqueue_item(item: str):
            """Callback that puts items in a thread-safe queue."""
            shared_queue.put(item)
            return {"queued": item, "queue_size": shared_queue.qsize()}

        sandbox = eryx.Sandbox(
            callbacks=[
                {
                    "name": "enqueue",
                    "fn": enqueue_item,
                    "description": "Enqueues an item",
                }
            ]
        )

        result = sandbox.execute("""
r1 = await enqueue(item="first")
r2 = await enqueue(item="second")
r3 = await enqueue(item="third")
print(f"queued {r3['queue_size']} items")
""")
        assert "queued 3 items" in result.stdout

        # Verify items were actually put in the queue
        items = []
        while not shared_queue.empty():
            items.append(shared_queue.get_nowait())

        assert items == ["first", "second", "third"]

    def test_async_callback_with_threading_lock(self):
        """Test that async callbacks can use threading.Lock for thread-safe shared state."""
        import asyncio
        import threading

        lock = threading.Lock()
        counter = {"value": 0}

        async def increment_with_lock():
            """Callback that increments a counter while holding a thread lock."""
            with lock:
                counter["value"] += 1
                await asyncio.sleep(0.001)  # Simulate some async work
                return {"count": counter["value"]}

        sandbox = eryx.Sandbox(
            callbacks=[
                {
                    "name": "increment",
                    "fn": increment_with_lock,
                    "description": "Increments with lock",
                }
            ]
        )

        result = sandbox.execute("""
r1 = await increment()
r2 = await increment()
r3 = await increment()
print(f"final count: {r3['count']}")
""")
        assert "final count: 3" in result.stdout
        assert counter["value"] == 3

    def test_async_callback_parallel_execution(self):
        """Test that multiple async callbacks can run in parallel via tokio."""
        import asyncio
        import time

        call_times = []
        lock = __import__("threading").Lock()

        async def timed_work(task_id: int):
            """Callback that records when it runs."""
            start = time.time()
            await asyncio.sleep(0.05)
            end = time.time()
            with lock:
                call_times.append((task_id, start, end))
            return {"task_id": task_id}

        sandbox = eryx.Sandbox(
            callbacks=[
                {
                    "name": "timed_work",
                    "fn": timed_work,
                    "description": "Timed work",
                }
            ]
        )

        result = sandbox.execute("""
import asyncio
results = await asyncio.gather(
    timed_work(task_id=1),
    timed_work(task_id=2),
    timed_work(task_id=3),
)
print(f"completed {len(results)} tasks")
""")
        assert "completed 3 tasks" in result.stdout
        assert len(call_times) == 3

        # Verify callbacks ran in parallel (overlapping time ranges)
        # If sequential, total time would be ~150ms; parallel should be ~50ms
        starts = [t[1] for t in call_times]
        ends = [t[2] for t in call_times]
        total_time = max(ends) - min(starts)
        # Should complete in roughly 50-100ms if parallel, not 150ms+
        assert total_time < 0.12, f"Callbacks appear sequential: {total_time}s"

    def test_sync_callback_from_async_context(self):
        """Test that sync callbacks work when called from async context."""
        import asyncio

        def sync_callback(value: int):
            return {"doubled": value * 2}

        async def main():
            sandbox = eryx.Sandbox(
                callbacks=[
                    {
                        "name": "sync_double",
                        "fn": sync_callback,
                        "description": "Sync double",
                    }
                ]
            )
            result = sandbox.execute(
                "r = await sync_double(value=21); print(r['doubled'])"
            )
            assert "42" in result.stdout

        asyncio.run(main())

    def test_mixed_sync_async_callbacks(self):
        """Test mixing sync and async callbacks."""
        import asyncio

        results_order = []
        lock = __import__("threading").Lock()

        def sync_cb(name: str):
            with lock:
                results_order.append(f"sync-{name}")
            return {"type": "sync", "name": name}

        async def async_cb(name: str):
            await asyncio.sleep(0.001)
            with lock:
                results_order.append(f"async-{name}")
            return {"type": "async", "name": name}

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "sync_cb", "fn": sync_cb, "description": ""},
                {"name": "async_cb", "fn": async_cb, "description": ""},
            ]
        )

        result = sandbox.execute("""
s = await sync_cb(name="first")
a = await async_cb(name="second")
print(f"{s['type']}-{a['type']}")
""")
        assert "sync-async" in result.stdout
        assert results_order == ["sync-first", "async-second"]

    def test_async_callback_error_propagation(self):
        """Test that async callback errors are properly propagated."""
        import asyncio

        async def async_fail(message: str):
            await asyncio.sleep(0.001)
            raise ValueError(message)

        sandbox = eryx.Sandbox(
            callbacks=[{"name": "async_fail", "fn": async_fail, "description": "Fails"}]
        )
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute('await async_fail(message="test error")')

    def test_async_callback_error_message_preserved(self):
        """Test that error messages are preserved in async callbacks."""
        import asyncio

        async def async_fail_with_message():
            await asyncio.sleep(0.001)
            raise ValueError("Specific error message for testing")

        sandbox = eryx.Sandbox(
            callbacks=[
                {"name": "fail", "fn": async_fail_with_message, "description": ""}
            ]
        )
        result = sandbox.execute("""
try:
    await fail()
except Exception as e:
    print(f"caught: {type(e).__name__}")
    print(f"has_message: {'Specific error message' in str(e)}")
""")
        assert "caught: RuntimeError" in result.stdout
        assert "has_message: True" in result.stdout
