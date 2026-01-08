"""Tests for the eryx Python bindings."""

import eryx
import pytest


class TestSandbox:
    """Tests for the Sandbox class."""

    def test_create_sandbox(self):
        """Test that a sandbox can be created."""
        sandbox = eryx.Sandbox()
        assert sandbox is not None

    def test_simple_execution(self):
        """Test simple code execution."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("hello")')
        assert result.stdout == "hello"

    def test_execute_returns_result(self):
        """Test that execute returns an ExecuteResult."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("test")')
        assert isinstance(result, eryx.ExecuteResult)
        assert hasattr(result, "stdout")
        assert hasattr(result, "duration_ms")
        assert hasattr(result, "callback_invocations")
        assert hasattr(result, "peak_memory_bytes")

    def test_duration_is_positive(self):
        """Test that execution duration is tracked."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("x = 1 + 1")
        assert result.duration_ms > 0

    def test_multiple_prints(self):
        """Test multiple print statements."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
print("line 1")
print("line 2")
print("line 3")
""")
        assert result.stdout == "line 1\nline 2\nline 3"

    def test_arithmetic(self):
        """Test arithmetic operations."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
x = 2 + 3
y = x * 4
print(f"{x}, {y}")
""")
        assert result.stdout == "5, 20"

    def test_data_structures(self):
        """Test Python data structures work in sandbox."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
lst = [1, 2, 3]
dct = {"a": 1, "b": 2}
print(f"list: {lst}")
print(f"dict: {dct}")
""")
        assert "list: [1, 2, 3]" in result.stdout
        assert "dict: {'a': 1, 'b': 2}" in result.stdout

    def test_sandbox_isolation(self):
        """Test that sandbox is isolated from host filesystem."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("""
import os
try:
    # Try to access host filesystem
    os.listdir("/etc")
    print("accessed")
except Exception as e:
    print(f"blocked: {type(e).__name__}")
""")
        # Should either fail or show an empty/virtual filesystem
        assert "blocked" in result.stdout or "accessed" not in result.stdout

    def test_sandbox_reuse(self):
        """Test that a sandbox can be reused for multiple executions."""
        sandbox = eryx.Sandbox()

        result1 = sandbox.execute('print("first")')
        assert result1.stdout == "first"

        result2 = sandbox.execute('print("second")')
        assert result2.stdout == "second"


class TestResourceLimits:
    """Tests for ResourceLimits configuration."""

    def test_default_limits(self):
        """Test default resource limits."""
        limits = eryx.ResourceLimits()
        assert limits.execution_timeout_ms == 30000
        assert limits.callback_timeout_ms == 10000
        assert limits.max_memory_bytes == 134217728  # 128 MB
        assert limits.max_callback_invocations == 1000

    def test_custom_limits(self):
        """Test custom resource limits."""
        limits = eryx.ResourceLimits(
            execution_timeout_ms=5000,
            max_memory_bytes=50_000_000,
        )
        assert limits.execution_timeout_ms == 5000
        assert limits.max_memory_bytes == 50_000_000

    def test_unlimited(self):
        """Test unlimited resource limits."""
        limits = eryx.ResourceLimits.unlimited()
        assert limits.execution_timeout_ms is None
        assert limits.callback_timeout_ms is None
        assert limits.max_memory_bytes is None
        assert limits.max_callback_invocations is None

    def test_sandbox_with_limits(self):
        """Test creating sandbox with resource limits."""
        limits = eryx.ResourceLimits(execution_timeout_ms=10000)
        sandbox = eryx.Sandbox(resource_limits=limits)
        result = sandbox.execute('print("ok")')
        assert result.stdout == "ok"

    def test_execution_timeout(self):
        """Test that execution timeout works."""
        limits = eryx.ResourceLimits(execution_timeout_ms=100)
        sandbox = eryx.Sandbox(resource_limits=limits)

        with pytest.raises(eryx.TimeoutError):
            sandbox.execute("while True: pass")


class TestExceptions:
    """Tests for exception handling."""

    def test_execution_error_on_exception(self):
        """Test that Python exceptions become ExecutionError."""
        sandbox = eryx.Sandbox()
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute("raise ValueError('test error')")

    def test_execution_error_on_syntax_error(self):
        """Test that syntax errors become ExecutionError."""
        sandbox = eryx.Sandbox()
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute("def broken(")

    def test_execution_error_on_import_error(self):
        """Test that import errors become ExecutionError."""
        sandbox = eryx.Sandbox()
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute("import nonexistent_module_xyz")

    def test_eryx_error_is_base_class(self):
        """Test that all eryx exceptions inherit from EryxError."""
        sandbox = eryx.Sandbox()
        with pytest.raises(eryx.EryxError):
            sandbox.execute("raise RuntimeError('test')")

    def test_timeout_error_is_catchable_as_builtin(self):
        """Test that TimeoutError can be caught as Python's TimeoutError."""
        limits = eryx.ResourceLimits(execution_timeout_ms=100)
        sandbox = eryx.Sandbox(resource_limits=limits)

        with pytest.raises(TimeoutError):  # Built-in TimeoutError
            sandbox.execute("while True: pass")


class TestExecuteResult:
    """Tests for ExecuteResult class."""

    def test_result_str_returns_stdout(self):
        """Test that str(result) returns stdout."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("test output")')
        assert str(result) == "test output"

    def test_result_repr(self):
        """Test that repr(result) is informative."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("x")')
        repr_str = repr(result)
        assert "ExecuteResult" in repr_str
        assert "stdout" in repr_str

    def test_callback_invocations_zero_without_callbacks(self):
        """Test that callback_invocations is 0 when no callbacks used."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("x = 1")
        assert result.callback_invocations == 0

    def test_peak_memory_bytes_is_present(self):
        """Test that peak memory usage is tracked."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute("x = [i for i in range(1000)]")
        assert result.peak_memory_bytes is not None
        assert result.peak_memory_bytes > 0


class TestModuleMetadata:
    """Tests for module-level metadata."""

    def test_version_is_string(self):
        """Test that __version__ is a string."""
        assert isinstance(eryx.__version__, str)

    def test_version_format(self):
        """Test that version follows semver-ish format."""
        parts = eryx.__version__.split(".")
        assert len(parts) >= 2
        assert all(p.isdigit() for p in parts[:2])

    def test_all_exports_exist(self):
        """Test that all __all__ exports are accessible."""
        for name in eryx.__all__:
            assert hasattr(eryx, name), f"Missing export: {name}"
