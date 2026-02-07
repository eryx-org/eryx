"""Tests for host filesystem volume mounts."""

import eryx
import pytest


class TestSandboxVolumes:
    """Test volume mounts with the stateless Sandbox."""

    def test_read_file_from_host(self, tmp_path):
        """Sandbox can read a file from a mounted host directory."""
        (tmp_path / "input.txt").write_text("hello from host")
        sandbox = eryx.Sandbox(
            volumes=[(str(tmp_path), "/mnt/data", False)],
        )
        result = sandbox.execute('print(open("/mnt/data/input.txt").read())')
        assert "hello from host" in result.stdout

    def test_write_file_to_host(self, tmp_path):
        """Sandbox can write a file back to the host through a writable mount."""
        sandbox = eryx.Sandbox(
            volumes=[(str(tmp_path), "/mnt/data", False)],
        )
        sandbox.execute(
            'open("/mnt/data/output.txt", "w").write("written from sandbox")'
        )
        assert (tmp_path / "output.txt").read_text() == "written from sandbox"

    def test_read_only_prevents_writes(self, tmp_path):
        """A read-only volume mount should prevent file creation."""
        (tmp_path / "existing.txt").write_text("keep me")
        sandbox = eryx.Sandbox(
            volumes=[(str(tmp_path), "/mnt/data", True)],
        )
        with pytest.raises(eryx.ExecutionError):
            sandbox.execute('open("/mnt/data/new.txt", "w").write("fail")')
        # Original file should be untouched
        assert (tmp_path / "existing.txt").read_text() == "keep me"

    def test_read_only_allows_reads(self, tmp_path):
        """A read-only volume mount should still allow reading."""
        (tmp_path / "readable.txt").write_text("can read this")
        sandbox = eryx.Sandbox(
            volumes=[(str(tmp_path), "/mnt/data", True)],
        )
        result = sandbox.execute('print(open("/mnt/data/readable.txt").read())')
        assert "can read this" in result.stdout

    def test_multiple_mounts(self, tmp_path):
        """Multiple directories can be mounted simultaneously."""
        dir_a = tmp_path / "a"
        dir_b = tmp_path / "b"
        dir_a.mkdir()
        dir_b.mkdir()
        (dir_a / "file_a.txt").write_text("from A")
        (dir_b / "file_b.txt").write_text("from B")

        sandbox = eryx.Sandbox(
            volumes=[
                (str(dir_a), "/mnt/a", True),
                (str(dir_b), "/mnt/b", True),
            ],
        )
        result = sandbox.execute(
            """
a = open("/mnt/a/file_a.txt").read()
b = open("/mnt/b/file_b.txt").read()
print(f"{a} + {b}")
"""
        )
        assert "from A + from B" in result.stdout

    def test_list_directory(self, tmp_path):
        """Sandbox can list files in a mounted directory."""
        (tmp_path / "one.txt").write_text("1")
        (tmp_path / "two.txt").write_text("2")
        sandbox = eryx.Sandbox(
            volumes=[(str(tmp_path), "/mnt/data", True)],
        )
        result = sandbox.execute(
            """
import os
files = sorted(os.listdir("/mnt/data"))
print(files)
"""
        )
        assert "one.txt" in result.stdout
        assert "two.txt" in result.stdout

    def test_subdirectory_access(self, tmp_path):
        """Sandbox can access files in subdirectories of the mount."""
        subdir = tmp_path / "sub" / "dir"
        subdir.mkdir(parents=True)
        (subdir / "nested.txt").write_text("deeply nested")
        sandbox = eryx.Sandbox(
            volumes=[(str(tmp_path), "/mnt/data", True)],
        )
        result = sandbox.execute(
            'print(open("/mnt/data/sub/dir/nested.txt").read())'
        )
        assert "deeply nested" in result.stdout


class TestSessionVolumes:
    """Test volume mounts with the stateful Session."""

    def test_volumes_with_session(self, tmp_path):
        """Session can use volume mounts for host filesystem access."""
        (tmp_path / "input.txt").write_text("session host file")
        session = eryx.Session(
            volumes=[(str(tmp_path), "/mnt/data", False)],
        )
        result = session.execute('print(open("/mnt/data/input.txt").read())')
        assert "session host file" in result.stdout

    def test_volumes_persist_across_executions(self, tmp_path):
        """Writes to volumes in one execution are visible in the next."""
        session = eryx.Session(
            volumes=[(str(tmp_path), "/mnt/data", False)],
        )
        session.execute(
            'open("/mnt/data/persisted.txt", "w").write("from exec 1")'
        )
        result = session.execute(
            'print(open("/mnt/data/persisted.txt").read())'
        )
        assert "from exec 1" in result.stdout
        # Also verify on host
        assert (tmp_path / "persisted.txt").read_text() == "from exec 1"

    def test_volumes_with_vfs_coexistence(self, tmp_path):
        """Volumes and VFS can coexist in the same session."""
        (tmp_path / "host_file.txt").write_text("from host")
        vfs = eryx.VfsStorage()
        session = eryx.Session(
            vfs=vfs,
            volumes=[(str(tmp_path), "/mnt/host", True)],
        )
        # Write to VFS
        session.execute(
            'open("/data/vfs_file.txt", "w").write("from vfs")'
        )
        # Read from both
        result = session.execute(
            """
host = open("/mnt/host/host_file.txt").read()
vfs = open("/data/vfs_file.txt").read()
print(f"host={host}, vfs={vfs}")
"""
        )
        assert "host=from host" in result.stdout
        assert "vfs=from vfs" in result.stdout

    def test_session_volumes_auto_creates_vfs(self, tmp_path):
        """When volumes are provided without explicit VFS, one is auto-created."""
        (tmp_path / "auto.txt").write_text("auto vfs")
        session = eryx.Session(
            volumes=[(str(tmp_path), "/mnt/data", True)],
        )
        result = session.execute('print(open("/mnt/data/auto.txt").read())')
        assert "auto vfs" in result.stdout


class TestSandboxFactoryVolumes:
    """Test volume mounts with SandboxFactory-created sandboxes."""

    def test_factory_sandbox_with_volumes(self, sandbox_factory, tmp_path):
        """Factory-created sandboxes support volume mounts."""
        (tmp_path / "factory.txt").write_text("from factory sandbox")
        sandbox = sandbox_factory.create_sandbox(
            volumes=[(str(tmp_path), "/mnt/data", True)],
        )
        result = sandbox.execute('print(open("/mnt/data/factory.txt").read())')
        assert "from factory sandbox" in result.stdout
