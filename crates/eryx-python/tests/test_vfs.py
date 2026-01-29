"""Tests for the virtual filesystem (VFS) functionality.

Note: VFS integration tests with Session are in test_session.py (TestSessionWithVfs).
This file contains tests for VfsStorage itself.
"""

import eryx


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
