"""Tests for callback replay and suspension."""

import json

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

        second = eryx.Sandbox(callbacks=cbs, replay_journal=journal).execute_with_journal(CODE)
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

    def test_execute_ignores_replay_journal(self):
        fetch, calls = counting_fetch()
        cbs = [{"name": "fetch", "fn": fetch}]
        journal = eryx.Sandbox(callbacks=cbs).execute_with_journal(CODE).journal

        eryx.Sandbox(callbacks=cbs, replay_journal=journal).execute(CODE)
        assert calls == ["a", "b", "a", "b"]


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
            replay_journal=journal,
        )
        return sandbox.execute_with_journal(SUSPEND_CODE), calls

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
