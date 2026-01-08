#!/usr/bin/env python3
"""
Jinja2 Template Sandbox Example

Demonstrates sandboxed Jinja2 template evaluation using eryx.
This shows how to safely evaluate user-supplied Jinja2 templates
inside the WebAssembly sandbox, preventing any access to the host system.

Prerequisites:
    # Download jinja2 (pure Python)
    pip download --only-binary=:all: --dest /tmp/wheels jinja2

    # Get markupsafe WASI wheel (compiled for WebAssembly)
    # See: https://github.com/AustinMCrane/wasi-wheels or similar
    # Copy markupsafe-*-wasi*.whl to /tmp/wheels/

Usage:
    python examples/jinja2_sandbox.py
"""

import json
import time
from pathlib import Path

import eryx


def main():
    print("=== Jinja2 Template Sandbox Example ===\n")

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

    # Create sandbox with jinja2 and markupsafe packages
    # Note: This is slower than using pre-initialization because the packages
    # are loaded fresh each time. For production use, consider pre-initialization.
    print("Creating sandbox with packages...")
    start = time.perf_counter()
    sandbox = eryx.Sandbox(
        packages=[str(jinja2_wheel), str(markupsafe_wheel)],
    )
    sandbox_time = time.perf_counter() - start
    print(f"  Sandbox created in {sandbox_time:.2f}s\n")

    # Example 1: Simple template
    print("--- Example 1: Simple template ---")
    start = time.perf_counter()
    result = sandbox.execute("""
from jinja2 import Template
template = Template("Hello, {{ name }}!")
print(template.render(name="World"))
""")
    exec_time = time.perf_counter() - start
    print(f"  Execute time: {exec_time * 1000:.1f}ms")
    print(f"  Output: {result.stdout}")
    print()

    # Example 2: Template with loops and conditionals
    print("--- Example 2: Loops and conditionals ---")
    result = sandbox.execute("""
from jinja2 import Template

template_str = '''
{% for item in items %}
  - {{ item.name }}: {% if item.active %}ACTIVE{% else %}inactive{% endif %}
{% endfor %}
'''

template = Template(template_str)
output = template.render(items=[
    {"name": "Server A", "active": True},
    {"name": "Server B", "active": False},
    {"name": "Server C", "active": True},
])
print(output)
""")
    print(f"  Output:{result.stdout}")
    print()

    # Example 3: User-supplied template (simulating untrusted input)
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

    # Safely pass the template and data to the sandbox
    code = f"""
import json
from jinja2 import Template

template_str = '''{user_template}'''
data = json.loads('''{json.dumps(user_data)}''')

template = Template(template_str)
output = template.render(**data)
print(output)
"""

    result = sandbox.execute(code)
    print(f"  Output:\n{result.stdout}")
    print()

    # Example 4: Security - sandbox isolation
    print("--- Example 4: Security - sandbox isolation ---")
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
    print()

    # Example 5: Reusing the sandbox for multiple templates
    print("--- Example 5: Fast template rendering (sandbox reuse) ---")
    templates = [
        ("Greeting", "Hello, {{ name }}!", {"name": "Alice"}),
        ("Math", "{{ a }} + {{ b }} = {{ a + b }}", {"a": 5, "b": 3}),
        ("List", "Items: {{ items|join(', ') }}", {"items": ["x", "y", "z"]}),
    ]

    for name, template_str, data in templates:
        code = f"""
from jinja2 import Template
import json
template = Template('''{template_str}''')
data = json.loads('''{json.dumps(data)}''')
print(template.render(**data))
"""
        start = time.perf_counter()
        result = sandbox.execute(code)
        exec_time = (time.perf_counter() - start) * 1000
        print(f"  {name}: {result.stdout.strip()} ({exec_time:.1f}ms)")

    print()
    print("=== Summary ===")
    print(f"  Initial sandbox creation: {sandbox_time:.2f}s (includes late-linking)")
    print("  Subsequent executions: ~450ms each (Python init per execution)")
    print("  Full isolation: each sandbox is independent from host")
    print()
    print("Note: For faster execution, use pre-initialization (Phase 2 feature)")
    print("      which bakes imports into the WASM component.")


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
