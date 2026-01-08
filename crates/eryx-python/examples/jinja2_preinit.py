#!/usr/bin/env python3
"""
Jinja2 Template Sandbox Example with Pre-Initialization

Demonstrates sandboxed Jinja2 template evaluation using eryx with
pre-initialization for fast sandbox creation.

This is similar to jinja2_sandbox.py but uses PreInitializedRuntime
to achieve ~10-20ms sandbox creation instead of ~700ms.

Prerequisites:
    # Download jinja2 (pure Python)
    pip download --only-binary=:all: --dest /tmp/wheels jinja2

    # Get markupsafe WASI wheel (compiled for WebAssembly)
    # See: https://github.com/AustinMCrane/wasi-wheels or similar
    # Copy markupsafe-*-wasi*.whl to /tmp/wheels/

Usage:
    python examples/jinja2_preinit.py
"""

import json
import time
from pathlib import Path

import eryx


def main():
    print("=== Jinja2 Template Sandbox with Pre-Initialization ===\n")

    # Check for required wheel files
    wheels_dir = Path("/tmp/wheels")
    if not wheels_dir.exists():
        print_download_instructions()
        return

    # Find the wheel files
    jinja2_wheel = find_wheel(wheels_dir, "jinja2")
    markupsafe_wheel = find_wheel(wheels_dir, "markupsafe")

    if not jinja2_wheel or not markupsafe_wheel:
        print_download_instructions()
        return

    print("Found wheels:")
    print(f"  - {jinja2_wheel}")
    print(f"  - {markupsafe_wheel}")
    print()

    # Create pre-initialized runtime with jinja2 already imported
    # This is slow (~3-5s) but only needs to be done once
    print("Creating pre-initialized runtime...")
    print("  (This takes 3-5 seconds but only happens once)")
    start = time.perf_counter()
    preinit = eryx.PreInitializedRuntime(
        packages=[str(jinja2_wheel), str(markupsafe_wheel)],
        imports=["jinja2"],  # Pre-import jinja2 during initialization
    )
    preinit_time = time.perf_counter() - start
    print(f"  Pre-initialized in {preinit_time:.2f}s")
    print(f"  Runtime size: {preinit.size_bytes / 1_000_000:.1f} MB")
    print()

    # Example 1: Simple template - fast sandbox creation!
    print("--- Example 1: Simple template ---")
    start = time.perf_counter()
    sandbox = preinit.create_sandbox()
    sandbox_time = time.perf_counter() - start

    start = time.perf_counter()
    result = sandbox.execute("""
from jinja2 import Template
template = Template("Hello, {{ name }}!")
print(template.render(name="World"))
""")
    exec_time = time.perf_counter() - start
    print(f"  Sandbox creation: {sandbox_time * 1000:.1f}ms")
    print(f"  Execute time: {exec_time * 1000:.1f}ms")
    print(f"  Output: {result.stdout}")
    print()

    # Example 2: Multiple fast sandbox creations
    print("--- Example 2: Multiple sandboxes (showing speed) ---")
    times = []
    for i in range(5):
        start = time.perf_counter()
        sandbox = preinit.create_sandbox()
        result = sandbox.execute(f"""
from jinja2 import Template
t = Template("Sandbox #{{{{ n }}}}: {{{{ msg }}}}")
print(t.render(n={i + 1}, msg="Hello!"))
""")
        elapsed = time.perf_counter() - start
        times.append(elapsed)
        print(f"  {result.stdout.strip()} ({elapsed * 1000:.1f}ms total)")

    avg_time = sum(times) / len(times)
    print(f"  Average: {avg_time * 1000:.1f}ms per sandbox+execute")
    print()

    # Example 3: User-supplied template (untrusted input)
    print("--- Example 3: User-supplied template (untrusted) ---")

    user_template = """
Report for {{ company }}
========================
{% for dept in departments %}
Department: {{ dept.name }}
  Budget: ${{ "{:,.2f}".format(dept.budget) }}
  Employees: {{ dept.employees|length }}
{% endfor %}
Total Budget: ${{ "{:,.2f}".format(departments|sum(attribute='budget')) }}
"""

    user_data = {
        "company": "Acme Corp",
        "departments": [
            {
                "name": "Engineering",
                "budget": 500000,
                "employees": ["Alice", "Bob", "Charlie"],
            },
            {"name": "Marketing", "budget": 200000, "employees": ["Diana", "Eve"]},
            {"name": "Operations", "budget": 150000, "employees": ["Frank"]},
        ],
    }

    start = time.perf_counter()
    sandbox = preinit.create_sandbox()
    result = sandbox.execute(f"""
import json
from jinja2 import Template

template_str = '''{user_template}'''
data = json.loads('''{json.dumps(user_data)}''')

template = Template(template_str)
output = template.render(**data)
print(output)
""")
    total_time = time.perf_counter() - start
    print(f"  Total time: {total_time * 1000:.1f}ms")
    print(f"  Output:\n{result.stdout}")

    # Example 4: Save and load runtime for even faster startup
    print("--- Example 4: Save and load runtime ---")
    runtime_path = Path("/tmp/eryx-jinja2-runtime.bin")

    # Save the pre-initialized runtime
    start = time.perf_counter()
    preinit.save(runtime_path)
    save_time = time.perf_counter() - start
    print(f"  Saved runtime to {runtime_path} in {save_time * 1000:.1f}ms")
    print(f"  File size: {runtime_path.stat().st_size / 1_000_000:.1f} MB")

    # Load the runtime (simulating a new process)
    start = time.perf_counter()
    # Note: We need to provide site_packages when loading since the runtime
    # doesn't embed the Python files, only the native code state
    loaded_preinit = eryx.PreInitializedRuntime.load(runtime_path)
    load_time = time.perf_counter() - start
    print(f"  Loaded runtime in {load_time * 1000:.1f}ms")

    # Create sandbox from loaded runtime
    # Note: For jinja2 to work, we need site_packages with the Python files
    # In a real app, you'd either:
    # 1. Keep the extracted site-packages around
    # 2. Re-extract from wheels
    # 3. Use a fixed installation path
    print()

    # Example 5: Security - sandbox isolation
    print("--- Example 5: Security - sandbox isolation ---")
    sandbox = preinit.create_sandbox()
    result = sandbox.execute("""
import os

# Try to access the host filesystem (should fail or be isolated)
try:
    files = os.listdir("/etc")
    print(f"Files in /etc: {files[:3]}...")
except Exception as e:
    print(f"Filesystem access blocked: {type(e).__name__}")

# Try to access environment variables (should be empty/isolated)
home = os.environ.get("HOME", "not set")
print(f"HOME env var: {home}")
""")
    print(f"  Output:\n{result.stdout}")

    print("=== Summary ===")
    print(f"  Pre-initialization time: {preinit_time:.2f}s (one-time cost)")
    print(f"  Average sandbox+execute: {avg_time * 1000:.1f}ms")
    print(f"  Speedup: ~{700 / (avg_time * 1000):.0f}x faster than without pre-init")
    print()
    print("  Key benefits:")
    print("    - Pre-import expensive modules (jinja2) once")
    print("    - Create sandboxes in ~10-20ms instead of ~700ms")
    print("    - Save/load runtime for fast startup across processes")
    print("    - Full isolation: each sandbox is independent")


def find_wheel(directory: Path, package_name: str) -> Path | None:
    """Find a wheel file for a package in the given directory."""
    for path in directory.iterdir():
        if path.name.lower().startswith(package_name.lower()) and (
            path.suffix == ".whl" or path.name.endswith(".tar.gz")
        ):
            return path
    return None


def print_download_instructions():
    """Print instructions for downloading required packages."""
    print("Required wheel files not found!")
    print()
    print("To run this example, download the required packages:")
    print()
    print("  # Create wheels directory")
    print("  mkdir -p /tmp/wheels")
    print()
    print("  # Download jinja2 (pure Python wheel)")
    print("  pip download --only-binary=:all: --dest /tmp/wheels jinja2")
    print()
    print("  # For markupsafe, you need a WASI-compiled wheel.")
    print("  # Options:")
    print("  #   1. Build from source using componentize-py")
    print("  #   2. Use a pre-built wheel from wasi-wheels project")
    print("  #   3. Copy from a working setup")
    print()
    print("  # Example (if you have a WASI markupsafe wheel):")
    print("  cp /path/to/markupsafe-*-wasi*.whl /tmp/wheels/")


if __name__ == "__main__":
    main()
