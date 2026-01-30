"""Tests for the Session class and VFS integration."""

import eryx
import pytest


class TestSession:
    """Tests for the Session class."""

    def test_create_session(self):
        """Test that a session can be created."""
        session = eryx.Session()
        assert session is not None

    def test_simple_execution(self):
        """Test simple code execution."""
        session = eryx.Session()
        result = session.execute('print("hello")')
        assert result.stdout == "hello"

    def test_state_persistence(self):
        """Test that state persists across executions."""
        session = eryx.Session()
        session.execute("x = 42")
        session.execute("y = x * 2")
        result = session.execute("print(f'{x}, {y}')")
        assert result.stdout == "42, 84"

    def test_function_persistence(self):
        """Test that functions persist across executions."""
        session = eryx.Session()
        session.execute("""
def greet(name):
    return f"Hello, {name}!"
""")
        result = session.execute('print(greet("World"))')
        assert result.stdout == "Hello, World!"

    def test_class_persistence(self):
        """Test that classes persist across executions."""
        session = eryx.Session()
        session.execute("""
class Counter:
    def __init__(self):
        self.count = 0
    def increment(self):
        self.count += 1
        return self.count
""")
        session.execute("c = Counter()")
        result = session.execute("print(c.increment(), c.increment(), c.increment())")
        assert result.stdout == "1 2 3"

    def test_import_persistence(self):
        """Test that imports persist across executions."""
        session = eryx.Session()
        session.execute("import json")
        result = session.execute('print(json.dumps({"a": 1}))')
        assert result.stdout == '{"a": 1}'

    def test_execution_count(self):
        """Test execution count tracking."""
        session = eryx.Session()
        assert session.execution_count == 0
        session.execute("x = 1")
        assert session.execution_count == 1
        session.execute("y = 2")
        assert session.execution_count == 2

    def test_reset_clears_state(self):
        """Test that reset clears Python state."""
        session = eryx.Session()
        session.execute("x = 42")
        session.reset()
        with pytest.raises(eryx.ExecutionError):
            session.execute("print(x)")

    def test_clear_state(self):
        """Test that clear_state clears variables."""
        session = eryx.Session()
        session.execute("x = 42")
        session.clear_state()
        with pytest.raises(eryx.ExecutionError):
            session.execute("print(x)")

    def test_execution_timeout(self):
        """Test that execution timeout can be set and retrieved."""
        session = eryx.Session()
        assert session.execution_timeout_ms is None

        session.execution_timeout_ms = 5000
        assert session.execution_timeout_ms == 5000

        session.execution_timeout_ms = None
        assert session.execution_timeout_ms is None

    def test_execution_timeout_from_constructor(self):
        """Test that execution timeout can be set in constructor."""
        session = eryx.Session(execution_timeout_ms=3000)
        assert session.execution_timeout_ms == 3000

    def test_timeout_triggers(self):
        """Test that timeout actually triggers."""
        session = eryx.Session(execution_timeout_ms=500)
        with pytest.raises(eryx.TimeoutError):
            session.execute("while True: pass")

    def test_repr(self):
        """Test __repr__ output."""
        session = eryx.Session()
        repr_str = repr(session)
        assert "Session" in repr_str
        assert "execution_count" in repr_str

    def test_snapshot_and_restore(self):
        """Test state snapshot and restore."""
        session = eryx.Session()
        session.execute("x = 42")
        session.execute("y = 'hello'")

        snapshot = session.snapshot_state()
        assert isinstance(snapshot, bytes)
        assert len(snapshot) > 0

        # Clear and restore
        session.clear_state()
        session.restore_state(snapshot)

        result = session.execute("print(f'{x}, {y}')")
        assert result.stdout == "42, hello"

    def test_snapshot_restore_across_sessions(self):
        """Test that snapshot can be restored in a different session."""
        session1 = eryx.Session()
        session1.execute("data = [1, 2, 3]")
        session1.execute("total = sum(data)")
        snapshot = session1.snapshot_state()

        session2 = eryx.Session()
        session2.restore_state(snapshot)
        result = session2.execute("print(f'{data}, {total}')")
        assert result.stdout == "[1, 2, 3], 6"


class TestSessionWithVfs:
    """Tests for Session with VFS support."""

    def test_session_with_vfs(self):
        """Test creating a session with VFS."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)
        assert session is not None
        assert session.vfs is not None

    def test_vfs_property(self):
        """Test that vfs property returns the storage."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)
        assert session.vfs is not None

    def test_vfs_mount_path_default(self):
        """Test default VFS mount path."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)
        assert session.vfs_mount_path == "/data"

    def test_vfs_mount_path_custom(self):
        """Test custom VFS mount path."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage, vfs_mount_path="/custom")
        assert session.vfs_mount_path == "/custom"

    def test_write_and_read_file(self):
        """Test writing and reading a file in the VFS."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        session.execute("""
with open('/data/test.txt', 'w') as f:
    f.write('hello world')
""")

        result = session.execute("""
with open('/data/test.txt', 'r') as f:
    print(f.read())
""")
        assert result.stdout == "hello world"

    def test_file_persistence_across_executions(self):
        """Test that files persist across multiple executions."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        session.execute("open('/data/file.txt', 'w').write('first')")
        session.execute("open('/data/file.txt', 'a').write(' second')")
        result = session.execute("print(open('/data/file.txt').read())")
        assert result.stdout == "first second"

    def test_storage_shared_between_sessions(self):
        """Test that storage can be shared between multiple sessions."""
        storage = eryx.VfsStorage()

        session1 = eryx.Session(vfs=storage)
        session1.execute("open('/data/shared.txt', 'w').write('from session 1')")

        session2 = eryx.Session(vfs=storage)
        result = session2.execute("print(open('/data/shared.txt').read())")
        assert result.stdout == "from session 1"

    def test_create_directory(self):
        """Test creating directories in VFS."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        session.execute("""
import os
os.makedirs('/data/nested/dir', exist_ok=True)
with open('/data/nested/dir/file.txt', 'w') as f:
    f.write('nested content')
""")

        result = session.execute("print(open('/data/nested/dir/file.txt').read())")
        assert result.stdout == "nested content"

    def test_list_directory(self):
        """Test listing directory contents."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        session.execute("""
import os
open('/data/a.txt', 'w').write('a')
open('/data/b.txt', 'w').write('b')
open('/data/c.txt', 'w').write('c')
""")

        result = session.execute("""
import os
files = sorted(os.listdir('/data'))
print(','.join(files))
""")
        assert "a.txt" in result.stdout
        assert "b.txt" in result.stdout
        assert "c.txt" in result.stdout

    def test_delete_file(self):
        """Test deleting a file."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        session.execute("open('/data/to_delete.txt', 'w').write('delete me')")
        session.execute("""
import os
os.remove('/data/to_delete.txt')
""")

        result = session.execute("""
import os
exists = os.path.exists('/data/to_delete.txt')
print(f'exists: {exists}')
""")
        assert "exists: False" in result.stdout

    def test_file_not_found(self):
        """Test that reading non-existent file raises error."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        with pytest.raises(eryx.ExecutionError):
            session.execute("open('/data/nonexistent.txt').read()")

    def test_pathlib_support(self):
        """Test that pathlib works with VFS."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        session.execute("""
from pathlib import Path
p = Path('/data/pathlib_test.txt')
p.write_text('pathlib content')
""")

        result = session.execute("""
from pathlib import Path
print(Path('/data/pathlib_test.txt').read_text())
""")
        assert result.stdout == "pathlib content"

    def test_binary_file(self):
        """Test reading and writing binary files."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        session.execute("""
data = bytes([0, 1, 2, 255, 254, 253])
with open('/data/binary.bin', 'wb') as f:
    f.write(data)
""")

        result = session.execute("""
with open('/data/binary.bin', 'rb') as f:
    data = f.read()
print(list(data))
""")
        assert result.stdout == "[0, 1, 2, 255, 254, 253]"

    def test_vfs_isolation_from_host(self):
        """Test that VFS doesn't expose host filesystem."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        # Write to VFS
        session.execute("open('/data/secret.txt', 'w').write('vfs only')")

        # Try to access real /etc (should fail or be empty)
        result = session.execute("""
import os
try:
    files = os.listdir('/etc')
    # Even if it doesn't error, /etc should be virtual/empty
    has_passwd = 'passwd' in files
    print(f'has_real_passwd: {has_passwd}')
except Exception as e:
    print(f'access_denied: {type(e).__name__}')
""")
        # Should either deny access or not have real files
        assert (
            "has_real_passwd: False" in result.stdout
            or "access_denied" in result.stdout
        )

    def test_vfs_persists_across_reset(self):
        """Test that VFS data persists across session reset."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        session.execute("open('/data/persist.txt', 'w').write('before reset')")
        session.reset()

        result = session.execute("print(open('/data/persist.txt').read())")
        assert result.stdout == "before reset"

    def test_custom_mount_path(self):
        """Test using a custom mount path."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage, vfs_mount_path="/myfs")

        session.execute("open('/myfs/test.txt', 'w').write('custom path')")
        result = session.execute("print(open('/myfs/test.txt').read())")
        assert result.stdout == "custom path"

    def test_session_isolation_without_shared_storage(self):
        """Test that sessions without shared storage are isolated."""
        storage1 = eryx.VfsStorage()
        storage2 = eryx.VfsStorage()

        session1 = eryx.Session(vfs=storage1)
        session1.execute("open('/data/isolated.txt', 'w').write('session1')")

        session2 = eryx.Session(vfs=storage2)
        result = session2.execute("""
import os
exists = os.path.exists('/data/isolated.txt')
print(f'file_exists: {exists}')
""")
        assert "file_exists: False" in result.stdout


class TestSqliteWithVfs:
    """Tests for SQLite3 database support with VFS."""

    def test_sqlite3_import(self):
        """Test that sqlite3 module can be imported."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        result = session.execute("""
import sqlite3
print(sqlite3.sqlite_version)
""")
        # Should print version like "3.51.2"
        assert "." in result.stdout
        assert (
            result.stdout.strip().replace(".", "").isdigit()
            or result.stdout.strip()[0].isdigit()
        )

    def test_sqlite3_in_memory_database(self):
        """Test SQLite3 in-memory database operations."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        result = session.execute("""
import sqlite3
conn = sqlite3.connect(':memory:')
cursor = conn.cursor()
cursor.execute('CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)')
cursor.execute("INSERT INTO test (name) VALUES ('hello')")
cursor.execute("INSERT INTO test (name) VALUES ('world')")
cursor.execute('SELECT name FROM test ORDER BY id')
rows = cursor.fetchall()
conn.close()
print([r[0] for r in rows])
""")
        assert "['hello', 'world']" in result.stdout

    def test_sqlite3_file_database_in_vfs(self):
        """Test SQLite3 database stored in VFS."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        # Create database and insert data
        session.execute("""
import sqlite3
conn = sqlite3.connect('/data/test.db')
cursor = conn.cursor()
cursor.execute('CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)')
cursor.execute("INSERT INTO users (name) VALUES ('alice')")
conn.commit()
conn.close()
""")

        # Query in separate execution
        result = session.execute("""
import sqlite3
conn = sqlite3.connect('/data/test.db')
cursor = conn.cursor()
cursor.execute('SELECT name FROM users')
name = cursor.fetchone()[0]
conn.close()
print(name)
""")
        assert result.stdout == "alice"

    def test_sqlite3_persistence_across_reset(self):
        """Test that SQLite database persists across session reset."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        # Create database
        session.execute("""
import sqlite3
conn = sqlite3.connect('/data/persist.db')
cursor = conn.cursor()
cursor.execute('CREATE TABLE data (value TEXT)')
cursor.execute("INSERT INTO data VALUES ('before_reset')")
conn.commit()
conn.close()
""")

        # Reset session
        session.reset()

        # Verify data persists
        result = session.execute("""
import sqlite3
conn = sqlite3.connect('/data/persist.db')
cursor = conn.cursor()
cursor.execute('SELECT value FROM data')
value = cursor.fetchone()[0]
conn.close()
print(value)
""")
        assert result.stdout == "before_reset"

    def test_sqlite3_shared_between_sessions(self):
        """Test that SQLite database can be shared between sessions."""
        storage = eryx.VfsStorage()

        # Session 1 creates database
        session1 = eryx.Session(vfs=storage)
        session1.execute("""
import sqlite3
conn = sqlite3.connect('/data/shared.db')
cursor = conn.cursor()
cursor.execute('CREATE TABLE shared (msg TEXT)')
cursor.execute("INSERT INTO shared VALUES ('from_session_1')")
conn.commit()
conn.close()
""")

        # Session 2 reads database
        session2 = eryx.Session(vfs=storage)
        result = session2.execute("""
import sqlite3
conn = sqlite3.connect('/data/shared.db')
cursor = conn.cursor()
cursor.execute('SELECT msg FROM shared')
msg = cursor.fetchone()[0]
conn.close()
print(msg)
""")
        assert result.stdout == "from_session_1"

    def test_sqlite3_transaction_rollback(self):
        """Test SQLite3 transaction rollback."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)

        result = session.execute("""
import sqlite3
conn = sqlite3.connect('/data/rollback.db')
cursor = conn.cursor()
cursor.execute('CREATE TABLE counter (n INTEGER)')
cursor.execute('INSERT INTO counter VALUES (1)')
conn.commit()

# Start transaction, insert, then rollback
cursor.execute('INSERT INTO counter VALUES (2)')
conn.rollback()

cursor.execute('SELECT COUNT(*) FROM counter')
count = cursor.fetchone()[0]
conn.close()
print(count)
""")
        assert result.stdout == "1"


class TestSessionRepr:
    """Tests for Session __repr__ with VFS info."""

    def test_repr_without_vfs(self):
        """Test repr without VFS."""
        session = eryx.Session()
        repr_str = repr(session)
        assert "Session" in repr_str
        assert "vfs_mount_path" not in repr_str

    def test_repr_with_vfs(self):
        """Test repr with VFS shows mount path."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage)
        repr_str = repr(session)
        assert "Session" in repr_str
        assert "vfs_mount_path" in repr_str
        assert "/data" in repr_str

    def test_repr_with_custom_mount_path(self):
        """Test repr shows custom mount path."""
        storage = eryx.VfsStorage()
        session = eryx.Session(vfs=storage, vfs_mount_path="/custom")
        repr_str = repr(session)
        assert "/custom" in repr_str
