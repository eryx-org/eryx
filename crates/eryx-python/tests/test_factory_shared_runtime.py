"""Tests for SandboxFactory sharing a single Tokio runtime across children."""

import concurrent.futures
import gc

import eryx
import pytest


def add(a, b):
    return {"sum": a + b}


CALLBACKS = [
    {"name": "add", "fn": add, "description": "Adds two numbers"},
]


@pytest.fixture(scope="module")
def factory():
    return eryx.SandboxFactory(callbacks=CALLBACKS)


class TestSharedRuntime:
    def test_multiple_sandboxes_execute_sequentially(self, factory):
        """Sandboxes created from the same factory execute correctly."""
        for i in range(5):
            sandbox = factory.create_sandbox()
            result = sandbox.execute(f"print({i} * {i})")
            assert result.stdout.strip() == str(i * i).encode()

    def test_concurrent_sandboxes(self, factory):
        """Multiple sandboxes execute concurrently on the shared runtime."""
        sandboxes = [factory.create_sandbox() for _ in range(4)]

        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            futures = [
                pool.submit(s.execute, f"print({i} + 1)")
                for i, s in enumerate(sandboxes)
            ]
            results = [f.result() for f in futures]

        for i, result in enumerate(results):
            assert result.stdout.strip() == str(i + 1).encode()

    def test_concurrent_sandboxes_with_callbacks(self, factory):
        """Callbacks route correctly when sandboxes share a runtime."""
        sandboxes = [factory.create_sandbox() for _ in range(4)]

        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            futures = [
                pool.submit(s.execute, f"print((await add(a={i}, b=10))['sum'])")
                for i, s in enumerate(sandboxes)
            ]
            results = [f.result() for f in futures]

        for i, result in enumerate(results):
            assert result.stdout.strip() == str(i + 10).encode()

    def test_factory_dropped_children_survive(self):
        """Sandboxes and sessions remain usable after the factory is dropped."""
        local_factory = eryx.SandboxFactory(callbacks=CALLBACKS)
        sandbox = local_factory.create_sandbox()
        session = local_factory.create_session()

        # Drop the factory
        del local_factory
        gc.collect()

        # Children should still work — the Arc<Runtime> keeps it alive.
        result = sandbox.execute("print(42)")
        assert result.stdout.strip() == b"42"

        result = session.execute("print(99)")
        assert result.stdout.strip() == b"99"

    def test_session_shares_runtime_with_sandbox(self, factory):
        """Sessions created from a factory also share the runtime."""
        session = factory.create_session()
        sandbox = factory.create_sandbox()

        r1 = session.execute("print('session')")
        r2 = sandbox.execute("print('sandbox')")
        assert r1.stdout.strip() == b"session"
        assert r2.stdout.strip() == b"sandbox"

    def test_loaded_factory_shares_runtime(self, factory, tmp_path):
        """A factory loaded from disk also shares a runtime across children."""
        path = tmp_path / "factory.bin"
        factory.save(path)

        loaded = eryx.SandboxFactory.load(path, callbacks=CALLBACKS)
        sandboxes = [loaded.create_sandbox() for _ in range(3)]

        for i, sb in enumerate(sandboxes):
            result = sb.execute(f"print((await add(a={i}, b=1))['sum'])")
            assert result.stdout.strip() == str(i + 1).encode()

    def test_error_recovery_on_shared_runtime(self, factory):
        """A failed execution on one sandbox doesn't poison the shared runtime."""
        s1 = factory.create_sandbox()
        s2 = factory.create_sandbox()

        with pytest.raises(eryx.ExecutionError):
            s1.execute("raise ValueError('boom')")

        result = s2.execute("print('still alive')")
        assert result.stdout.strip() == b"still alive"

    def test_timeout_recovery_on_shared_runtime(self, factory):
        """A timed-out sandbox doesn't break sibling sandboxes."""
        s1 = factory.create_sandbox(
            resource_limits=eryx.ResourceLimits(execution_timeout_ms=100)
        )
        s2 = factory.create_sandbox()

        with pytest.raises(eryx.TimeoutError):
            s1.execute("import time; time.sleep(10)")

        result = s2.execute("print('ok')")
        assert result.stdout.strip() == b"ok"
