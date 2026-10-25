"""Tests for the virtual filesystem (VFS) functionality.

Note: VFS integration tests with Session are in test_session.py (TestSessionWithVfs).
This file contains tests for VfsStorage itself.
"""

import eryx
import pytest


# Close explicitly: an error raised from an implicit close at GC is swallowed.
WRITE_BIG = """
with open("/data/big.bin", "wb") as f:
    f.write(b"x" * 100)
"""


class TestVfsStorage:
    """Tests for the VfsStorage class."""

    def test_create_storage(self):
        """Test that VfsStorage can be created."""
        storage = eryx.VfsStorage()
        assert storage is not None

    def test_storage_repr(self):
        """Test VfsStorage repr."""
        storage = eryx.VfsStorage()
        assert "VfsStorage" in repr(storage)

    def test_storage_max_bytes(self):
        """VfsStorage(max_bytes=...) caps total file content."""
        session = eryx.Session(vfs=eryx.VfsStorage(max_bytes=1024))
        session.execute('open("/data/small.bin", "wb").write(b"x" * 1000)')
        with pytest.raises(eryx.PythonExceptionError, match="OSError"):
            session.execute(WRITE_BIG)
        # The default cap leaves room for the same writes.
        session = eryx.Session(vfs=eryx.VfsStorage())
        session.execute('open("/data/small.bin", "wb").write(b"x" * 1000)')
        session.execute(WRITE_BIG)
