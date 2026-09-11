"""Tests for SandboxFactory(callbacks=...): declarations baked into the snapshot."""

import eryx
import pytest


def get_time():
    return {"timestamp": 1234}


def add(a, b):
    return {"sum": a + b}


def ping():
    return "pong"


CALLBACKS = [
    {"name": "get_time", "fn": get_time, "description": "Returns a fixed time"},
    {"name": "add", "fn": add, "description": "Adds two numbers"},
]


@pytest.fixture(scope="module")
def callback_factory():
    """Factory whose snapshot has get_time and add baked in."""
    return eryx.SandboxFactory(callbacks=CALLBACKS)


class TestFactoryCallbacks:
    def test_sandbox_registers_factory_callbacks_by_default(self, callback_factory):
        sandbox = callback_factory.create_sandbox()
        result = sandbox.execute(
            "t = await get_time(); r = await add(a=3, b=4); print(t['timestamp'], r['sum'])"
        )
        assert result.stdout.strip() == "1234 7"

    def test_introspection_lists_baked_callbacks(self, callback_factory):
        sandbox = callback_factory.create_sandbox()
        result = sandbox.execute("print(sorted(c['name'] for c in list_callbacks()))")
        assert result.stdout.strip() == "['add', 'get_time']"

    def test_repeated_sandboxes_keep_working(self, callback_factory):
        for i in range(3):
            sandbox = callback_factory.create_sandbox()
            result = sandbox.execute(f"print((await add(a={i}, b=1))['sum'])")
            assert result.stdout.strip() == str(i + 1)

    def test_explicit_callbacks_override_the_baked_set(self, callback_factory):
        sandbox = callback_factory.create_sandbox(
            callbacks=[{"name": "ping", "fn": ping, "description": ""}]
        )
        result = sandbox.execute(
            "print(sorted(c['name'] for c in list_callbacks()), await ping())"
        )
        assert result.stdout.strip() == "['ping'] pong"

    def test_session_registers_factory_callbacks_by_default(self, callback_factory):
        session = callback_factory.create_session()
        result = session.execute("print((await get_time())['timestamp'])")
        assert result.stdout.strip() == "1234"

    def test_save_and_load_with_callbacks(self, callback_factory, tmp_path):
        path = tmp_path / "factory.bin"
        callback_factory.save(path)

        loaded = eryx.SandboxFactory.load(path, callbacks=CALLBACKS)
        sandbox = loaded.create_sandbox()
        result = sandbox.execute("print((await add(a=20, b=22))['sum'])")
        assert result.stdout.strip() == "42"

        # Loading without callbacks still works; the sandbox just has none.
        bare = eryx.SandboxFactory.load(path)
        result = bare.create_sandbox().execute("print(list_callbacks())")
        assert result.stdout.strip() == "[]"


class TestFactoryCallbacksWithSetupCode:
    def test_setup_code_and_callbacks_together(self):
        factory = eryx.SandboxFactory(
            setup_code="base = 100",
            callbacks=[{"name": "add", "fn": add, "description": "Adds two numbers"}],
        )
        sandbox = factory.create_sandbox()
        result = sandbox.execute("print((await add(a=base, b=1))['sum'])")
        assert result.stdout.strip() == "101"
