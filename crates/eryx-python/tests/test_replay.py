"""Tests for callback replay and suspension."""

import json

import pytest

import eryx

CODE = """
a = await fetch(q="a")
b = await fetch(q="b")
print(a["v"], b["v"])
"""


def counting_fetch():
    calls = []

    def fetch(q: str):
        calls.append(q)
        return {"v": q.upper()}

    return fetch, calls


class TestReplay:
    def test_record_then_replay(self):
        fetch, calls = counting_fetch()
        cbs = [{"name": "fetch", "fn": fetch}]

        first = eryx.Sandbox(callbacks=cbs).execute_with_journal(CODE)
        assert first.error is None
        assert first.result.stdout_text == "A B\n"
        assert first.replayed_callbacks == 0
        assert len(first.journal["entries"]) == 2
        assert calls == ["a", "b"]

        # The journal survives a JSON round trip.
        journal = json.loads(json.dumps(first.journal))

        second = eryx.Sandbox(callbacks=cbs).execute_with_journal(CODE, journal=journal)
        assert second.error is None
        assert second.result.stdout_text == "A B\n"
        assert second.replayed_callbacks == 2
        assert calls == ["a", "b"], "replayed callbacks must not run live"

    def test_journal_present_on_error(self):
        fetch, _ = counting_fetch()
        sandbox = eryx.Sandbox(callbacks=[{"name": "fetch", "fn": fetch}])
        outcome = sandbox.execute_with_journal(CODE + "\nraise ValueError('late')")
        assert outcome.result is None
        assert isinstance(outcome.error, eryx.ExecutionError)
        assert len(outcome.journal["entries"]) == 2

    def test_one_sandbox_records_and_replays(self):
        fetch, calls = counting_fetch()
        sandbox = eryx.Sandbox(callbacks=[{"name": "fetch", "fn": fetch}])
        journal = sandbox.execute_with_journal(CODE).journal

        assert sandbox.execute_with_journal(CODE, journal).replayed_callbacks == 2
        assert sandbox.execute_with_journal(CODE).replayed_callbacks == 0
        assert calls == ["a", "b", "a", "b"]

    def test_session_replays_and_keeps_state(self):
        fetch, calls = counting_fetch()
        session = eryx.Session(callbacks=[{"name": "fetch", "fn": fetch}])
        journal = session.execute_with_journal(CODE).journal

        second = session.execute_with_journal(CODE, journal=journal)
        assert second.error is None
        assert second.replayed_callbacks == 2
        assert calls == ["a", "b"]
        assert session.execute("print(a['v'])").stdout_text == "A\n"

    def test_pooled_sandbox_replays(self):
        fetch, calls = counting_fetch()
        pool = eryx.SandboxFactory(cache=True).create_pool(max_size=1, min_idle=0)
        try:
            with pool.acquire(callbacks=[{"name": "fetch", "fn": fetch}]) as sandbox:
                journal = sandbox.execute_with_journal(CODE).journal
                assert sandbox.execute_with_journal(CODE, journal).replayed_callbacks == 2
            assert calls == ["a", "b"]
        finally:
            pool.close()

    def test_invalid_journal_raises(self):
        with pytest.raises(ValueError, match="Invalid journal"):
            eryx.Sandbox().execute_with_journal(CODE, journal={"nope": 1})


SUSPEND_CODE = """
a = await fetch(q="a")
ok = await approve(item="x")
print("after", a["v"], ok)
"""


class TestSuspend:
    def _run(self, approve, journal=None):
        fetch, calls = counting_fetch()
        sandbox = eryx.Sandbox(
            callbacks=[{"name": "fetch", "fn": fetch}, {"name": "approve", "fn": approve}],
        )
        return sandbox.execute_with_journal(SUSPEND_CODE, journal), calls

    def test_sync_suspend_then_resume(self):
        def pending(item: str):
            raise eryx.SuspendCallback(f"awaiting approval for {item}")

        outcome, calls = self._run(pending)
        assert outcome.suspended is not None
        assert outcome.suspended.name == "approve"
        assert json.loads(outcome.suspended.args_json) == {"item": "x"}
        assert outcome.suspended.reason == "awaiting approval for x"
        assert outcome.result is None
        assert isinstance(outcome.error, eryx.ExecutionError)
        # Only the completed prefix is journaled, not the suspended call.
        assert [e["name"] for e in outcome.journal["entries"]] == ["fetch"]
        assert calls == ["a"]

        approved = []

        def granted(item: str):
            approved.append(item)
            return True

        resumed, calls = self._run(granted, journal=outcome.journal)
        assert resumed.error is None
        assert resumed.suspended is None
        assert resumed.result.stdout_text == "after A True\n"
        assert resumed.replayed_callbacks == 1
        assert calls == [], "prefix replays from the journal"
        assert approved == ["x"], "suspended call runs live on resume"

    def test_async_suspend(self):
        async def pending(item: str):
            raise eryx.SuspendCallback("later")

        outcome, _ = self._run(pending)
        assert outcome.suspended is not None
        assert outcome.suspended.reason == "later"
        assert outcome.result is None

    def test_session_suspend_then_resume_in_same_session(self):
        fetch, calls = counting_fetch()
        approved = []

        def approve(item: str):
            if not approved:
                approved.append(item)
                raise eryx.SuspendCallback("later")
            return True

        session = eryx.Session(
            callbacks=[{"name": "fetch", "fn": fetch}, {"name": "approve", "fn": approve}]
        )
        session.execute("n = 0")
        code = "n += 1\n" + SUSPEND_CODE + "print('n', n)\n"

        first = session.execute_with_journal(code)
        assert first.suspended is not None

        resumed = session.execute_with_journal(code, journal=first.journal)
        assert resumed.error is None, resumed.error
        assert resumed.replayed_callbacks == 1
        assert calls == ["a"], "prefix replays from the journal"
        assert resumed.result.stdout_text == "after A True\nn 1\n"

    def test_session_timeout_rolls_back(self):
        fetch, _ = counting_fetch()
        session = eryx.Session(callbacks=[{"name": "fetch", "fn": fetch}])
        session.execute("n = 0")
        session.execution_timeout_ms = 500
        outcome = session.execute_with_journal('n += 1\nawait fetch(q="a")\nwhile True: pass')
        assert isinstance(outcome.error, eryx.TimeoutError)
        assert session.execute("print(n)").stdout_text == "0\n"
