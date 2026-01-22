"""Tests for the virtual filesystem (VFS) functionality."""

import eryx
import pytest


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


# VFS is now only available on SessionExecutor in the Rust library.
# Python Session bindings with VFS support are not yet implemented.
# These tests are skipped until Session bindings are added.


@pytest.mark.skip(
    reason="VFS moved to SessionExecutor; Python Session bindings not yet implemented"
)
class TestVfsSandbox:
    """Tests for sandbox with VFS enabled."""

    def test_sandbox_with_vfs(self):
        """Test that a sandbox can be created with VFS."""
        pass

    def test_write_and_read_file(self):
        """Test writing and reading a file in the VFS."""
        pass

    def test_file_persistence_across_executions(self):
        """Test that files persist across multiple sandbox executions."""
        pass

    def test_storage_shared_between_sandboxes(self):
        """Test that storage can be shared between multiple sandboxes."""
        pass

    def test_create_directory(self):
        """Test creating directories in VFS."""
        pass

    def test_list_directory(self):
        """Test listing directory contents."""
        pass

    def test_delete_file(self):
        """Test deleting a file."""
        pass

    def test_file_not_found(self):
        """Test that reading non-existent file raises error."""
        pass

    def test_pathlib_support(self):
        """Test that pathlib works with VFS."""
        pass

    def test_binary_file(self):
        """Test reading and writing binary files."""
        pass

    def test_append_mode(self):
        """Test appending to files."""
        pass

    def test_vfs_isolation_from_host(self):
        """Test that VFS doesn't expose host filesystem."""
        pass
