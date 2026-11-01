"""Tests for rebuilding factories saved by another pyeryx version.

A saved factory only loads in the pyeryx version that saved it. To stand in
for an old file, these tests rewrite the eryx version recorded in a freshly
saved one. Rebuilding writes to the eryx cache directory, which is resolved
once per process, so those tests run in a child process with
``XDG_CACHE_HOME`` pointing at a temporary directory.
"""

import gc
import json
import os
import re
import subprocess
import sys
import textwrap

import eryx
import pytest

SETUP_CODE = "def greet(name): return 'hello ' + name\n"


@pytest.fixture
def site_packages(tmp_path):
    """A package with a pre-imported module, a lazily imported one and data."""
    pkg = tmp_path / "site_packages" / "mypkg"
    pkg.mkdir(parents=True)
    (pkg / "__init__.py").write_text("VALUE = 41\n")
    (pkg / "extra.py").write_text("def answer(): return 42\n")
    (pkg / "data.json").write_text('{"key": "value"}')
    return tmp_path / "site_packages"


@pytest.fixture
def saved_factory(site_packages, tmp_path):
    """A factory saved by this version, then marked as saved by another one."""
    factory = eryx.SandboxFactory(
        site_packages=str(site_packages),
        imports=["mypkg"],
        setup_code=SETUP_CODE,
    )
    path = tmp_path / "factory.bin"
    factory.save(str(path))
    _forge_other_version(path)
    return path


@pytest.fixture
def cache_home(tmp_path):
    path = tmp_path / "cache"
    path.mkdir(mode=0o700)
    return path


def _forge_other_version(path):
    """Change the eryx version recorded in a saved factory's component.

    wasmtime stores it as a length byte and the string, so the replacement
    keeps the same length.
    """
    data = bytearray(path.read_bytes())
    matches = list(re.finditer(rb"eryx \d[^\x00()]* \(wasmtime \d+\)", data))
    assert len(matches) == 1, "the factory should record one eryx version"
    match = matches[0]
    assert data[match.start() - 1] == len(match.group())
    forged = b"eryx 9" + match.group()[len(b"eryx ") :].replace(b" (", b"(", 1)
    data[match.start() : match.end()] = forged
    path.write_bytes(bytes(data))


def _run(code, cache_home):
    """Run `code` in a fresh interpreter using `cache_home` as XDG_CACHE_HOME,
    and return what it prints as JSON."""
    env = dict(os.environ, XDG_CACHE_HOME=str(cache_home))
    result = subprocess.run(
        [sys.executable, "-c", textwrap.dedent(code)],
        env=env,
        capture_output=True,
        text=True,
        timeout=600,
    )
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout.strip().splitlines()[-1])


REBUILD_AND_RUN = """
    import json, warnings
    import eryx

    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        factory = eryx.SandboxFactory.load({path!r}, rebuild_if_incompatible=True)
    result = factory.create_sandbox().execute(
        "import mypkg, mypkg.extra, os\\n"
        "print(mypkg.VALUE, mypkg.extra.answer(), greet('x'))\\n"
        "print(open(os.path.join(mypkg.__path__[0], 'data.json')).read())\\n"
    )
    print(json.dumps({{
        "stdout": result.stdout.decode(),
        "warnings": [str(w.message) for w in caught if issubclass(w.category, RuntimeWarning)],
    }}))
"""


def _cached_factories(cache_home):
    directory = cache_home / "eryx" / "factories"
    return sorted(directory.glob("*.bin")) if directory.is_dir() else []


class TestIncompatibleFactory:
    def test_load_raises_and_points_at_the_flag(self, saved_factory):
        with pytest.raises(eryx.InitializationError, match="rebuild_if_incompatible=True"):
            eryx.SandboxFactory.load(str(saved_factory))

    def test_factory_without_build_inputs_says_to_recreate_it(self, tmp_path):
        # `to_bytes()` is the bare component: a v1 file, which stores nothing
        # to rebuild from.
        path = tmp_path / "v1.bin"
        path.write_bytes(eryx.SandboxFactory(imports=["json"]).to_bytes())
        _forge_other_version(path)

        with pytest.raises(eryx.InitializationError, match="recreate it"):
            eryx.SandboxFactory.load(str(path), rebuild_if_incompatible=True)


class TestRebuild:
    def test_rebuild_restores_packages_setup_code_and_data_files(
        self, saved_factory, cache_home
    ):
        original = saved_factory.read_bytes()

        out = _run(REBUILD_AND_RUN.format(path=str(saved_factory)), cache_home)

        assert out["stdout"] == '41 42 hello x\n{"key": "value"}\n'
        assert out["warnings"] == []
        # The rebuilt copy goes to the cache; the original is left alone.
        assert saved_factory.read_bytes() == original
        assert len(_cached_factories(cache_home)) == 1

    def test_next_process_reuses_the_cached_copy(self, saved_factory, cache_home):
        _run(REBUILD_AND_RUN.format(path=str(saved_factory)), cache_home)
        [cached] = _cached_factories(cache_home)
        first_write = cached.stat().st_mtime_ns

        out = _run(REBUILD_AND_RUN.format(path=str(saved_factory)), cache_home)

        assert out["stdout"] == '41 42 hello x\n{"key": "value"}\n'
        assert _cached_factories(cache_home) == [cached]
        assert cached.stat().st_mtime_ns == first_write, "the copy was rebuilt again"

    def test_rebuild_warns_when_the_copy_cannot_be_cached(
        self, saved_factory, cache_home
    ):
        # A file where the cache's `factories` directory would go.
        (cache_home / "eryx").mkdir(mode=0o700)
        (cache_home / "eryx" / "factories").write_text("not a directory")

        out = _run(REBUILD_AND_RUN.format(path=str(saved_factory)), cache_home)

        assert out["stdout"] == '41 42 hello x\n{"key": "value"}\n'
        assert len(out["warnings"]) == 1
        assert "couldn't cache the rebuilt copy" in out["warnings"][0]

    def test_rebuilt_factory_saves_its_build_inputs(
        self, saved_factory, cache_home, tmp_path
    ):
        resaved = tmp_path / "resaved.bin"
        code = f"""
            import json, eryx
            factory = eryx.SandboxFactory.load({str(saved_factory)!r},
                                               rebuild_if_incompatible=True)
            factory.save({str(resaved)!r})
            print(json.dumps(True))
        """
        _run(code, cache_home)

        # The re-saved file loads as it is ...
        loaded = eryx.SandboxFactory.load(str(resaved))
        result = loaded.create_sandbox().execute("import mypkg; print(mypkg.VALUE)")
        assert result.stdout == b"41\n"
        # ... and still stores what's needed to rebuild it next time.
        _forge_other_version(resaved)
        # `cache=False`: forging leaves the footer's content hash alone, and this
        # process has already cached a component under it, which a cached load
        # would reuse without looking at the forged file. A file really built by
        # another version has a different hash.
        with pytest.raises(eryx.InitializationError, match="rebuild_if_incompatible=True"):
            eryx.SandboxFactory.load(str(resaved), cache=False)


class TestSaveLoadRoundTrip:
    def test_modules_not_imported_during_creation_survive_save_and_load(
        self, site_packages, tmp_path
    ):
        # Only `mypkg` is in the snapshot; `mypkg.extra` has to come from the
        # saved files.
        factory = eryx.SandboxFactory(site_packages=str(site_packages), imports=["mypkg"])
        path = tmp_path / "factory.bin"
        factory.save(str(path))

        loaded = eryx.SandboxFactory.load(str(path))
        result = loaded.create_sandbox().execute(
            "import mypkg.extra; print(mypkg.extra.answer())"
        )
        assert result.stdout == b"42\n"

    @pytest.mark.parametrize("create", ["create_sandbox", "create_session"])
    def test_children_outlive_a_loaded_factory(self, site_packages, tmp_path, create):
        # A loaded factory extracts its files to a temporary directory, which
        # sandboxes and sessions mount when they execute.
        factory = eryx.SandboxFactory(site_packages=str(site_packages), imports=["mypkg"])
        path = tmp_path / "factory.bin"
        factory.save(str(path))

        child = getattr(eryx.SandboxFactory.load(str(path)), create)()
        gc.collect()
        result = child.execute("import mypkg.extra; print(mypkg.extra.answer())")
        assert result.stdout == b"42\n"

    def test_loaded_factory_resaves_its_build_inputs(self, site_packages, tmp_path):
        factory = eryx.SandboxFactory(site_packages=str(site_packages), imports=["mypkg"])
        first = tmp_path / "first.bin"
        factory.save(str(first))

        second = tmp_path / "second.bin"
        eryx.SandboxFactory.load(str(first)).save(str(second))

        _forge_other_version(second)
        # `cache=False`: forging leaves the footer's content hash alone, and this
        # process has already cached a component under it, which a cached load
        # would reuse without looking at the forged file. A file really built by
        # another version has a different hash.
        with pytest.raises(eryx.InitializationError, match="rebuild_if_incompatible=True"):
            eryx.SandboxFactory.load(str(second), cache=False)


class TestUpgradeCommand:
    """`python -m eryx factory upgrade OLD NEW`."""

    def test_upgrade_saves_a_rebuilt_copy(self, saved_factory, cache_home, tmp_path):
        original = saved_factory.read_bytes()
        new = tmp_path / "upgraded.bin"
        result = subprocess.run(
            [sys.executable, "-m", "eryx", "factory", "upgrade", str(saved_factory), str(new)],
            env=dict(os.environ, XDG_CACHE_HOME=str(cache_home)),
            capture_output=True,
            text=True,
            timeout=600,
        )
        assert result.returncode == 0, result.stderr
        assert result.stdout.strip() == f"Saved {new}"

        # NEW loads in this version as it is, with everything the factory had.
        upgraded = eryx.SandboxFactory.load(str(new))
        out = upgraded.create_sandbox().execute(
            "import mypkg, mypkg.extra, os\n"
            "print(mypkg.VALUE, mypkg.extra.answer(), greet('x'))\n"
            "print(open(os.path.join(mypkg.__path__[0], 'data.json')).read())\n"
        )
        assert out.stdout.decode() == '41 42 hello x\n{"key": "value"}\n'
        assert saved_factory.read_bytes() == original

    def test_upgrade_refuses_to_overwrite_the_old_file(self, saved_factory, capsys):
        from eryx.__main__ import main

        assert main(["factory", "upgrade", str(saved_factory), str(saved_factory)]) == 1
        assert "NEW must differ from OLD" in capsys.readouterr().err

    def test_upgrade_reports_a_factory_that_cannot_be_rebuilt(self, tmp_path, capsys):
        from eryx.__main__ import main

        old = tmp_path / "v1.bin"
        old.write_bytes(eryx.SandboxFactory(imports=["json"]).to_bytes())
        _forge_other_version(old)

        assert main(["factory", "upgrade", str(old), str(tmp_path / "new.bin")]) == 1
        assert "recreate it" in capsys.readouterr().err
        assert not (tmp_path / "new.bin").exists()
