"""Tests for SandboxFactory data file persistence across save/load."""

import eryx
import pytest


def _stdout_str(result):
    """Get stdout as a string, handling both str and bytes return types."""
    stdout = result.stdout
    if isinstance(stdout, bytes):
        return stdout.decode("utf-8")
    return stdout


@pytest.fixture
def site_packages_with_data(tmp_path):
    """Create a site-packages directory with a package that has data files."""
    pkg_dir = tmp_path / "site_packages" / "mypkg"
    pkg_dir.mkdir(parents=True)
    (pkg_dir / "__init__.py").write_text("DATA_DIR = __path__[0]\n")
    data_dir = pkg_dir / "data"
    data_dir.mkdir()
    (data_dir / "config.json").write_text('{"key": "value"}')
    (data_dir / "binary.dat").write_bytes(b"\x00\x01\x02\x03")
    return tmp_path / "site_packages"


class TestDataFileSaveLoad:
    def test_data_files_survive_round_trip(self, site_packages_with_data, tmp_path):
        factory = eryx.SandboxFactory(
            site_packages=str(site_packages_with_data),
            imports=["mypkg"],
        )

        path = tmp_path / "factory.bin"
        factory.save(str(path))

        loaded = eryx.SandboxFactory.load(str(path))
        sandbox = loaded.create_sandbox()
        result = sandbox.execute(
            "import mypkg, os\n"
            "data_path = os.path.join(mypkg.DATA_DIR, 'data', 'config.json')\n"
            "print(open(data_path).read())\n"
        )
        assert _stdout_str(result).strip() == '{"key": "value"}'

    def test_binary_data_files_survive_round_trip(
        self, site_packages_with_data, tmp_path
    ):
        factory = eryx.SandboxFactory(
            site_packages=str(site_packages_with_data),
            imports=["mypkg"],
        )

        path = tmp_path / "factory.bin"
        factory.save(str(path))

        loaded = eryx.SandboxFactory.load(str(path))
        sandbox = loaded.create_sandbox()
        result = sandbox.execute(
            "import mypkg, os\n"
            "data_path = os.path.join(mypkg.DATA_DIR, 'data', 'binary.dat')\n"
            "data = open(data_path, 'rb').read()\n"
            "print(list(data))\n"
        )
        assert _stdout_str(result).strip() == "[0, 1, 2, 3]"

    def test_v1_format_still_loads(self, tmp_path):
        """Old factory files (raw precompiled bytes, no envelope) must still load."""
        factory = eryx.SandboxFactory()
        sandbox = factory.create_sandbox()
        result = sandbox.execute("print(1 + 1)")
        assert _stdout_str(result).strip() == "2"

        path = tmp_path / "factory.bin"
        factory.save(str(path))

        loaded = eryx.SandboxFactory.load(str(path))
        sandbox2 = loaded.create_sandbox()
        result2 = sandbox2.execute("print(2 + 2)")
        assert _stdout_str(result2).strip() == "4"

    def test_no_data_files_factory(self, tmp_path):
        """A factory with no packages still saves/loads correctly."""
        factory = eryx.SandboxFactory()

        path = tmp_path / "factory.bin"
        factory.save(str(path))

        loaded = eryx.SandboxFactory.load(str(path))
        sandbox = loaded.create_sandbox()
        result = sandbox.execute("print('hello')")
        assert _stdout_str(result).strip() == "hello"

    def test_site_packages_override_on_load(
        self, site_packages_with_data, tmp_path
    ):
        """Explicit site_packages on load() overrides embedded data files."""
        factory = eryx.SandboxFactory(
            site_packages=str(site_packages_with_data),
            imports=["mypkg"],
        )

        path = tmp_path / "factory.bin"
        factory.save(str(path))

        # Create a different site-packages with different data
        alt_dir = tmp_path / "alt_site_packages" / "mypkg"
        alt_dir.mkdir(parents=True)
        (alt_dir / "__init__.py").write_text("DATA_DIR = __path__[0]\n")
        data_dir = alt_dir / "data"
        data_dir.mkdir()
        (data_dir / "config.json").write_text('{"key": "override"}')

        loaded = eryx.SandboxFactory.load(
            str(path),
            site_packages=str(tmp_path / "alt_site_packages"),
        )
        sandbox = loaded.create_sandbox()
        result = sandbox.execute(
            "import mypkg, os\n"
            "data_path = os.path.join(mypkg.DATA_DIR, 'data', 'config.json')\n"
            "print(open(data_path).read())\n"
        )
        assert _stdout_str(result).strip() == '{"key": "override"}'
