"""Tests for SandboxFactory setup_code parameter."""

import eryx
import pytest


@pytest.fixture(scope="module")
def setup_factory():
    """Factory with setup_code that defines variables and functions."""
    return eryx.SandboxFactory(
        setup_code=(
            "setup_value = 42\n"
            "setup_list = [1, 2, 3]\n"
            "def setup_add(a, b): return a + b\n"
        ),
    )


class TestSetupCodeBasic:
    def test_variable_available(self, setup_factory):
        sandbox = setup_factory.create_sandbox()
        result = sandbox.execute("print(setup_value)")
        assert result.stdout.strip() == "42"

    def test_list_available(self, setup_factory):
        sandbox = setup_factory.create_sandbox()
        result = sandbox.execute("print(setup_list)")
        assert result.stdout.strip() == "[1, 2, 3]"

    def test_function_available(self, setup_factory):
        sandbox = setup_factory.create_sandbox()
        result = sandbox.execute("print(setup_add(3, 4))")
        assert result.stdout.strip() == "7"


class TestSetupCodeWithImports:
    def test_setup_uses_imported_module(self):
        factory = eryx.SandboxFactory(
            imports=["json"],
            setup_code="precomputed = json.dumps({'ready': True})",
        )
        sandbox = factory.create_sandbox()
        result = sandbox.execute("print(precomputed)")
        assert result.stdout.strip() == '{"ready": true}'


class TestSetupCodeIsolation:
    def test_mutation_does_not_leak(self, setup_factory):
        # First sandbox: mutate the setup variable
        sb1 = setup_factory.create_sandbox()
        sb1.execute("setup_value = 999")

        # Second sandbox: should see original value
        sb2 = setup_factory.create_sandbox()
        result = sb2.execute("print(setup_value)")
        assert result.stdout.strip() == "42"

    def test_list_mutation_does_not_leak(self, setup_factory):
        sb1 = setup_factory.create_sandbox()
        sb1.execute("setup_list.append(999)")

        sb2 = setup_factory.create_sandbox()
        result = sb2.execute("print(len(setup_list))")
        assert result.stdout.strip() == "3"


class TestSetupCodeErrors:
    def test_syntax_error_raises(self):
        with pytest.raises(eryx.InitializationError):
            eryx.SandboxFactory(setup_code="def broken(")

    def test_runtime_error_raises(self):
        with pytest.raises(eryx.InitializationError):
            eryx.SandboxFactory(setup_code="raise ValueError('setup failed')")

    def test_import_error_in_setup_raises(self):
        with pytest.raises(eryx.InitializationError):
            eryx.SandboxFactory(setup_code="import nonexistent_module_xyz")


class TestSetupCodeSaveLoad:
    def test_setup_state_survives_save_load(self, setup_factory, tmp_path):
        path = tmp_path / "factory.bin"
        setup_factory.save(str(path))

        loaded = eryx.SandboxFactory.load(str(path))
        sandbox = loaded.create_sandbox()
        result = sandbox.execute("print(setup_value)")
        assert result.stdout.strip() == "42"
