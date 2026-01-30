#!/usr/bin/env python3
"""SQLite database with VFS (Virtual File System) example.

Demonstrates using SQLite3 databases inside an eryx sandbox with persistent
storage via the VFS. The database file is stored in the virtual filesystem
and persists across multiple executions and even across sessions.

This example showcases:
- SQLite3 support in the eryx sandbox
- VFS for persistent file storage
- Database operations (CRUD)
- Data persistence across session resets
- Sharing database between sessions via VfsStorage

Usage:
    python examples/sqlite_vfs.py
"""

import eryx


def main():
    print("=== Eryx SQLite + VFS Example ===\n")

    # Create a VFS storage backend
    # This provides persistent storage that survives session resets
    storage = eryx.VfsStorage()

    # Create a session with VFS mounted at /data
    print("Creating session with VFS storage...")
    session = eryx.Session(vfs=storage)
    print(f"VFS mount path: {session.vfs_mount_path}")

    # Check SQLite version
    print("\n--- SQLite Version ---")
    result = session.execute("""
import sqlite3
print(f"SQLite version: {sqlite3.sqlite_version}")
print(f"SQLite version info: {sqlite3.sqlite_version_info}")
""")
    print(result.stdout)

    # Create a database in the VFS
    print("--- Creating Database ---")
    result = session.execute("""
import sqlite3

# Connect to a database file in the VFS
# This file will persist in the VfsStorage
conn = sqlite3.connect('/data/myapp.db')
cursor = conn.cursor()

# Create a table
cursor.execute('''
    CREATE TABLE IF NOT EXISTS users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        username TEXT UNIQUE NOT NULL,
        email TEXT NOT NULL,
        created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    )
''')

# Create an index for faster lookups
cursor.execute('CREATE INDEX IF NOT EXISTS idx_username ON users(username)')

conn.commit()
conn.close()
print("Database and table created successfully!")
""")
    print(result.stdout)

    # Insert some data
    print("--- Inserting Data ---")
    result = session.execute("""
import sqlite3

conn = sqlite3.connect('/data/myapp.db')
cursor = conn.cursor()

# Insert users
users = [
    ('alice', 'alice@example.com'),
    ('bob', 'bob@example.com'),
    ('charlie', 'charlie@example.com'),
]

cursor.executemany(
    'INSERT OR IGNORE INTO users (username, email) VALUES (?, ?)',
    users
)

conn.commit()
print(f"Inserted {cursor.rowcount} users")
conn.close()
""")
    print(result.stdout)

    # Query data
    print("--- Querying Data ---")
    result = session.execute("""
import sqlite3

conn = sqlite3.connect('/data/myapp.db')
conn.row_factory = sqlite3.Row  # Enable column access by name
cursor = conn.cursor()

cursor.execute('SELECT id, username, email FROM users ORDER BY username')
rows = cursor.fetchall()

print(f"Found {len(rows)} users:")
for row in rows:
    print(f"  [{row['id']}] {row['username']} <{row['email']}>")

conn.close()
""")
    print(result.stdout)

    # Demonstrate persistence across session reset
    print("--- Persistence Across Reset ---")
    print("Resetting session (clears Python state but keeps VFS)...")
    session.reset()

    result = session.execute("""
import sqlite3

# The database file persists in VFS after reset!
conn = sqlite3.connect('/data/myapp.db')
cursor = conn.cursor()

cursor.execute('SELECT COUNT(*) FROM users')
count = cursor.fetchone()[0]
print(f"After reset: database still has {count} users")

# Add another user to prove we can still write
cursor.execute(
    'INSERT OR IGNORE INTO users (username, email) VALUES (?, ?)',
    ('diana', 'diana@example.com')
)
conn.commit()

cursor.execute('SELECT COUNT(*) FROM users')
new_count = cursor.fetchone()[0]
print(f"After insert: now have {new_count} users")

conn.close()
""")
    print(result.stdout)

    # Demonstrate sharing storage between sessions
    print("--- Sharing Database Between Sessions ---")
    print("Creating a second session with the same VfsStorage...")

    session2 = eryx.Session(vfs=storage)
    result = session2.execute("""
import sqlite3

# Second session can access the same database!
conn = sqlite3.connect('/data/myapp.db')
cursor = conn.cursor()

cursor.execute('SELECT username, email FROM users ORDER BY id')
rows = cursor.fetchall()

print(f"Session 2 sees {len(rows)} users:")
for username, email in rows:
    print(f"  - {username}: {email}")

conn.close()
""")
    print(result.stdout)

    # Demonstrate update and delete
    print("--- Update and Delete Operations ---")
    result = session.execute("""
import sqlite3

conn = sqlite3.connect('/data/myapp.db')
cursor = conn.cursor()

# Update a user's email
cursor.execute(
    "UPDATE users SET email = ? WHERE username = ?",
    ('alice.smith@example.com', 'alice')
)
print(f"Updated {cursor.rowcount} row(s)")

# Delete a user
cursor.execute("DELETE FROM users WHERE username = ?", ('bob',))
print(f"Deleted {cursor.rowcount} row(s)")

conn.commit()

# Show final state
cursor.execute('SELECT username, email FROM users ORDER BY username')
print("\\nFinal user list:")
for username, email in cursor.fetchall():
    print(f"  - {username}: {email}")

conn.close()
""")
    print(result.stdout)

    # Demonstrate transactions
    print("--- Transaction Rollback ---")
    result = session.execute("""
import sqlite3

conn = sqlite3.connect('/data/myapp.db')
cursor = conn.cursor()

# Get initial count
cursor.execute('SELECT COUNT(*) FROM users')
initial_count = cursor.fetchone()[0]
print(f"Initial user count: {initial_count}")

try:
    # Start a transaction (implicit)
    cursor.execute("INSERT INTO users (username, email) VALUES ('temp1', 'temp1@test.com')")
    cursor.execute("INSERT INTO users (username, email) VALUES ('temp2', 'temp2@test.com')")

    # Simulate an error before commit
    raise ValueError("Simulated error - rolling back!")

    conn.commit()  # Never reached
except ValueError as e:
    print(f"Error occurred: {e}")
    conn.rollback()

# Verify rollback worked
cursor.execute('SELECT COUNT(*) FROM users')
final_count = cursor.fetchone()[0]
print(f"After rollback: {final_count} users (unchanged)")

conn.close()
""")
    print(result.stdout)

    # Demonstrate in-memory database (no VFS needed)
    print("--- In-Memory Database ---")
    result = session.execute("""
import sqlite3

# In-memory databases work too (not persisted to VFS)
conn = sqlite3.connect(':memory:')
cursor = conn.cursor()

cursor.execute('CREATE TABLE temp (value TEXT)')
cursor.executemany('INSERT INTO temp VALUES (?)', [('a',), ('b',), ('c',)])

cursor.execute('SELECT GROUP_CONCAT(value) FROM temp')
print(f"In-memory result: {cursor.fetchone()[0]}")

conn.close()
print("In-memory database closed (data discarded)")
""")
    print(result.stdout)

    # Show database file info
    print("--- Database File Info ---")
    result = session.execute("""
import os

db_path = '/data/myapp.db'
stat = os.stat(db_path)
print(f"Database file: {db_path}")
print(f"Size: {stat.st_size} bytes")

# List all files in /data
print(f"\\nFiles in VFS:")
for f in os.listdir('/data'):
    fstat = os.stat(f'/data/{f}')
    print(f"  - {f}: {fstat.st_size} bytes")
""")
    print(result.stdout)

    print("=== Done ===")
    print("\nKey takeaways:")
    print("  ✓ SQLite3 works natively in the eryx sandbox")
    print("  ✓ VFS provides persistent storage for database files")
    print("  ✓ Data persists across session.reset() calls")
    print("  ✓ Multiple sessions can share the same VfsStorage")
    print("  ✓ Full SQLite functionality: CRUD, transactions, indexes")


if __name__ == "__main__":
    main()
