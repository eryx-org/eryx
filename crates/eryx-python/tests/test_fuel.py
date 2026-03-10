"""Regression tests for fuel consumption tracking.

Verifies that fuel_consumed is correctly returned from both Sandbox and Session
when max_fuel is configured, and that fuel exhaustion raises the appropriate error.
"""

import eryx
import pytest


class TestSandboxFuel:
    """Tests for fuel tracking via Sandbox."""

    def test_fuel_consumed_returned_with_limit(self):
        """fuel_consumed should be populated when max_fuel is set.

        Sandbox initialization (Python startup in WASM) consumes significant
        fuel, so we use a very high limit here.
        """
        limits = eryx.ResourceLimits(max_fuel=10_000_000_000)
        sandbox = eryx.Sandbox(resource_limits=limits)
        result = sandbox.execute('print("hello")')
        assert result.fuel_consumed is not None
        assert result.fuel_consumed > 0

    def test_fuel_consumed_returned_without_limit(self):
        """fuel_consumed should still be tracked even without a max_fuel limit."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("hello")')
        assert result.fuel_consumed is not None
        assert result.fuel_consumed > 0

    def test_fuel_consumed_scales_with_work(self):
        """More work should consume more fuel."""
        sandbox = eryx.Sandbox()

        result_small = sandbox.execute("x = 1")
        result_large = sandbox.execute("""
total = 0
for i in range(10000):
    total += i
print(total)
""")
        assert result_large.fuel_consumed > result_small.fuel_consumed

    def test_fuel_exhaustion_raises_error(self):
        """Exceeding the fuel limit should raise an error."""
        limits = eryx.ResourceLimits(max_fuel=1000)
        sandbox = eryx.Sandbox(resource_limits=limits)

        with pytest.raises(eryx.EryxError):
            sandbox.execute("x = sum(range(10000))")

    def test_fuel_consumed_in_repr(self):
        """fuel_consumed should appear in the repr output."""
        sandbox = eryx.Sandbox()
        result = sandbox.execute('print("hi")')
        assert "fuel_consumed" in repr(result)


class TestSessionFuel:
    """Tests for fuel tracking via Session."""

    def test_fuel_consumed_returned(self):
        """Session.execute should return fuel_consumed."""
        session = eryx.Session(max_fuel=100_000_000)
        result = session.execute('print("hello")')
        assert result.fuel_consumed is not None
        assert result.fuel_consumed > 0

    def test_fuel_consumed_without_limit(self):
        """Session without max_fuel should still track fuel."""
        session = eryx.Session()
        result = session.execute('print("hello")')
        assert result.fuel_consumed is not None
        assert result.fuel_consumed > 0

    def test_fuel_limit_getter_setter(self):
        """Session.fuel_limit should be gettable and settable."""
        session = eryx.Session()
        assert session.fuel_limit is None

        session.fuel_limit = 50_000_000
        assert session.fuel_limit == 50_000_000

        session.fuel_limit = None
        assert session.fuel_limit is None

    def test_fuel_limit_via_constructor(self):
        """max_fuel passed to constructor should be accessible via fuel_limit."""
        session = eryx.Session(max_fuel=42_000_000)
        assert session.fuel_limit == 42_000_000

    def test_fuel_exhaustion_in_session(self):
        """Exceeding fuel limit in a session should raise ResourceLimitError."""
        session = eryx.Session(max_fuel=1000)
        with pytest.raises(eryx.ResourceLimitError):
            session.execute("x = sum(range(10000))")

    def test_fuel_consumed_scales_with_work_session(self):
        """More work in a session should consume more fuel."""
        session = eryx.Session()

        result_small = session.execute("x = 1")
        result_large = session.execute("""
total = 0
for i in range(10000):
    total += i
""")
        assert result_large.fuel_consumed > result_small.fuel_consumed
