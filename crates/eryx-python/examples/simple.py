#!/usr/bin/env python3
"""Simple sandbox example.

Demonstrates basic usage of the eryx sandbox for executing Python code
in complete isolation.

Usage:
    python examples/simple.py
"""

import eryx


def main():
    print("=== Eryx Simple Example ===\n")

    # Create a sandbox with the embedded Python runtime
    # This includes a complete Python interpreter running inside WebAssembly
    print("Creating sandbox...")
    sandbox = eryx.Sandbox()

    # Execute simple Python code
    print("\n--- Example 1: Hello World ---")
    result = sandbox.execute('print("Hello from the sandbox!")')
    print(f"Output: {result.stdout}")
    print(f"Duration: {result.duration_ms:.2f}ms")

    # Execute code with variables and expressions
    print("\n--- Example 2: Arithmetic ---")
    result = sandbox.execute("""
x = 2 + 2
y = 10 * 5
print(f"2 + 2 = {x}")
print(f"10 * 5 = {y}")
print(f"Sum: {x + y}")
""")
    print(f"Output:\n{result.stdout}")

    # Execute code with loops and data structures
    print("--- Example 3: Data Structures ---")
    result = sandbox.execute("""
# Lists
numbers = [1, 2, 3, 4, 5]
squared = [n ** 2 for n in numbers]
print(f"Numbers: {numbers}")
print(f"Squared: {squared}")

# Dictionaries
person = {"name": "Alice", "age": 30}
print(f"Person: {person}")

# Sets
unique = set([1, 2, 2, 3, 3, 3])
print(f"Unique: {unique}")
""")
    print(f"Output:\n{result.stdout}")

    # Show execution statistics
    print("--- Execution Statistics ---")
    print(f"Duration: {result.duration_ms:.2f}ms")
    print(f"Callback invocations: {result.callback_invocations}")
    if result.peak_memory_bytes:
        print(f"Peak memory: {result.peak_memory_bytes / 1024 / 1024:.2f}MB")

    # Demonstrate isolation - the sandbox cannot access the host filesystem
    print("\n--- Example 4: Sandbox Isolation ---")
    result = sandbox.execute("""
import os

try:
    # This will fail or return empty - no access to host filesystem
    files = os.listdir("/")
    print(f"Root directory: {files[:5]}...")  # Only shows sandbox's virtual FS
except Exception as e:
    print(f"Filesystem access: {type(e).__name__}")

# No network access either
try:
    import socket
    s = socket.socket()
    s.connect(("example.com", 80))
except Exception as e:
    print(f"Network access: {type(e).__name__}")
""")
    print(f"Output:\n{result.stdout}")

    print("=== Done ===")


if __name__ == "__main__":
    main()
