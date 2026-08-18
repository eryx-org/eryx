#!/usr/bin/env python3
"""
Resource Limits Example

Demonstrates how to configure and use resource limits in Eryx sandboxes.
Resource limits help protect against runaway code, infinite loops,
and excessive resource consumption.
"""

import eryx


def main():
    print("=== Resource Limits Example ===\n")

    # Example 1: Default limits
    print("--- Example 1: Default Limits ---")
    default_limits = eryx.ResourceLimits()
    print(f"Default limits: {default_limits}")
    print(f"  Execution timeout: {default_limits.execution_timeout_ms}ms")
    print(f"  Callback timeout: {default_limits.callback_timeout_ms}ms")
    print(f"  Max memory: {default_limits.max_memory_bytes} bytes")
    print(f"  Max callbacks: {default_limits.max_callback_invocations}")
    print()

    # Example 2: Custom strict limits
    print("--- Example 2: Custom Strict Limits ---")
    strict_limits = eryx.ResourceLimits(
        execution_timeout_ms=1000,  # 1 second
        max_memory_bytes=50_000_000,  # 50 MB
        max_callback_invocations=10,
    )
    print(f"Strict limits: {strict_limits}")

    sandbox = eryx.Sandbox(resource_limits=strict_limits)
    result = sandbox.execute('print("Quick execution with strict limits")')
    print(f"Output: {result.stdout}")
    print()

    # Example 3: Timeout handling
    print("--- Example 3: Timeout Handling ---")
    timeout_limits = eryx.ResourceLimits(execution_timeout_ms=100)  # 100ms
    sandbox = eryx.Sandbox(resource_limits=timeout_limits)

    try:
        # This will timeout
        result = sandbox.execute("""
import time
# Simulate long-running computation
total = 0
for i in range(10_000_000):
    total += i
print(f"Total: {total}")
""")
        print(f"Output: {result.stdout}")
    except eryx.TimeoutError as e:
        print(f"Caught timeout: {e}")
    print()

    # Example 4: Unlimited limits (use with caution!)
    print("--- Example 4: Unlimited Limits ---")
    unlimited = eryx.ResourceLimits.unlimited()
    print(f"Unlimited: {unlimited}")
    print("Warning: Only use unlimited limits with trusted code!")
    print()

    # Example 5: Disabling specific limits
    print("--- Example 5: Selective Limits ---")
    selective_limits = eryx.ResourceLimits(
        execution_timeout_ms=60000,  # 60 seconds
        callback_timeout_ms=None,  # No callback timeout
        max_memory_bytes=256_000_000,  # 256 MB
        max_callback_invocations=None,  # Unlimited callbacks
    )
    print(f"Selective limits: {selective_limits}")
    print()

    print("=== Done ===")


if __name__ == "__main__":
    main()
