#!/usr/bin/env python3
"""
Error Handling Example

Demonstrates how to handle various exceptions that can occur
when executing code in the Eryx sandbox.
"""

import eryx


def main():
    print("=== Eryx Error Handling Examples ===\n")

    # Example 1: ExecutionError - Python code raises an exception
    print("--- Example 1: ExecutionError ---")
    sandbox = eryx.Sandbox()
    try:
        sandbox.execute("raise ValueError('Something went wrong!')")
    except eryx.ExecutionError as e:
        print(f"Caught ExecutionError: {e}")
    print()

    # Example 2: TimeoutError - Code takes too long
    print("--- Example 2: TimeoutError ---")
    sandbox = eryx.Sandbox(
        resource_limits=eryx.ResourceLimits(execution_timeout_ms=100)
    )
    try:
        sandbox.execute("while True: pass")
    except eryx.TimeoutError as e:
        print(f"Caught TimeoutError: {e}")
    print()

    # Example 3: Syntax errors in user code
    print("--- Example 3: Syntax Error ---")
    sandbox = eryx.Sandbox()
    try:
        sandbox.execute("def broken(")
    except eryx.ExecutionError as e:
        print(f"Caught ExecutionError (syntax): {e}")
    print()

    # Example 4: Import errors (sandboxed code can't import host modules)
    print("--- Example 4: Import Error ---")
    sandbox = eryx.Sandbox()
    try:
        sandbox.execute("import requests  # Not available in sandbox")
    except eryx.ExecutionError as e:
        print(f"Caught ExecutionError (import): {e}")
    print()

    # Example 5: Catching all Eryx errors with the base class
    print("--- Example 5: Catching with Base Class ---")
    sandbox = eryx.Sandbox()
    try:
        sandbox.execute("raise RuntimeError('Generic failure')")
    except eryx.EryxError as e:
        print(f"Caught EryxError (base class): {type(e).__name__}: {e}")
    print()

    # Example 6: Safe execution wrapper
    print("--- Example 6: Safe Execution Wrapper ---")

    def safe_execute(sandbox, code):
        """Execute code and return (result, error) tuple."""
        try:
            result = sandbox.execute(code)
            return result, None
        except eryx.TimeoutError as e:
            return None, f"Timeout: {e}"
        except eryx.ResourceLimitError as e:
            return None, f"Resource limit: {e}"
        except eryx.ExecutionError as e:
            return None, f"Execution failed: {e}"
        except eryx.EryxError as e:
            return None, f"Unknown error: {e}"

    sandbox = eryx.Sandbox()

    # Good code
    result, error = safe_execute(sandbox, "print('Hello!')")
    if error:
        print(f"  Error: {error}")
    else:
        print(f"  Success: {result.stdout.strip()}")

    # Bad code
    result, error = safe_execute(sandbox, "1/0")
    if error:
        print(f"  Error: {error}")
    else:
        print(f"  Success: {result.stdout.strip()}")

    print("\n=== Done ===")


if __name__ == "__main__":
    main()
