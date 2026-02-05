# VFS and File Persistence

Eryx provides a Virtual Filesystem (VFS) that allows sandboxed Python code to read and write files without accessing the host filesystem. This is essential for applications that need file I/O while maintaining security isolation.

## Overview

The VFS provides:

- **Isolated file storage** - Files exist only within the VFS, not on the host
- **Persistence across executions** - Files survive multiple `execute()` calls in a session
- **Shared storage** - Multiple sessions can share the same VFS storage
- **Full filesystem API** - Standard Python file operations work (open, read, write, mkdir, etc.)
- **SQLite support** - SQLite databases can be stored in the VFS

## Creating a Session with VFS

To use the VFS, create a `VfsStorage` and attach it to a session:

```python
import eryx

# Create VFS storage
storage = eryx.VfsStorage()

# Create a session with VFS enabled
session = eryx.Session(vfs=storage)

# Now you can read/write files in the sandbox
session.execute("""
with open('/data/hello.txt', 'w') as f:
    f.write('Hello, VFS!')
""")

result = session.execute("""
with open('/data/hello.txt', 'r') as f:
    print(f.read())
""")
print(result.stdout)  # "Hello, VFS!"
```

## Default Mount Path

By default, the VFS is mounted at `/data`. You can customize this:

```python
import eryx

storage = eryx.VfsStorage()

# Use default mount path (/data)
session1 = eryx.Session(vfs=storage)
print(session1.vfs_mount_path)  # "/data"

# Use custom mount path
session2 = eryx.Session(vfs=storage, vfs_mount_path="/myfiles")
print(session2.vfs_mount_path)  # "/myfiles"

session2.execute("open('/myfiles/test.txt', 'w').write('custom path')")
```

## File Operations

The VFS supports standard Python file operations:

### Reading and Writing Files

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

# Write a file
session.execute("""
with open('/data/example.txt', 'w') as f:
    f.write('Line 1\\n')
    f.write('Line 2\\n')
""")

# Read the file
result = session.execute("""
with open('/data/example.txt', 'r') as f:
    content = f.read()
    print(content)
""")
print(result.stdout)
# Line 1
# Line 2
```

### Appending to Files

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

session.execute("open('/data/log.txt', 'w').write('Entry 1\\n')")
session.execute("open('/data/log.txt', 'a').write('Entry 2\\n')")
session.execute("open('/data/log.txt', 'a').write('Entry 3\\n')")

result = session.execute("print(open('/data/log.txt').read())")
print(result.stdout)
# Entry 1
# Entry 2
# Entry 3
```

### Binary Files

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

session.execute("""
# Write binary data
data = bytes([0, 1, 2, 255, 254, 253])
with open('/data/binary.bin', 'wb') as f:
    f.write(data)
""")

result = session.execute("""
with open('/data/binary.bin', 'rb') as f:
    data = f.read()
print(list(data))
""")
print(result.stdout)  # "[0, 1, 2, 255, 254, 253]"
```

### Using pathlib

The VFS works with Python's `pathlib`:

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

session.execute("""
from pathlib import Path

# Write using pathlib
p = Path('/data/pathlib_test.txt')
p.write_text('Written with pathlib')
""")

result = session.execute("""
from pathlib import Path
print(Path('/data/pathlib_test.txt').read_text())
""")
print(result.stdout)  # "Written with pathlib"
```

## Directory Operations

### Creating Directories

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

session.execute("""
import os
os.makedirs('/data/nested/deep/directory', exist_ok=True)

with open('/data/nested/deep/directory/file.txt', 'w') as f:
    f.write('Deep file')
""")

result = session.execute("print(open('/data/nested/deep/directory/file.txt').read())")
print(result.stdout)  # "Deep file"
```

### Listing Directories

```python
import eryx

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
print(files)
""")
print(result.stdout)  # "['a.txt', 'b.txt', 'c.txt']"
```

### Deleting Files and Directories

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

session.execute("""
import os

# Create file
open('/data/temp.txt', 'w').write('temporary')

# Delete file
os.remove('/data/temp.txt')

# Check if deleted
print(f"exists: {os.path.exists('/data/temp.txt')}")
""")
```

## Sharing Storage Between Sessions

Multiple sessions can share the same VFS storage:

```python
import eryx

# Create shared storage
storage = eryx.VfsStorage()

# Session 1 writes a file
session1 = eryx.Session(vfs=storage)
session1.execute("open('/data/shared.txt', 'w').write('from session 1')")

# Session 2 can read the same file
session2 = eryx.Session(vfs=storage)
result = session2.execute("print(open('/data/shared.txt').read())")
print(result.stdout)  # "from session 1"
```

## Isolated Storage

Each `VfsStorage` instance is independent:

```python
import eryx

# Two separate storage instances
storage1 = eryx.VfsStorage()
storage2 = eryx.VfsStorage()

session1 = eryx.Session(vfs=storage1)
session1.execute("open('/data/isolated.txt', 'w').write('only in storage1')")

session2 = eryx.Session(vfs=storage2)
result = session2.execute("""
import os
print(f"exists: {os.path.exists('/data/isolated.txt')}")
""")
print(result.stdout)  # "exists: False"
```

## VFS Persistence Across Reset

VFS data persists even when you reset the session's Python state:

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

# Write a file
session.execute("open('/data/persist.txt', 'w').write('before reset')")

# Reset Python state (clears variables, etc.)
session.reset()

# VFS data is still there
result = session.execute("print(open('/data/persist.txt').read())")
print(result.stdout)  # "before reset"
```

## SQLite Databases in VFS

The VFS supports SQLite databases, allowing persistent structured data:

### In-Memory Databases

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

result = session.execute("""
import sqlite3

conn = sqlite3.connect(':memory:')
cursor = conn.cursor()

cursor.execute('CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)')
cursor.execute("INSERT INTO users (name) VALUES ('Alice')")
cursor.execute("INSERT INTO users (name) VALUES ('Bob')")

cursor.execute('SELECT name FROM users ORDER BY id')
names = [row[0] for row in cursor.fetchall()]
conn.close()

print(names)
""")
print(result.stdout)  # "['Alice', 'Bob']"
```

### File-Based Databases

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

# Create database and insert data
session.execute("""
import sqlite3
conn = sqlite3.connect('/data/app.db')
cursor = conn.cursor()
cursor.execute('CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT, price REAL)')
cursor.execute("INSERT INTO products (name, price) VALUES ('Widget', 9.99)")
cursor.execute("INSERT INTO products (name, price) VALUES ('Gadget', 19.99)")
conn.commit()
conn.close()
""")

# Query in a separate execution
result = session.execute("""
import sqlite3
conn = sqlite3.connect('/data/app.db')
cursor = conn.cursor()
cursor.execute('SELECT name, price FROM products')
for name, price in cursor.fetchall():
    print(f"{name}: ${price}")
conn.close()
""")
print(result.stdout)
# Widget: $9.99
# Gadget: $19.99
```

### Database Persistence Across Sessions

```python
import eryx

storage = eryx.VfsStorage()

# Session 1: Create database
session1 = eryx.Session(vfs=storage)
session1.execute("""
import sqlite3
conn = sqlite3.connect('/data/shared.db')
cursor = conn.cursor()
cursor.execute('CREATE TABLE messages (id INTEGER PRIMARY KEY, text TEXT)')
cursor.execute("INSERT INTO messages (text) VALUES ('Hello from session 1')")
conn.commit()
conn.close()
""")

# Session 2: Read from same database
session2 = eryx.Session(vfs=storage)
result = session2.execute("""
import sqlite3
conn = sqlite3.connect('/data/shared.db')
cursor = conn.cursor()
cursor.execute('SELECT text FROM messages')
print(cursor.fetchone()[0])
conn.close()
""")
print(result.stdout)  # "Hello from session 1"
```

## Host Filesystem Isolation

The VFS is completely isolated from the host filesystem:

```python
import eryx

storage = eryx.VfsStorage()
session = eryx.Session(vfs=storage)

result = session.execute("""
import os

# Try to access real /etc (should fail or be virtual)
try:
    files = os.listdir('/etc')
    has_passwd = 'passwd' in files
    print(f"has_real_passwd: {has_passwd}")
except Exception as e:
    print(f"access_denied: {type(e).__name__}")
""")
# Should show "has_real_passwd: False" or "access_denied: ..."
```

## Best Practices

1. **Use shared storage for multi-session workflows** - When sessions need to share data
2. **Use separate storage for isolation** - When sessions should be independent
3. **Prefer file-based SQLite over in-memory** - For data that needs to persist
4. **Use appropriate mount paths** - Customize for your application's needs
5. **Handle file errors gracefully** - Files may not exist or paths may be invalid

## Next Steps

- [Sessions](./sessions.md) - Learn more about stateful execution
- [Sandboxes](./sandboxes.md) - Basic sandbox usage
- [Resource Limits](./resource-limits.md) - Controlling execution constraints
