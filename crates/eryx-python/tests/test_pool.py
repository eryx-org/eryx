"""Tests for the SandboxPool Python bindings."""

import threading
import time

import eryx
import pytest


class TestPoolCreation:
    def test_factory_create_pool(self):
        """Factory creates a pool with default config."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=2, min_idle=1)
        stats = pool.stats()
        assert stats.total == 1
        assert stats.idle == 1
        pool.close()

    def test_pool_custom_config(self):
        """Pool respects custom config."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=3, min_idle=2)
        stats = pool.stats()
        assert stats.total == 2
        pool.close()

    def test_pool_invalid_config(self):
        """min_idle > max_size raises."""
        factory = eryx.SandboxFactory(cache=True)
        with pytest.raises(eryx.InitializationError):
            factory.create_pool(max_size=1, min_idle=5)


class TestPoolAcquireRelease:
    def test_basic_acquire_execute_release(self):
        """Acquire, execute, release via context manager."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=2, min_idle=1)
        with pool.acquire() as sandbox:
            result = sandbox.execute('print("hello")')
            assert result.stdout_text.strip() == "hello"
        pool.close()

    def test_pool_context_manager(self):
        """Pool as context manager calls close."""
        factory = eryx.SandboxFactory(cache=True)
        with factory.create_pool(max_size=2, min_idle=1) as pool:
            with pool.acquire() as sandbox:
                result = sandbox.execute('print(42)')
                assert result.stdout_text.strip() == "42"
        assert pool.is_closed

    def test_exclusive_leases(self):
        """Two threads get different sandboxes, execute concurrently."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=2, min_idle=2)
        results = [None, None]

        def worker(idx):
            with pool.acquire() as sandbox:
                r = sandbox.execute(f"print({idx})")
                results[idx] = r.stdout_text.strip()

        t1 = threading.Thread(target=worker, args=(0,))
        t2 = threading.Thread(target=worker, args=(1,))
        t1.start()
        t2.start()
        t1.join(timeout=30)
        t2.join(timeout=30)
        assert results[0] == "0"
        assert results[1] == "1"
        pool.close()

    def test_pool_exhaustion_timeout(self):
        """max_size=1, hold one lease, second acquire times out."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=1, min_idle=1, acquire_timeout_ms=200)
        with pool.acquire() as _sandbox:
            with pytest.raises(eryx.PoolTimeoutError):
                pool.acquire()
        pool.close()

    def test_try_acquire_when_full(self):
        """try_acquire returns None when all sandboxes in use."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=1, min_idle=1)
        with pool.acquire() as _sandbox:
            result = pool.try_acquire()
            assert result is None
        pool.close()

    def test_try_acquire_when_available(self):
        """try_acquire returns PooledSandbox."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=2, min_idle=1)
        sandbox = pool.try_acquire()
        assert sandbox is not None
        with sandbox:
            result = sandbox.execute('print("try")')
            assert result.stdout_text.strip() == "try"
        pool.close()


class TestPoolLifecycle:
    def test_close_prevents_acquire(self):
        """After close(), acquire raises PoolClosedError."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=2, min_idle=1)
        pool.close()
        with pytest.raises(eryx.PoolClosedError):
            pool.acquire()

    def test_close_wakes_blocked_acquires(self):
        """Thread blocked on acquire is woken with PoolClosedError."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=1, min_idle=1, acquire_timeout_ms=30_000)
        errors = []

        with pool.acquire() as _sandbox:

            def blocked_worker():
                try:
                    pool.acquire()
                except eryx.PoolClosedError:
                    errors.append("closed")
                except eryx.PoolTimeoutError:
                    errors.append("timeout")

            t = threading.Thread(target=blocked_worker)
            t.start()
            time.sleep(0.1)
            pool.close()
            t.join(timeout=5)

        assert "closed" in errors

    def test_use_after_release(self):
        """execute() after release raises ValueError."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=2, min_idle=1)
        with pool.acquire() as sandbox:
            sandbox.execute("pass")
        with pytest.raises(ValueError, match="released"):
            sandbox.execute("pass")
        pool.close()

    def test_repeated_release(self):
        """release() is idempotent."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=2, min_idle=1)
        with pool.acquire() as sandbox:
            pass
        sandbox.release()
        pool.close()

    def test_evict_idle(self):
        """evict_idle removes excess sandboxes beyond min_idle."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=3, min_idle=1, idle_timeout_ms=10)
        s1 = pool.acquire()
        s2 = pool.acquire()
        s1.release()
        s2.release()
        assert pool.stats().idle == 2
        time.sleep(0.05)
        evicted = pool.evict_idle()
        assert evicted >= 1
        assert pool.stats().idle <= 1
        pool.close()


class TestPoolIsolation:
    def test_fresh_state_across_leases(self):
        """Globals from one lease don't leak to next (stateless execute)."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=1, min_idle=1)
        with pool.acquire() as sandbox:
            sandbox.execute("x = 42")
        with pool.acquire() as sandbox:
            # x should not be defined — each execute() creates a fresh Store
            with pytest.raises(eryx.ExecutionError, match="NameError"):
                sandbox.execute("print(x)")
        pool.close()

    def test_per_request_callbacks(self):
        """Each lease gets its own callbacks."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=1, min_idle=1)

        def cb1():
            return {"from": "cb1"}

        def cb2():
            return {"from": "cb2"}

        callbacks1 = [{"name": "get_source", "fn": cb1, "description": "source 1"}]
        callbacks2 = [{"name": "get_source", "fn": cb2, "description": "source 2"}]

        with pool.acquire(callbacks=callbacks1) as sandbox:
            result = sandbox.execute(
                'import json; r = await get_source(); print(json.dumps(r))'
            )
            assert '"cb1"' in result.stdout_text

        with pool.acquire(callbacks=callbacks2) as sandbox:
            result = sandbox.execute(
                'import json; r = await get_source(); print(json.dumps(r))'
            )
            assert '"cb2"' in result.stdout_text

        pool.close()

    def test_per_request_output_handlers(self):
        """on_stdout/on_stderr are per-lease, don't leak."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=1, min_idle=1)
        chunks1 = []
        chunks2 = []

        with pool.acquire(on_stdout=lambda c: chunks1.append(c)) as sandbox:
            sandbox.execute('print("first")')

        with pool.acquire(on_stdout=lambda c: chunks2.append(c)) as sandbox:
            sandbox.execute('print("second")')

        text1 = b"".join(chunks1).decode()
        text2 = b"".join(chunks2).decode()
        assert "first" in text1
        assert "second" not in text1
        assert "second" in text2
        assert "first" not in text2
        pool.close()


class TestPoolStats:
    def test_stats_tracking(self):
        """Stats reflect acquisitions and creations."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=3, min_idle=1)

        stats = pool.stats()
        assert stats.total_creations == 1

        with pool.acquire() as sandbox:
            sandbox.execute("pass")

        stats = pool.stats()
        assert stats.total_acquisitions == 1
        assert stats.in_use == 0
        assert stats.idle == 1
        pool.close()

    def test_idle_count(self):
        """idle reflects actual queue length."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=3, min_idle=1)
        assert pool.stats().idle == 1

        s1 = pool.acquire()
        assert pool.stats().idle == 0

        s2 = pool.acquire()
        assert pool.stats().idle == 0

        s1.release()
        assert pool.stats().idle == 1

        s2.release()
        assert pool.stats().idle == 2
        pool.close()


class TestPoolGIL:
    def test_other_threads_progress_during_acquire(self):
        """GIL is released during pool acquire wait."""
        factory = eryx.SandboxFactory(cache=True)
        pool = factory.create_pool(max_size=1, min_idle=1, acquire_timeout_ms=5000)
        progress = {"count": 0}

        with pool.acquire() as _sandbox:

            def counter():
                while progress["count"] < 100:
                    progress["count"] += 1
                    time.sleep(0.001)

            def blocked_acquire():
                try:
                    pool.acquire()
                except (eryx.PoolTimeoutError, eryx.PoolClosedError):
                    pass

            counter_thread = threading.Thread(target=counter)
            acquire_thread = threading.Thread(target=blocked_acquire)

            counter_thread.start()
            acquire_thread.start()

            time.sleep(0.3)
            pool.close()

            counter_thread.join(timeout=5)
            acquire_thread.join(timeout=5)

        assert progress["count"] > 10, (
            "Counter thread should have made progress while acquire blocked"
        )
