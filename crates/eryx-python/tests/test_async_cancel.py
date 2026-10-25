"""Tests for execute_async(), CancelToken and Ctrl-C handling."""

import _thread
import asyncio
import os
import signal
import sys
import threading
import time

import eryx
import pytest

LOOP = "while True: pass"


def cancel_after(token, delay=0.5):
    threading.Timer(delay, token.cancel).start()


def test_sandbox_execute_async():
    async def main():
        return await eryx.Sandbox().execute_async("print(1 + 1)")

    assert asyncio.run(main()).stdout == b"2\n"


def test_execute_async_keeps_event_loop_running():
    sandbox = eryx.Sandbox()

    async def main():
        ticks = 0

        async def ticker():
            nonlocal ticks
            while True:
                await asyncio.sleep(0.01)
                ticks += 1

        task = asyncio.create_task(ticker())
        await sandbox.execute_async("import time\ntime.sleep(0.5)")
        task.cancel()
        return ticks

    assert asyncio.run(main()) > 5


def test_execute_async_propagates_errors():
    async def main():
        await eryx.Sandbox().execute_async("raise ValueError('boom')")

    with pytest.raises(eryx.PythonExceptionError, match="boom"):
        asyncio.run(main())


def test_session_execute_async_serializes_in_call_order():
    session = eryx.Session()
    session.execute("log = []")

    async def main():
        await asyncio.gather(
            *(
                session.execute_async(f"import time\ntime.sleep(0.02)\nlog.append({i})")
                for i in range(5)
            )
        )
        return await session.execute_async("print(log)")

    assert asyncio.run(main()).stdout == b"[0, 1, 2, 3, 4]\n"


@pytest.mark.parametrize("make", [eryx.Sandbox, eryx.Session])
def test_cancel_token_stops_execute(make):
    target = make()
    token = eryx.CancelToken()
    cancel_after(token)
    start = time.monotonic()
    with pytest.raises(eryx.CancelledError):
        target.execute(LOOP, cancel=token)
    assert time.monotonic() - start < 10
    assert token.cancelled


@pytest.mark.parametrize("make", [eryx.Sandbox, eryx.Session])
def test_cancel_token_stops_execute_async(make):
    target = make()
    token = eryx.CancelToken()

    async def main():
        asyncio.get_running_loop().call_later(0.2, token.cancel)
        await target.execute_async(LOOP, cancel=token)

    with pytest.raises(eryx.CancelledError):
        asyncio.run(main())


def test_cancelled_token_cancels_before_start():
    token = eryx.CancelToken()
    token.cancel()
    with pytest.raises(eryx.CancelledError):
        eryx.Sandbox().execute("print('never')", cancel=token)


def test_session_usable_after_cancel():
    session = eryx.Session()
    session.execute("x = 1")
    token = eryx.CancelToken()
    cancel_after(token)
    with pytest.raises(eryx.CancelledError):
        session.execute(LOOP, cancel=token)
    # State is lost, but the session works again.
    result = session.execute("print('x' in globals())")
    assert result.stdout == b"False\n"


def test_cancelling_asyncio_task_frees_session():
    session = eryx.Session()

    async def main():
        task = asyncio.ensure_future(session.execute_async(LOOP))
        await asyncio.sleep(0.2)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        return await asyncio.wait_for(session.execute_async("print('ok')"), 10)

    assert asyncio.run(main()).stdout == b"ok\n"


@pytest.mark.skipif(
    threading.current_thread() is not threading.main_thread(),
    reason="signals are only delivered to the main thread",
)
@pytest.mark.parametrize("make", [eryx.Sandbox, eryx.Session])
def test_ctrl_c_interrupts_execute(make):
    target = make()
    if sys.platform == "win32":
        # os.kill(SIGINT) terminates the process on Windows; simulate the
        # console Ctrl-C handler instead.
        interrupt = (_thread.interrupt_main, (signal.SIGINT,))
    else:
        interrupt = (os.kill, (os.getpid(), signal.SIGINT))
    threading.Timer(0.2, *interrupt).start()
    start = time.monotonic()
    with pytest.raises(KeyboardInterrupt):
        target.execute(LOOP)
    assert time.monotonic() - start < 10
    # The run was stopped, not left running in the background.
    assert target.execute("print('again')").stdout == b"again\n"


def test_execute_async_without_event_loop_runs_nothing():
    calls = []
    sandbox = eryx.Sandbox(
        callbacks=[{"name": "mark", "fn": lambda: calls.append(1), "description": "x"}]
    )
    with pytest.raises(RuntimeError):
        sandbox.execute_async("await mark()")
    session = eryx.Session()
    session.execute("x = 0")
    with pytest.raises(RuntimeError):
        session.execute_async("x = 1")
    time.sleep(0.3)
    assert calls == []
    # Nothing ran and no queue slot was left behind.
    assert session.execute("print(x)").stdout == b"0\n"


@pytest.mark.parametrize("pooled", [False, True])
def test_cancelling_asyncio_task_stops_sandbox_run(pooled):
    if pooled:
        pool = eryx.SandboxFactory(cache=True).create_pool(max_size=1)
        lease = pool.acquire()
    else:
        lease = eryx.Sandbox()

    async def main():
        task = asyncio.ensure_future(lease.execute_async(LOOP))
        await asyncio.sleep(0.2)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

    start = time.monotonic()
    asyncio.run(main())
    assert time.monotonic() - start < 10
    assert lease.execute("print('ok')").stdout == b"ok\n"


def test_snapshot_waits_for_queued_execute_async():
    session = eryx.Session()

    async def main():
        run = session.execute_async("import time\ntime.sleep(0.3)\nx = 1")
        # Called after the execute_async, so it must see its effects.
        snapshot = await asyncio.to_thread(session.snapshot_state)
        await run
        return snapshot

    snapshot = asyncio.run(main())
    fresh = eryx.Session()
    fresh.restore_state(snapshot)
    assert fresh.execute("print(x)").stdout == b"1\n"
