#!/usr/bin/env python3
"""Example demonstrating callback support in Eryx.

This example shows how to register Python functions as callbacks that can be
invoked from sandboxed Python code. Two APIs are demonstrated:

1. Dict-based API: Simple and explicit, good for dynamic callback registration
2. Decorator-based API: Pythonic and convenient, good for static callbacks

Run with: python examples/callbacks.py
"""

import time

import eryx


def main():
    print("=== Eryx Callback Examples ===\n")

    # =========================================================================
    # Example 1: Dict-based API
    # =========================================================================
    print("--- Example 1: Dict-based Callback API ---")

    def get_time():
        """Returns the current Unix timestamp."""
        return {"timestamp": time.time()}

    def echo(message: str, repeat: int = 1):
        """Echoes the message back, optionally repeated."""
        return {"echoed": message * repeat}

    def add(a: int, b: int):
        """Adds two numbers."""
        return {"sum": a + b}

    sandbox = eryx.Sandbox(
        callbacks=[
            {
                "name": "get_time",
                "fn": get_time,
                "description": "Returns current timestamp",
            },
            {"name": "echo", "fn": echo, "description": "Echoes the message"},
            {"name": "add", "fn": add, "description": "Adds two numbers"},
        ]
    )

    result = sandbox.execute("""
# Call the get_time callback
t = await get_time()
print(f"Current timestamp: {t['timestamp']:.2f}")

# Call the echo callback
response = await echo(message="Hello! ", repeat=3)
print(f"Echo: {response['echoed']}")

# Call the add callback
result = await add(a=17, b=25)
print(f"17 + 25 = {result['sum']}")
""")

    print(f"Output:\n{result.stdout}")
    print(f"Callback invocations: {result.callback_invocations}")
    print()

    # =========================================================================
    # Example 2: Decorator-based API with CallbackRegistry
    # =========================================================================
    print("--- Example 2: Decorator-based Callback API ---")

    registry = eryx.CallbackRegistry()

    @registry.callback(description="Greets a person by name")
    def greet(name: str, formal: bool = False):
        if formal:
            return {"greeting": f"Good day, {name}. How may I assist you?"}
        else:
            return {"greeting": f"Hey {name}! What's up?"}

    @registry.callback(name="calculate", description="Performs a calculation")
    def do_calculation(operation: str, a: float, b: float):
        ops = {
            "add": a + b,
            "sub": a - b,
            "mul": a * b,
            "div": a / b if b != 0 else float("inf"),
        }
        result = ops.get(operation, 0)
        return {"result": result, "operation": operation}

    sandbox2 = eryx.Sandbox(callbacks=registry)

    result = sandbox2.execute("""
# Use the greet callback
informal = await greet(name="Alice")
print(f"Informal: {informal['greeting']}")

formal = await greet(name="Dr. Smith", formal=True)
print(f"Formal: {formal['greeting']}")

# Use the calculate callback
for op in ["add", "sub", "mul", "div"]:
    r = await calculate(operation=op, a=10, b=3)
    print(f"10 {op} 3 = {r['result']:.2f}")
""")

    print(f"Output:\n{result.stdout}")
    print()

    # =========================================================================
    # Example 3: Callback Introspection
    # =========================================================================
    print("--- Example 3: Callback Introspection ---")

    result = sandbox2.execute("""
# List all available callbacks
callbacks = list_callbacks()
print(f"Available callbacks ({len(callbacks)}):")
for cb in callbacks:
    print(f"  - {cb['name']}: {cb['description']}")
    schema = cb.get('parameters_schema', {})
    if schema and 'properties' in schema:
        props = list(schema['properties'].keys())
        if props:
            print(f"    Parameters: {props}")
""")

    print(f"Output:\n{result.stdout}")
    print()

    # =========================================================================
    # Example 4: Parallel Callback Execution
    # =========================================================================
    print("--- Example 4: Parallel Callback Execution ---")

    def slow_operation(id: int, delay_ms: int = 50):
        """Simulates a slow operation."""
        time.sleep(delay_ms / 1000)
        return {"id": id, "delay_ms": delay_ms}

    sandbox3 = eryx.Sandbox(
        callbacks=[
            {"name": "slow_op", "fn": slow_operation, "description": "Slow operation"}
        ]
    )

    start = time.time()
    result = sandbox3.execute("""
import asyncio

# Run multiple callbacks in parallel using asyncio.gather
results = await asyncio.gather(
    slow_op(id=1, delay_ms=100),
    slow_op(id=2, delay_ms=100),
    slow_op(id=3, delay_ms=100),
)

for r in results:
    print(f"Completed operation {r['id']}")
""")
    elapsed = time.time() - start

    print(f"Output:\n{result.stdout}")
    print(f"Total time: {elapsed * 1000:.0f}ms (parallel execution)")
    print(f"Sequential would take: ~300ms")
    print()

    # =========================================================================
    # Example 5: Stateful Callbacks with Closures
    # =========================================================================
    print("--- Example 5: Stateful Callbacks ---")

    # Create a stateful counter using a closure
    def create_counter(initial: int = 0):
        state = {"count": initial}

        def increment(amount: int = 1):
            state["count"] += amount
            return {"count": state["count"]}

        def get_count():
            return {"count": state["count"]}

        return increment, get_count

    increment, get_count = create_counter(10)

    sandbox4 = eryx.Sandbox(
        callbacks=[
            {"name": "increment", "fn": increment, "description": "Increments counter"},
            {"name": "get_count", "fn": get_count, "description": "Gets current count"},
        ]
    )

    result = sandbox4.execute("""
# Get initial count
c = await get_count()
print(f"Initial count: {c['count']}")

# Increment several times
await increment(amount=5)
await increment(amount=3)
await increment()  # default amount=1

# Get final count
c = await get_count()
print(f"Final count: {c['count']}")
""")

    print(f"Output:\n{result.stdout}")
    print()

    # =========================================================================
    # Example 6: Async Callbacks
    # =========================================================================
    print("--- Example 6: Async Callbacks ---")

    import asyncio

    registry2 = eryx.CallbackRegistry()

    @registry2.callback(description="Async delay that returns after sleeping")
    async def async_delay(ms: int):
        await asyncio.sleep(ms / 1000)
        return {"delayed_ms": ms, "message": "completed"}

    @registry2.callback(description="Async greeting")
    async def async_greet(name: str):
        # Simulate async operation
        await asyncio.sleep(0.001)
        return {"greeting": f"Hello async, {name}!"}

    # Mix sync and async callbacks
    def sync_timestamp():
        return {"timestamp": time.time()}

    registry2.add(sync_timestamp, name="sync_timestamp", description="Sync timestamp")

    sandbox6 = eryx.Sandbox(callbacks=registry2)

    result = sandbox6.execute("""
# Call async callbacks - they work just like sync ones from Python's perspective
delayed = await async_delay(ms=10)
print(f"Async delay result: {delayed}")

greeting = await async_greet(name="World")
print(f"Async greeting: {greeting['greeting']}")

# Mix sync and async
ts = await sync_timestamp()
print(f"Sync timestamp: {ts['timestamp']:.2f}")
""")

    print(f"Output:\n{result.stdout}")
    print()

    # =========================================================================
    # Example 7: Error Handling
    # =========================================================================
    print("--- Example 7: Error Handling in Callbacks ---")

    def may_fail(should_fail: bool = False):
        """A callback that may fail."""
        if should_fail:
            raise ValueError("Intentional failure for demonstration")
        return {"status": "success"}

    sandbox5 = eryx.Sandbox(
        callbacks=[{"name": "may_fail", "fn": may_fail, "description": "May fail"}]
    )

    result = sandbox5.execute("""
# Successful call
r = await may_fail(should_fail=False)
print(f"First call: {r['status']}")

# Failing call with error handling
try:
    await may_fail(should_fail=True)
except Exception as e:
    print(f"Caught error: {type(e).__name__}")
    print(f"Error message contains 'Intentional failure': {'Intentional failure' in str(e)}")
""")

    print(f"Output:\n{result.stdout}")
    print()

    # =========================================================================
    # Example 8: Async Error Handling
    # =========================================================================
    print("--- Example 8: Async Error Handling ---")

    async def async_may_fail(should_fail: bool = False):
        """An async callback that may fail."""
        await asyncio.sleep(0.001)  # Simulate async work
        if should_fail:
            raise ValueError("Async intentional failure")
        return {"status": "async success"}

    sandbox8 = eryx.Sandbox(
        callbacks=[
            {"name": "async_may_fail", "fn": async_may_fail, "description": "May fail"}
        ]
    )

    result = sandbox8.execute("""
# Successful async call
r = await async_may_fail(should_fail=False)
print(f"Async success: {r['status']}")

# Failing async call with error handling
try:
    await async_may_fail(should_fail=True)
except Exception as e:
    print(f"Caught async error: {type(e).__name__}")
""")

    print(f"Output:\n{result.stdout}")
    print()

    print("=== All Examples Complete ===")


if __name__ == "__main__":
    main()
