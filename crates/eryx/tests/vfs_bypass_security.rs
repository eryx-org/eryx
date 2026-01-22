//! Security tests for VFS bypass attempts.
//!
//! These tests verify that Python code cannot bypass the eryx VFS to access
//! the host filesystem or escape the sandboxed directory structure.
//!
//! Think like an attacker: what are all the ways to escape a filesystem sandbox?
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "vfs")]

#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::sync::Arc;

use eryx::{PythonExecutor, SessionExecutor, vfs::InMemoryStorage};

/// Helper to run adversarial Python code and check it doesn't succeed
async fn run_adversarial_test(code: &str, test_name: &str) -> (bool, String) {
    let storage = Arc::new(InMemoryStorage::new());
    let executor = create_executor().await;
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage)
        .await
        .expect("Failed to create session");

    let result = session.execute(code).run().await;
    match result {
        Ok(output) => {
            let stdout = output.stdout;
            let has_security_issue =
                stdout.contains("SECURITY ISSUE") || stdout.contains("BYPASS SUCCESSFUL");
            if has_security_issue {
                eprintln!("SECURITY ISSUE in {}: {}", test_name, stdout);
            }
            (!has_security_issue, stdout)
        }
        Err(e) => {
            // Execution error is generally safe (sandbox blocked something)
            (true, format!("Execution error (safe): {}", e))
        }
    }
}

#[allow(dead_code)]
/// Helper to run adversarial test with verbose output (for debugging)
async fn run_adversarial_test_verbose(code: &str, test_name: &str) -> (bool, String) {
    let storage = Arc::new(InMemoryStorage::new());
    let executor = create_executor().await;
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage)
        .await
        .expect("Failed to create session");

    let result = session.execute(code).run().await;
    match result {
        Ok(output) => {
            let stdout = output.stdout;
            let has_security_issue =
                stdout.contains("SECURITY ISSUE") || stdout.contains("BYPASS SUCCESSFUL");
            println!("=== {} ===\n{}", test_name, stdout);
            if !output.stderr.is_empty() {
                println!("stderr: {}", output.stderr);
            }
            (!has_security_issue, stdout)
        }
        Err(e) => {
            println!("=== {} ===\nExecution error (safe): {}", test_name, e);
            (true, format!("Execution error (safe): {}", e))
        }
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

#[cfg(not(feature = "embedded"))]
fn runtime_wasm_path() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-runtime")
        .join("runtime.wasm")
}

#[cfg(not(feature = "embedded"))]
fn python_stdlib_path() -> PathBuf {
    if let Ok(path) = std::env::var("ERYX_PYTHON_STDLIB") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib")
}

/// Create a PythonExecutor for use with SessionExecutor
async fn create_executor() -> Arc<PythonExecutor> {
    #[cfg(feature = "embedded")]
    {
        Arc::new(
            PythonExecutor::from_embedded_runtime().expect("Failed to create embedded executor"),
        )
    }

    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        let executor = PythonExecutor::from_file(runtime_wasm_path())
            .expect("Failed to load runtime")
            .with_python_stdlib(&stdlib_path);
        Arc::new(executor)
    }
}

// =============================================================================
// Basic VFS Functionality Tests
// =============================================================================

/// Test that VFS basic operations work (write, read, list)
#[tokio::test]
async fn test_vfs_basic_operations() {
    let storage = Arc::new(InMemoryStorage::new());
    let executor = create_executor().await;
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage)
        .await
        .expect("Failed to create session");

    let result = session
        .execute(
            r#"
import os

# Write a file (simpler test - no mkdir)
with open('/data/test.txt', 'w') as f:
    f.write('hello world')

# Read it back
with open('/data/test.txt', 'r') as f:
    content = f.read()

# List directory
files = os.listdir('/data')

print(f"Content: {content}")
print(f"Files: {sorted(files)}")
print("VFS basic operations work")
"#,
        )
        .run()
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("Content: hello world"),
        "Should read written content: {}",
        output.stdout
    );
    assert!(
        output.stdout.contains("VFS basic operations work"),
        "Should complete: {}",
        output.stdout
    );
}

/// Test that VFS append mode works correctly
#[tokio::test]
async fn test_vfs_append_mode() {
    let storage = Arc::new(InMemoryStorage::new());
    let executor = create_executor().await;
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage)
        .await
        .expect("Failed to create session");

    // First: write initial content
    let result1 = session
        .execute(
            r#"
with open('/data/append.txt', 'w') as f:
    f.write('first')
print("Wrote initial content")
"#,
        )
        .run()
        .await;
    assert!(
        result1.is_ok(),
        "Initial write should succeed: {:?}",
        result1
    );

    // Second: append to the file
    let result2 = session
        .execute(
            r#"
with open('/data/append.txt', 'a') as f:
    f.write(' second')
print("Appended content")
"#,
        )
        .run()
        .await;
    assert!(result2.is_ok(), "Append should succeed: {:?}", result2);

    // Third: read back and verify
    let result3 = session
        .execute(
            r#"
with open('/data/append.txt', 'r') as f:
    content = f.read()
print(f"Content: {content}")
"#,
        )
        .run()
        .await;

    assert!(result3.is_ok(), "Read should succeed: {:?}", result3);
    let output = result3.unwrap();
    assert!(
        output.stdout.contains("Content: first second"),
        "Append mode should work correctly: {}",
        output.stdout
    );
}

/// Test that VFS data persists across executions within a session
#[tokio::test]
async fn test_vfs_persistence_across_executions() {
    let storage = Arc::new(InMemoryStorage::new());
    let executor = create_executor().await;
    let mut session = SessionExecutor::new_with_vfs(executor, &[], storage)
        .await
        .expect("Failed to create session");

    // First execution: write a file
    let result1 = session
        .execute(
            r#"
with open('/data/persist.txt', 'w') as f:
    f.write('persistent data')
print("Written")
"#,
        )
        .run()
        .await;
    assert!(result1.is_ok(), "First execution should succeed");

    // Second execution: read the file (same session, VFS persists)
    let result2 = session
        .execute(
            r#"
try:
    with open('/data/persist.txt', 'r') as f:
        content = f.read()
    print(f"Read: {content}")
except FileNotFoundError:
    print("File not found - persistence failed!")
"#,
        )
        .run()
        .await;

    assert!(result2.is_ok(), "Second execution should succeed");
    let output = result2.unwrap();
    assert!(
        output.stdout.contains("Read: persistent data"),
        "Should persist data across executions: {}",
        output.stdout
    );
}

// =============================================================================
// Path Traversal Attack Tests
// =============================================================================

/// Test that .. cannot escape the /data directory
#[tokio::test]
async fn test_path_traversal_dot_dot() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try to escape /data using ..
paths_to_try = [
    '/data/../etc/passwd',
    '/data/../../etc/passwd',
    '/data/../../../etc/passwd',
    '/data/subdir/../../etc/passwd',
    '/data/./../../etc/passwd',
]

for path in paths_to_try:
    try:
        with open(path, 'r') as f:
            content = f.read(100)
        print(f"SECURITY ISSUE: Read {path}: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: Cannot read {path}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked {path}: {type(e).__name__}: {e}")
"#,
        "path_traversal_dot_dot",
    )
    .await;
    assert!(safe, "Path traversal with .. should be blocked: {}", output);
}

/// Test that absolute paths outside /data are blocked
#[tokio::test]
async fn test_absolute_path_escape() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try to access absolute paths outside the sandbox
dangerous_paths = [
    '/etc/passwd',
    '/etc/shadow',
    '/proc/self/environ',
    '/proc/1/cmdline',
    '/root/.ssh/id_rsa',
    '/home',
    '/tmp',
    '/var/log/syslog',
]

for path in dangerous_paths:
    try:
        with open(path, 'r') as f:
            content = f.read(100)
        print(f"SECURITY ISSUE: Read {path}: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: Cannot read {path}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked {path}: {type(e).__name__}: {e}")
"#,
        "absolute_path_escape",
    )
    .await;
    assert!(
        safe,
        "Absolute paths outside sandbox should be blocked: {}",
        output
    );
}

/// Test symlink attacks cannot escape the sandbox
#[tokio::test]
async fn test_symlink_escape_attack() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try to create symlinks pointing outside the sandbox
try:
    os.symlink('/etc/passwd', '/data/passwd_link')
    print("Created symlink to /etc/passwd")

    # Try to read through the symlink
    try:
        with open('/data/passwd_link', 'r') as f:
            content = f.read(100)
        print(f"SECURITY ISSUE: Read through symlink: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: Cannot read through symlink: {e}")
except (OSError, PermissionError) as e:
    print(f"EXPECTED: Cannot create escape symlink: {e}")
except Exception as e:
    print(f"EXPECTED: Symlink attack blocked: {type(e).__name__}: {e}")

# Also try relative symlink escape
try:
    os.makedirs('/data/deep/nested', exist_ok=True)
    os.symlink('../../../etc/passwd', '/data/deep/nested/escape')
    with open('/data/deep/nested/escape', 'r') as f:
        content = f.read(100)
    print(f"SECURITY ISSUE: Read via relative symlink escape: {content[:50]}")
except (OSError, PermissionError) as e:
    print(f"EXPECTED: Relative symlink escape blocked: {e}")
except Exception as e:
    print(f"EXPECTED: Relative symlink blocked: {type(e).__name__}: {e}")
"#,
        "symlink_escape_attack",
    )
    .await;
    assert!(safe, "Symlink escape attacks should be blocked: {}", output);
}

/// Test that path normalization doesn't allow escape
#[tokio::test]
async fn test_path_normalization_attacks() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Various path normalization tricks
tricky_paths = [
    '/data/foo/../../../etc/passwd',
    '/data///..///../etc/passwd',
    '/data/./././../../../etc/passwd',
    '/data/subdir/./../../../etc/passwd',
    '/data/.../.../etc/passwd',  # Not valid but test anyway
]

for path in tricky_paths:
    try:
        # Try with os.path.normpath first
        normalized = os.path.normpath(path)
        print(f"Normalized {path} -> {normalized}")

        with open(path, 'r') as f:
            content = f.read(100)
        print(f"SECURITY ISSUE: Read {path}: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: Cannot read {path}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked {path}: {type(e).__name__}: {e}")
"#,
        "path_normalization_attacks",
    )
    .await;
    assert!(
        safe,
        "Path normalization attacks should be blocked: {}",
        output
    );
}

// =============================================================================
// Low-level Filesystem Access Tests
// =============================================================================

/// Test that os.open with dangerous flags is blocked
#[tokio::test]
async fn test_os_open_dangerous_flags() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try os.open with various flags to access host filesystem
try:
    # O_CREAT with path traversal
    fd = os.open('/data/../tmp/evil.txt', os.O_CREAT | os.O_WRONLY, 0o644)
    os.write(fd, b'malicious content')
    os.close(fd)
    print("SECURITY ISSUE: Created file via path traversal with os.open")
except (OSError, PermissionError) as e:
    print(f"EXPECTED: os.open path traversal blocked: {e}")
except Exception as e:
    print(f"EXPECTED: os.open blocked: {type(e).__name__}: {e}")

# Try direct access to sensitive file
try:
    fd = os.open('/etc/passwd', os.O_RDONLY)
    data = os.read(fd, 100)
    os.close(fd)
    print(f"SECURITY ISSUE: Read /etc/passwd via os.open: {data[:50]}")
except (OSError, PermissionError, FileNotFoundError) as e:
    print(f"EXPECTED: os.open to /etc/passwd blocked: {e}")
except Exception as e:
    print(f"EXPECTED: os.open blocked: {type(e).__name__}: {e}")
"#,
        "os_open_dangerous_flags",
    )
    .await;
    assert!(safe, "os.open attacks should be blocked: {}", output);
}

/// Test that _io module cannot bypass VFS
#[tokio::test]
async fn test_io_module_vfs_bypass() {
    let (safe, output) = run_adversarial_test(
        r#"
# Attack: Use _io module to bypass VFS
try:
    import _io

    # Try to open /etc/passwd directly via _io
    try:
        f = _io.open('/etc/passwd', 'r')
        content = f.read(100)
        f.close()
        print(f"SECURITY ISSUE: Read /etc/passwd via _io: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: _io.open blocked: {e}")

    # Try path traversal via _io
    try:
        f = _io.open('/data/../etc/passwd', 'r')
        content = f.read(100)
        f.close()
        print(f"SECURITY ISSUE: Path traversal via _io: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: _io path traversal blocked: {e}")

except ImportError as e:
    print(f"EXPECTED: _io not available: {e}")
except Exception as e:
    print(f"EXPECTED: _io attack failed: {type(e).__name__}: {e}")
"#,
        "io_module_vfs_bypass",
    )
    .await;
    assert!(safe, "_io module should not bypass VFS: {}", output);
}

/// Test that FileIO cannot bypass VFS
#[tokio::test]
async fn test_fileio_vfs_bypass() {
    let (safe, output) = run_adversarial_test(
        r#"
# Attack: Use io.FileIO directly
try:
    import io

    # Try FileIO directly
    try:
        f = io.FileIO('/etc/passwd', 'r')
        content = f.read(100)
        f.close()
        print(f"SECURITY ISSUE: Read via FileIO: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: FileIO blocked: {e}")

except ImportError as e:
    print(f"EXPECTED: io not available: {e}")
except Exception as e:
    print(f"EXPECTED: FileIO attack failed: {type(e).__name__}: {e}")
"#,
        "fileio_vfs_bypass",
    )
    .await;
    assert!(safe, "FileIO should not bypass VFS: {}", output);
}

// =============================================================================
// /proc and /dev Access Tests
// =============================================================================

/// Test that /proc filesystem is not accessible
#[tokio::test]
async fn test_proc_filesystem_blocked() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try to access /proc for information disclosure or escape
proc_paths = [
    '/proc/self/exe',
    '/proc/self/cwd',
    '/proc/self/root',
    '/proc/self/fd/0',
    '/proc/self/maps',
    '/proc/self/environ',
    '/proc/self/cmdline',
    '/proc/1/root',  # Init process root
    '/proc/mounts',
    '/proc/version',
]

for path in proc_paths:
    try:
        if os.path.islink(path):
            target = os.readlink(path)
            print(f"SECURITY ISSUE: Read symlink {path} -> {target}")
        elif os.path.exists(path):
            with open(path, 'r') as f:
                content = f.read(100)
            print(f"SECURITY ISSUE: Read {path}: {content[:50]}")
        else:
            print(f"EXPECTED: {path} does not exist")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: Cannot access {path}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked {path}: {type(e).__name__}: {e}")
"#,
        "proc_filesystem_blocked",
    )
    .await;
    assert!(safe, "/proc access should be blocked: {}", output);
}

/// Test that /dev devices are not accessible
#[tokio::test]
async fn test_dev_devices_blocked() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try to access device files
dev_paths = [
    '/dev/null',
    '/dev/zero',
    '/dev/random',
    '/dev/urandom',
    '/dev/mem',
    '/dev/kmem',
    '/dev/sda',
    '/dev/tty',
    '/dev/ptmx',
]

for path in dev_paths:
    try:
        fd = os.open(path, os.O_RDONLY)
        data = os.read(fd, 10)
        os.close(fd)
        print(f"SECURITY ISSUE: Opened device {path}, read {len(data)} bytes")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: Cannot open {path}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked {path}: {type(e).__name__}: {e}")
"#,
        "dev_devices_blocked",
    )
    .await;
    assert!(safe, "/dev access should be blocked: {}", output);
}

// =============================================================================
// File Descriptor Manipulation Tests
// =============================================================================

/// Test that file descriptor tricks cannot escape the sandbox
#[tokio::test]
async fn test_fd_manipulation_escape() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try to manipulate file descriptors to escape
try:
    # Try to open /proc/self/fd to find open FDs
    try:
        fds = os.listdir('/proc/self/fd')
        print(f"WARNING: Can list FDs: {fds}")

        for fd_str in fds:
            try:
                fd = int(fd_str)
                # Try to read from each FD
                link = os.readlink(f'/proc/self/fd/{fd}')
                print(f"FD {fd} -> {link}")
                if '/etc' in link or '/root' in link:
                    print(f"SECURITY ISSUE: FD {fd} points to sensitive path: {link}")
            except:
                pass
    except (FileNotFoundError, OSError) as e:
        print(f"EXPECTED: Cannot list /proc/self/fd: {e}")

    # Try dup2 tricks
    try:
        # Open a file in /data
        fd1 = os.open('/data/test.txt', os.O_CREAT | os.O_WRONLY, 0o644)
        os.write(fd1, b'test')

        # Try to dup to stdin/stdout/stderr
        os.dup2(fd1, 100)  # Arbitrary high FD
        os.close(fd1)
        print("Created dup, but this is safe within sandbox")
    except Exception as e:
        print(f"dup2 result: {e}")

except Exception as e:
    print(f"EXPECTED: FD manipulation blocked: {type(e).__name__}: {e}")
"#,
        "fd_manipulation_escape",
    )
    .await;
    assert!(
        safe,
        "FD manipulation should not escape sandbox: {}",
        output
    );
}

// =============================================================================
// Ctypes and Memory Access Tests
// =============================================================================

/// Test that ctypes cannot be used to access host filesystem
#[tokio::test]
async fn test_ctypes_filesystem_bypass() {
    let (safe, output) = run_adversarial_test(
        r#"
# Attack: Use ctypes to call libc filesystem functions directly
try:
    import ctypes

    # Try to load libc
    try:
        libc = ctypes.CDLL("libc.so.6")
        print("WARNING: Loaded libc.so.6")

        # Try to call open() directly
        try:
            libc.open.argtypes = [ctypes.c_char_p, ctypes.c_int]
            libc.open.restype = ctypes.c_int
            fd = libc.open(b"/etc/passwd", 0)  # O_RDONLY
            if fd >= 0:
                print(f"SECURITY ISSUE: Opened /etc/passwd via ctypes, fd={fd}")
                libc.close(fd)
            else:
                print(f"EXPECTED: ctypes open failed with fd={fd}")
        except Exception as e:
            print(f"EXPECTED: ctypes open call failed: {e}")

    except OSError as e:
        print(f"EXPECTED: Cannot load libc: {e}")

except ImportError as e:
    print(f"EXPECTED: ctypes not available: {e}")
except Exception as e:
    print(f"EXPECTED: ctypes attack failed: {type(e).__name__}: {e}")
"#,
        "ctypes_filesystem_bypass",
    )
    .await;
    assert!(
        safe,
        "ctypes should not bypass filesystem sandbox: {}",
        output
    );
}

// =============================================================================
// Subprocess and Process Tests
// =============================================================================

/// Test that subprocess cannot access host filesystem
#[tokio::test]
async fn test_subprocess_filesystem_access() {
    let (safe, output) = run_adversarial_test(
        r#"
# Attack: Try to use subprocess to read host files
try:
    import subprocess

    # Try to cat /etc/passwd
    try:
        result = subprocess.run(['cat', '/etc/passwd'], capture_output=True, timeout=5)
        if result.returncode == 0 and result.stdout:
            print(f"SECURITY ISSUE: Read /etc/passwd via subprocess: {result.stdout[:50]}")
        else:
            print(f"EXPECTED: subprocess cat failed: {result.stderr}")
    except FileNotFoundError:
        print("EXPECTED: cat not found")
    except Exception as e:
        print(f"EXPECTED: subprocess blocked: {type(e).__name__}: {e}")

    # Try to ls /
    try:
        result = subprocess.run(['ls', '/'], capture_output=True, timeout=5)
        if result.returncode == 0 and b'etc' in result.stdout:
            print(f"SECURITY ISSUE: Listed / via subprocess: {result.stdout[:100]}")
        else:
            print(f"EXPECTED: subprocess ls failed or sandboxed")
    except FileNotFoundError:
        print("EXPECTED: ls not found")
    except Exception as e:
        print(f"EXPECTED: subprocess blocked: {type(e).__name__}: {e}")

except ImportError as e:
    print(f"EXPECTED: subprocess not available: {e}")
except Exception as e:
    print(f"EXPECTED: subprocess attack failed: {type(e).__name__}: {e}")
"#,
        "subprocess_filesystem_access",
    )
    .await;
    assert!(safe, "subprocess should not access host fs: {}", output);
}

// =============================================================================
// Import System Manipulation Tests
// =============================================================================

/// Test that importlib cannot load modules from host filesystem
#[tokio::test]
async fn test_importlib_host_filesystem() {
    let (safe, output) = run_adversarial_test(
        r#"
import importlib.util
import sys

# Attack: Try to load a module from an absolute host path
try:
    # Try to create a spec for a file on the host
    spec = importlib.util.spec_from_file_location(
        "evil_module",
        "/etc/passwd"  # Not a Python file, but tests path access
    )
    if spec and spec.loader:
        print("WARNING: Created spec for /etc/passwd")
        try:
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            print("SECURITY ISSUE: Loaded /etc/passwd as module!")
        except Exception as e:
            print(f"EXPECTED: Cannot exec /etc/passwd as module: {e}")
    else:
        print("EXPECTED: Cannot create spec for /etc/passwd")
except Exception as e:
    print(f"EXPECTED: importlib attack blocked: {type(e).__name__}: {e}")

# Try path traversal in module loading
try:
    spec = importlib.util.spec_from_file_location(
        "escape_module",
        "/data/../../../etc/passwd"
    )
    if spec and spec.loader:
        print("WARNING: Created spec with path traversal")
    else:
        print("EXPECTED: Path traversal in spec blocked")
except Exception as e:
    print(f"EXPECTED: Path traversal blocked: {type(e).__name__}: {e}")
"#,
        "importlib_host_filesystem",
    )
    .await;
    assert!(
        safe,
        "importlib should not access host filesystem: {}",
        output
    );
}

// =============================================================================
// Temporary File Tests
// =============================================================================

/// Test that tempfile module is sandboxed
#[tokio::test]
async fn test_tempfile_sandboxed() {
    let (safe, output) = run_adversarial_test(
        r#"
import tempfile
import os

# Check where tempfile wants to create files
print(f"tempfile.gettempdir(): {tempfile.gettempdir()}")

# Try to create a temp file
try:
    with tempfile.NamedTemporaryFile(delete=False) as f:
        temp_path = f.name
        f.write(b'test data')
    print(f"Created temp file at: {temp_path}")

    # Check if it's within the sandbox
    if temp_path.startswith('/data') or temp_path.startswith('/tmp'):
        print("EXPECTED: Temp file is within sandbox or allowed temp dir")
    else:
        print(f"WARNING: Temp file outside expected dirs: {temp_path}")

    # Clean up
    os.unlink(temp_path)

except Exception as e:
    print(f"tempfile result: {type(e).__name__}: {e}")

# Try to explicitly create in /tmp (host)
try:
    with tempfile.NamedTemporaryFile(dir='/tmp', delete=True) as f:
        print(f"Created temp in /tmp: {f.name}")
        # Check if this is actually host /tmp or sandboxed
except Exception as e:
    print(f"EXPECTED: Cannot create temp in /tmp: {type(e).__name__}: {e}")
"#,
        "tempfile_sandboxed",
    )
    .await;
    // tempfile behavior is documented, not a security issue if sandboxed properly
    assert!(safe, "tempfile should be sandboxed: {}", output);
}

// =============================================================================
// Race Condition Tests
// =============================================================================

/// Test that TOCTOU attacks don't work
#[tokio::test]
async fn test_toctou_attack() {
    let (safe, output) = run_adversarial_test(
        r#"
import os
import threading
import time

# Attack: Time-of-check to time-of-use race condition
# Try to swap a symlink between check and use

attack_success = False

def swap_symlink():
    global attack_success
    for _ in range(100):
        try:
            # Try to quickly swap between safe and unsafe targets
            try:
                os.unlink('/data/race_link')
            except:
                pass
            os.symlink('/data/safe.txt', '/data/race_link')
            time.sleep(0.001)
            try:
                os.unlink('/data/race_link')
            except:
                pass
            os.symlink('/etc/passwd', '/data/race_link')
        except:
            pass

# Create a safe file
with open('/data/safe.txt', 'w') as f:
    f.write('safe content')

# Start the swapping thread
thread = threading.Thread(target=swap_symlink)
thread.daemon = True
thread.start()

# Try to read through the link many times
for i in range(100):
    try:
        # Check if link exists and points somewhere "safe"
        if os.path.exists('/data/race_link'):
            with open('/data/race_link', 'r') as f:
                content = f.read(100)
            if 'root:' in content or ':x:' in content:
                print(f"SECURITY ISSUE: TOCTOU race succeeded on iteration {i}!")
                print(f"Read /etc/passwd content: {content[:50]}")
                attack_success = True
                break
    except (FileNotFoundError, PermissionError, OSError):
        pass
    except Exception as e:
        pass

thread.join(timeout=1)

if not attack_success:
    print("EXPECTED: TOCTOU attack did not succeed")
"#,
        "toctou_attack",
    )
    .await;
    assert!(safe, "TOCTOU attacks should be blocked: {}", output);
}

// =============================================================================
// Unicode and Encoding Attacks
// =============================================================================

/// Test that unicode tricks in paths don't bypass sandbox
#[tokio::test]
async fn test_unicode_path_attacks() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Various unicode tricks that might bypass path validation
unicode_paths = [
    '/data/..\\x2f..\\x2fetc/passwd',  # Escaped slashes
    '/data/\u002e\u002e/etc/passwd',   # Unicode dots
    '/data/‥/etc/passwd',              # Two dot leader (U+2025)
    '/data/．．/etc/passwd',           # Fullwidth dots
    '/data/\x2e\x2e/etc/passwd',       # Hex encoded dots
    '/etc\x00/passwd',                 # Null byte injection
    '/data/../etc/passwd\x00.txt',     # Null byte to truncate
]

for path in unicode_paths:
    try:
        normalized = os.path.normpath(path)
        print(f"Path: {repr(path)} -> {repr(normalized)}")

        with open(path, 'r') as f:
            content = f.read(100)
        print(f"SECURITY ISSUE: Read via unicode trick: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError, ValueError) as e:
        print(f"EXPECTED: Unicode path blocked: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked: {type(e).__name__}: {e}")
"#,
        "unicode_path_attacks",
    )
    .await;
    assert!(safe, "Unicode path attacks should be blocked: {}", output);
}

// =============================================================================
// Pathlib Tests
// =============================================================================

/// Test that pathlib operations are sandboxed
#[tokio::test]
async fn test_pathlib_sandboxed() {
    let (safe, output) = run_adversarial_test(
        r#"
from pathlib import Path

# Attack: Use pathlib to escape sandbox
escape_paths = [
    Path('/data') / '..' / 'etc' / 'passwd',
    Path('/data/../etc/passwd'),
    Path('/etc/passwd'),
    Path('/data').parent.parent / 'etc' / 'passwd',
]

for p in escape_paths:
    try:
        # Resolve to absolute
        resolved = p.resolve()
        print(f"Path {p} resolves to {resolved}")

        if resolved.exists():
            content = resolved.read_text()[:100]
            if 'root:' in content:
                print(f"SECURITY ISSUE: Read via pathlib: {content[:50]}")
            else:
                print(f"Read content (might be sandboxed): {content[:30]}")
        else:
            print(f"EXPECTED: {resolved} does not exist")
    except (PermissionError, OSError) as e:
        print(f"EXPECTED: pathlib blocked: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked: {type(e).__name__}: {e}")
"#,
        "pathlib_sandboxed",
    )
    .await;
    assert!(safe, "pathlib should be sandboxed: {}", output);
}

// =============================================================================
// Shutil Tests
// =============================================================================

/// Test that shutil cannot copy files from host
#[tokio::test]
async fn test_shutil_copy_from_host() {
    let (safe, output) = run_adversarial_test(
        r#"
import shutil
import os

# Attack: Try to copy files from host filesystem
try:
    # Try to copy /etc/passwd to sandbox
    shutil.copy('/etc/passwd', '/data/stolen_passwd')

    if os.path.exists('/data/stolen_passwd'):
        with open('/data/stolen_passwd', 'r') as f:
            content = f.read(100)
        if 'root:' in content:
            print(f"SECURITY ISSUE: Copied /etc/passwd: {content[:50]}")
        else:
            print(f"Copied something but not passwd: {content[:30]}")
    else:
        print("EXPECTED: Copy did not create file")

except (FileNotFoundError, PermissionError, OSError) as e:
    print(f"EXPECTED: shutil.copy blocked: {e}")
except Exception as e:
    print(f"EXPECTED: shutil blocked: {type(e).__name__}: {e}")

# Try copytree
try:
    shutil.copytree('/etc', '/data/etc_copy')
    print("SECURITY ISSUE: Copied /etc directory!")
except (FileNotFoundError, PermissionError, OSError) as e:
    print(f"EXPECTED: shutil.copytree blocked: {e}")
except Exception as e:
    print(f"EXPECTED: copytree blocked: {type(e).__name__}: {e}")
"#,
        "shutil_copy_from_host",
    )
    .await;
    assert!(safe, "shutil should not copy from host: {}", output);
}

// =============================================================================
// Glob Tests
// =============================================================================

/// Test that glob cannot enumerate host filesystem
#[tokio::test]
async fn test_glob_host_enumeration() {
    let (safe, output) = run_adversarial_test(
        r#"
import glob

# Attack: Try to enumerate host filesystem
patterns = [
    '/etc/*',
    '/etc/pass*',
    '/home/*',
    '/root/*',
    '/*',
    '/data/../etc/*',
    '/data/../../*',
]

for pattern in patterns:
    try:
        matches = glob.glob(pattern)
        if matches:
            # Check if these are real host files
            sensitive = [m for m in matches if '/etc/' in m or '/root/' in m or '/home/' in m]
            if sensitive:
                print(f"SECURITY ISSUE: glob found sensitive paths: {sensitive[:5]}")
            else:
                print(f"glob {pattern} matched: {matches[:5]}")
        else:
            print(f"EXPECTED: glob {pattern} returned empty")
    except (PermissionError, OSError) as e:
        print(f"EXPECTED: glob blocked: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked: {type(e).__name__}: {e}")
"#,
        "glob_host_enumeration",
    )
    .await;
    assert!(
        safe,
        "glob should not enumerate host filesystem: {}",
        output
    );
}

// =============================================================================
// Os.walk Tests
// =============================================================================

/// Test that os.walk cannot traverse host filesystem
#[tokio::test]
async fn test_os_walk_host_traversal() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try to walk host filesystem
roots_to_try = ['/', '/etc', '/home', '/data/..', '/data/../..']

for root in roots_to_try:
    try:
        count = 0
        sensitive_found = []
        for dirpath, dirnames, filenames in os.walk(root):
            count += 1
            if count > 10:  # Limit iterations
                break
            # Check for sensitive paths
            if '/etc/' in dirpath or '/home/' in dirpath or '/root/' in dirpath:
                sensitive_found.append(dirpath)
            if 'passwd' in filenames or 'shadow' in filenames:
                sensitive_found.append(f"{dirpath}/passwd or shadow")

        if sensitive_found:
            print(f"SECURITY ISSUE: os.walk from {root} found: {sensitive_found[:3]}")
        else:
            print(f"os.walk {root}: traversed {count} dirs, no sensitive paths")
    except (PermissionError, OSError) as e:
        print(f"EXPECTED: os.walk {root} blocked: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked: {type(e).__name__}: {e}")
"#,
        "os_walk_host_traversal",
    )
    .await;
    assert!(
        safe,
        "os.walk should not traverse host filesystem: {}",
        output
    );
}

// =============================================================================
// Mmap Tests
// =============================================================================

/// Test that mmap cannot map host files
#[tokio::test]
async fn test_mmap_host_files() {
    let (safe, output) = run_adversarial_test(
        r#"
import mmap
import os

# Attack: Try to mmap host files
try:
    fd = os.open('/etc/passwd', os.O_RDONLY)
    try:
        mm = mmap.mmap(fd, 0, access=mmap.ACCESS_READ)
        content = mm[:100]
        mm.close()
        print(f"SECURITY ISSUE: mmap'd /etc/passwd: {content[:50]}")
    except Exception as e:
        print(f"EXPECTED: mmap failed: {e}")
    finally:
        os.close(fd)
except (FileNotFoundError, PermissionError, OSError) as e:
    print(f"EXPECTED: Cannot open /etc/passwd for mmap: {e}")
except Exception as e:
    print(f"EXPECTED: mmap attack blocked: {type(e).__name__}: {e}")

# Try mmap on /dev/mem
try:
    fd = os.open('/dev/mem', os.O_RDONLY)
    mm = mmap.mmap(fd, 4096, access=mmap.ACCESS_READ)
    print(f"SECURITY ISSUE: mmap'd /dev/mem!")
    mm.close()
    os.close(fd)
except (FileNotFoundError, PermissionError, OSError) as e:
    print(f"EXPECTED: Cannot mmap /dev/mem: {e}")
except Exception as e:
    print(f"EXPECTED: /dev/mem mmap blocked: {type(e).__name__}: {e}")
"#,
        "mmap_host_files",
    )
    .await;
    assert!(safe, "mmap should not access host files: {}", output);
}

// =============================================================================
// VFS Storage Isolation Tests
// =============================================================================

/// Test that two sessions with different storage are isolated from each other
#[tokio::test]
async fn test_vfs_storage_isolation_between_sessions() {
    let executor = create_executor().await;

    // Create two sessions with different storage instances
    let storage1 = Arc::new(InMemoryStorage::new());
    let storage2 = Arc::new(InMemoryStorage::new());

    let mut session1 = SessionExecutor::new_with_vfs(Arc::clone(&executor), &[], storage1)
        .await
        .expect("Failed to create session1");
    let mut session2 = SessionExecutor::new_with_vfs(Arc::clone(&executor), &[], storage2)
        .await
        .expect("Failed to create session2");

    // Write a file in session1
    let result1 = session1
        .execute(
            r#"
with open('/data/secret.txt', 'w') as f:
    f.write('session1 secret data')
print("Session 1: wrote secret.txt")
"#,
        )
        .run()
        .await;
    assert!(result1.is_ok(), "Session1 write should succeed");

    // Try to read the file in session2 - should NOT exist
    let result2 = session2
        .execute(
            r#"
import os
try:
    with open('/data/secret.txt', 'r') as f:
        content = f.read()
    print(f"ISOLATION FAILURE: session2 read session1's file: {content}")
except FileNotFoundError:
    print("EXPECTED: File not found in session2 (storage is isolated)")
"#,
        )
        .run()
        .await;

    assert!(result2.is_ok(), "Session2 read should execute");
    let output2 = result2.unwrap();
    assert!(
        output2.stdout.contains("EXPECTED: File not found"),
        "Session2 should NOT see session1's files: {}",
        output2.stdout
    );
    assert!(
        !output2.stdout.contains("ISOLATION FAILURE"),
        "Storage should be isolated: {}",
        output2.stdout
    );
}

/// Test that sessions sharing the same storage CAN see each other's files
#[tokio::test]
async fn test_vfs_storage_sharing_between_sessions() {
    let executor = create_executor().await;

    // Create two sessions with the SAME storage instance
    let shared_storage = Arc::new(InMemoryStorage::new());

    let mut session1 =
        SessionExecutor::new_with_vfs(Arc::clone(&executor), &[], Arc::clone(&shared_storage))
            .await
            .expect("Failed to create session1");
    let mut session2 =
        SessionExecutor::new_with_vfs(Arc::clone(&executor), &[], Arc::clone(&shared_storage))
            .await
            .expect("Failed to create session2");

    // Write a file in session1
    let result1 = session1
        .execute(
            r#"
with open('/data/shared.txt', 'w') as f:
    f.write('shared data from session1')
print("Session 1: wrote shared.txt")
"#,
        )
        .run()
        .await;
    assert!(result1.is_ok(), "Session1 write should succeed");

    // Read the file in session2 - SHOULD exist since storage is shared
    let result2 = session2
        .execute(
            r#"
try:
    with open('/data/shared.txt', 'r') as f:
        content = f.read()
    print(f"SUCCESS: session2 read shared file: {content}")
except FileNotFoundError:
    print("UNEXPECTED: File not found (storage should be shared)")
"#,
        )
        .run()
        .await;

    assert!(result2.is_ok(), "Session2 read should execute");
    let output2 = result2.unwrap();
    assert!(
        output2
            .stdout
            .contains("SUCCESS: session2 read shared file"),
        "Session2 should see session1's files when storage is shared: {}",
        output2.stdout
    );
}

/// Test that VFS storage persists across session reset
#[tokio::test]
async fn test_vfs_storage_persists_across_reset() {
    let executor = create_executor().await;
    let storage = Arc::new(InMemoryStorage::new());

    let mut session = SessionExecutor::new_with_vfs(Arc::clone(&executor), &[], storage)
        .await
        .expect("Failed to create session");

    // Write a file
    let result1 = session
        .execute(
            r#"
with open('/data/persist.txt', 'w') as f:
    f.write('data before reset')
print("Wrote file before reset")
"#,
        )
        .run()
        .await;
    assert!(result1.is_ok(), "Write before reset should succeed");

    // Reset the session (Python state cleared, but VFS should persist)
    session.reset(&[]).await.expect("Reset should succeed");

    // Read the file after reset - SHOULD still exist
    let result2 = session
        .execute(
            r#"
try:
    with open('/data/persist.txt', 'r') as f:
        content = f.read()
    print(f"SUCCESS: File persisted across reset: {content}")
except FileNotFoundError:
    print("UNEXPECTED: File not found after reset")
"#,
        )
        .run()
        .await;

    assert!(result2.is_ok(), "Read after reset should execute");
    let output2 = result2.unwrap();
    assert!(
        output2
            .stdout
            .contains("SUCCESS: File persisted across reset"),
        "VFS storage should persist across reset: {}",
        output2.stdout
    );
}

/// Test custom VFS mount path configuration
#[tokio::test]
async fn test_vfs_custom_mount_path() {
    use eryx::VfsConfig;

    let executor = create_executor().await;
    let storage = Arc::new(InMemoryStorage::new());
    let config = VfsConfig::new("/workspace"); // Custom mount path instead of /data

    let mut session =
        SessionExecutor::new_with_vfs_config(Arc::clone(&executor), &[], storage, config)
            .await
            .expect("Failed to create session");

    // Write to custom mount path
    let result1 = session
        .execute(
            r#"
with open('/workspace/test.txt', 'w') as f:
    f.write('custom path works')
print("Wrote to /workspace")
"#,
        )
        .run()
        .await;
    assert!(result1.is_ok(), "Write to custom path should succeed");

    // Read back from custom mount path
    let result2 = session
        .execute(
            r#"
with open('/workspace/test.txt', 'r') as f:
    content = f.read()
print(f"Read from /workspace: {content}")
"#,
        )
        .run()
        .await;

    assert!(result2.is_ok(), "Read from custom path should succeed");
    let output2 = result2.unwrap();
    assert!(
        output2.stdout.contains("custom path works"),
        "Custom mount path should work: {}",
        output2.stdout
    );

    // Verify default /data path doesn't work with custom config
    let result3 = session
        .execute(
            r#"
try:
    with open('/data/test.txt', 'w') as f:
        f.write('should fail')
    print("UNEXPECTED: /data worked with custom config")
except (FileNotFoundError, OSError) as e:
    print(f"EXPECTED: /data not available: {type(e).__name__}")
"#,
        )
        .run()
        .await;

    assert!(result3.is_ok(), "Check /data should execute");
    let output3 = result3.unwrap();
    assert!(
        output3.stdout.contains("EXPECTED: /data not available"),
        "/data should not be available when using custom mount path: {}",
        output3.stdout
    );
}

// =============================================================================
// Additional Security Tests
// =============================================================================

/// Test that rename cannot be used to escape the sandbox
#[tokio::test]
async fn test_rename_escape_attack() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Create a file in the VFS
with open('/data/myfile.txt', 'w') as f:
    f.write('sensitive data')

# Attack: Try to rename/move file outside the sandbox
escape_targets = [
    '/etc/cron.d/evil',
    '/tmp/escaped.txt',
    '/data/../tmp/escaped.txt',
    '/data/../../../tmp/escaped.txt',
    '../../../tmp/escaped.txt',
]

for target in escape_targets:
    try:
        os.rename('/data/myfile.txt', target)
        print(f"SECURITY ISSUE: Renamed file to {target}")
        # Try to recreate for next iteration
        try:
            with open('/data/myfile.txt', 'w') as f:
                f.write('sensitive data')
        except:
            pass
    except (OSError, PermissionError, FileNotFoundError) as e:
        print(f"EXPECTED: Cannot rename to {target}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked: {type(e).__name__}: {e}")

# Also try shutil.move
import shutil
try:
    with open('/data/moveme.txt', 'w') as f:
        f.write('data')
    shutil.move('/data/moveme.txt', '/tmp/escaped.txt')
    print("SECURITY ISSUE: shutil.move escaped sandbox")
except (OSError, PermissionError, FileNotFoundError) as e:
    print(f"EXPECTED: shutil.move blocked: {type(e).__name__}")
except Exception as e:
    print(f"EXPECTED: shutil.move blocked: {type(e).__name__}: {e}")
"#,
        "rename_escape_attack",
    )
    .await;
    assert!(safe, "Rename should not escape sandbox: {}", output);
}

/// Test that special/malicious filenames are handled safely
#[tokio::test]
async fn test_special_filenames() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try various special/malicious filenames
special_names = [
    '.',
    '..',
    '',
    '\x00',
    '\x00evil.txt',
    'file\x00.txt',
    '...',
    '....',
    '. ',
    ' .',
    '.hidden',
    '-',
    '--',
    '-rf',
    '~',
    '~root',
    '*',
    '?',
    '|',
    ';',
    '$(whoami)',
    '`whoami`',
    'a' * 1000,  # Very long filename
    'a' * 10000,  # Extremely long filename
]

for name in special_names:
    path = f'/data/{name}'
    try:
        with open(path, 'w') as f:
            f.write('test')
        # If we could write, try to read back
        try:
            with open(path, 'r') as f:
                content = f.read()
            # This is fine as long as we're still in /data
            if name in ['', '.', '..']:
                # These should not create files with these exact names
                print(f"WARNING: Created file with name {repr(name)}")
            else:
                print(f"OK: Created file with special name {repr(name)[:50]}")
        except:
            print(f"OK: Write succeeded but read failed for {repr(name)[:50]}")
    except (OSError, ValueError, FileNotFoundError) as e:
        print(f"EXPECTED: Cannot use filename {repr(name)[:50]}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked {repr(name)[:50]}: {type(e).__name__}: {str(e)[:50]}")

# Check that . and .. didn't do anything dangerous
try:
    # If /data/.. was created as a file, this is concerning
    if os.path.isfile('/data/..'):
        print("WARNING: /data/.. exists as a file")
    else:
        print("OK: /data/.. is not a file")
except:
    print("OK: Cannot check /data/..")
"#,
        "special_filenames",
    )
    .await;
    assert!(
        safe,
        "Special filenames should be handled safely: {}",
        output
    );
}

/// Test that extremely long paths don't cause issues
#[tokio::test]
async fn test_long_path_attack() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try to create extremely deep nested paths or long filenames
# This could cause stack overflow, memory exhaustion, or buffer overflow

# Very deep path
deep_path = '/data' + '/a' * 500  # 500 levels deep
try:
    os.makedirs(deep_path, exist_ok=True)
    print(f"WARNING: Created path {len(deep_path)} chars deep")
except (OSError, ValueError) as e:
    print(f"EXPECTED: Deep path rejected: {type(e).__name__}")
except RecursionError:
    print(f"EXPECTED: Recursion limit hit for deep path")
except Exception as e:
    print(f"EXPECTED: Blocked: {type(e).__name__}: {e}")

# Very long single component
long_name = 'a' * 100000
try:
    with open(f'/data/{long_name}', 'w') as f:
        f.write('test')
    print("WARNING: Created file with 100k char name")
except (OSError, ValueError) as e:
    print(f"EXPECTED: Long filename rejected: {type(e).__name__}")
except MemoryError:
    print(f"EXPECTED: Memory error for long filename")
except Exception as e:
    print(f"EXPECTED: Blocked: {type(e).__name__}: {e}")

# Many path components
many_components = '/data/' + '/'.join(['a'] * 10000)
try:
    os.makedirs(many_components, exist_ok=True)
    print("WARNING: Created path with 10k components")
except (OSError, ValueError) as e:
    print(f"EXPECTED: Many components rejected: {type(e).__name__}")
except Exception as e:
    print(f"EXPECTED: Blocked: {type(e).__name__}: {e}")

print("Long path tests completed without crash")
"#,
        "long_path_attack",
    )
    .await;
    assert!(safe, "Long paths should be handled safely: {}", output);
}

/// Test that seeking/reading at extreme offsets doesn't cause issues
#[tokio::test]
async fn test_extreme_offset_attack() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Create a small file
with open('/data/small.txt', 'w') as f:
    f.write('small content')

# Attack: Try to read/write at extreme offsets
extreme_offsets = [
    2**31 - 1,      # Max signed 32-bit
    2**31,          # Overflow signed 32-bit
    2**32 - 1,      # Max unsigned 32-bit
    2**32,          # Overflow unsigned 32-bit
    2**63 - 1,      # Max signed 64-bit
    2**62,          # Large but not max
]

for offset in extreme_offsets:
    # Try seek and read
    try:
        with open('/data/small.txt', 'rb') as f:
            f.seek(offset)
            data = f.read(10)
            if data:
                print(f"WARNING: Read data at offset {offset}: {data}")
            else:
                print(f"OK: No data at offset {offset}")
    except (OSError, OverflowError, ValueError) as e:
        print(f"EXPECTED: Cannot seek to {offset}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked seek to {offset}: {type(e).__name__}")

    # Try seek and write
    try:
        with open('/data/sparse.bin', 'wb') as f:
            f.seek(offset)
            f.write(b'X')
        # Check file size
        size = os.path.getsize('/data/sparse.bin')
        if size > 10 * 1024 * 1024 * 1024:  # 10GB
            print(f"SECURITY ISSUE: Created sparse file of {size} bytes")
        else:
            print(f"OK: Sparse file size: {size}")
        os.remove('/data/sparse.bin')
    except (OSError, OverflowError, ValueError, MemoryError) as e:
        print(f"EXPECTED: Cannot write at {offset}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked write at {offset}: {type(e).__name__}")

print("Extreme offset tests completed without crash")
"#,
        "extreme_offset_attack",
    )
    .await;
    assert!(safe, "Extreme offsets should be handled safely: {}", output);
}

/// Test that pickle/marshal deserialization from VFS files is sandboxed
#[tokio::test]
async fn test_pickle_deserialization_attack() {
    let (safe, output) = run_adversarial_test(
        r#"
import pickle
import os

# Attack: Create a malicious pickle that executes code when loaded
# This tests if VFS file content can lead to code execution

class MaliciousPickle:
    def __reduce__(self):
        import os
        return (os.system, ('echo PICKLE_CODE_EXECUTION > /tmp/pwned',))

# Write malicious pickle to VFS
try:
    payload = pickle.dumps(MaliciousPickle())
    with open('/data/malicious.pkl', 'wb') as f:
        f.write(payload)
    print("Wrote malicious pickle")
except Exception as e:
    print(f"Could not write pickle: {e}")

# Load the pickle - this should not execute code on the HOST
try:
    with open('/data/malicious.pkl', 'rb') as f:
        obj = pickle.load(f)
    # If we get here, pickle loaded (might have "executed" in sandbox)
    print("Pickle loaded")
except Exception as e:
    print(f"Pickle load failed: {type(e).__name__}: {e}")

# Check if the attack escaped to host
try:
    with open('/tmp/pwned', 'r') as f:
        content = f.read()
    print(f"SECURITY ISSUE: Pickle attack created /tmp/pwned: {content}")
except FileNotFoundError:
    print("EXPECTED: /tmp/pwned does not exist (pickle didn't escape)")
except PermissionError:
    print("EXPECTED: Cannot check /tmp/pwned (sandboxed)")
except Exception as e:
    print(f"EXPECTED: Check blocked: {type(e).__name__}")

# Also test marshal which can embed code objects
try:
    import marshal
    # Try to create a code object that would execute malicious code
    code = compile('import os; os.system("touch /tmp/marshal_pwned")', '<string>', 'exec')
    payload = marshal.dumps(code)
    with open('/data/malicious.marshal', 'wb') as f:
        f.write(payload)

    with open('/data/malicious.marshal', 'rb') as f:
        loaded_code = marshal.load(f)
    exec(loaded_code)
    print("Marshal code executed")
except Exception as e:
    print(f"Marshal test result: {type(e).__name__}: {str(e)[:50]}")

# Check marshal escape
try:
    with open('/tmp/marshal_pwned', 'r') as f:
        print("SECURITY ISSUE: Marshal attack created /tmp/marshal_pwned")
except FileNotFoundError:
    print("EXPECTED: /tmp/marshal_pwned does not exist")
except Exception as e:
    print(f"EXPECTED: Check blocked: {type(e).__name__}")
"#,
        "pickle_deserialization_attack",
    )
    .await;
    assert!(safe, "Pickle/marshal should not escape sandbox: {}", output);
}

/// Test that double/URL encoding in paths doesn't bypass checks
#[tokio::test]
async fn test_double_encoding_attack() {
    let (safe, output) = run_adversarial_test(
        r#"
import urllib.parse
import os

# Attack: Try double/triple URL encoding to bypass path validation
encoded_paths = [
    # URL encoded
    '/data/%2e%2e/etc/passwd',         # ..
    '/data/%2e%2e%2f%2e%2e/etc/passwd', # ../..
    '/data/..%2f..%2fetc/passwd',       # mixed
    '/data/%2e%2e%2fetc%2fpasswd',      # fully encoded

    # Double encoded
    '/data/%252e%252e/etc/passwd',      # %2e -> .
    '/data/%25%32%65%25%32%65/etc/passwd',  # double encoded ..

    # Overlong UTF-8 sequences (should be rejected)
    '/data/\xc0\xae\xc0\xae/etc/passwd',  # Overlong . encoding
]

for path in encoded_paths:
    # Try both raw and decoded versions
    for p in [path, urllib.parse.unquote(path), urllib.parse.unquote(urllib.parse.unquote(path))]:
        try:
            with open(p, 'r') as f:
                content = f.read(100)
            if 'root:' in content:
                print(f"SECURITY ISSUE: Read /etc/passwd via {repr(path)[:50]}")
            else:
                print(f"Read something via {repr(path)[:50]}: {content[:20]}")
        except (FileNotFoundError, PermissionError, OSError, ValueError) as e:
            pass  # Expected
        except Exception as e:
            pass  # Expected

print("EXPECTED: All encoded path attacks blocked")
"#,
        "double_encoding_attack",
    )
    .await;
    assert!(
        safe,
        "Double encoding should not bypass path checks: {}",
        output
    );
}

/// Test that null byte injection in paths is blocked
#[tokio::test]
async fn test_null_byte_injection() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Null byte injection to truncate paths
# This was a classic attack in C-based systems

null_paths = [
    '/data/safe.txt\x00/etc/passwd',
    '/data/../etc/passwd\x00.txt',
    '/data/\x00../etc/passwd',
    '/etc/passwd\x00',
    '\x00/etc/passwd',
]

for path in null_paths:
    try:
        # Try to read
        with open(path, 'r') as f:
            content = f.read(100)
        if 'root:' in content:
            print(f"SECURITY ISSUE: Null byte attack read passwd via {repr(path)[:50]}")
        else:
            print(f"Read content via null path: {content[:20]}")
    except (ValueError, TypeError) as e:
        print(f"EXPECTED: Null byte rejected: {type(e).__name__}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: Path blocked: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked: {type(e).__name__}")

    try:
        # Try to write
        with open(path, 'w') as f:
            f.write('evil')
        print(f"WARNING: Wrote via null path: {repr(path)[:50]}")
    except (ValueError, TypeError) as e:
        print(f"EXPECTED: Null byte rejected for write: {type(e).__name__}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: Write blocked: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Blocked: {type(e).__name__}")
"#,
        "null_byte_injection",
    )
    .await;
    assert!(safe, "Null byte injection should be blocked: {}", output);
}

/// Test that VFS handles concurrent access safely
#[tokio::test]
async fn test_concurrent_vfs_access() {
    let (safe, output) = run_adversarial_test(
        r#"
import threading
import os
import time

# Attack: Try to cause race conditions or corruption via concurrent access
errors = []
success_count = 0

def writer(thread_id):
    global success_count
    for i in range(20):
        try:
            with open(f'/data/concurrent_{thread_id}.txt', 'w') as f:
                f.write(f'thread {thread_id} iteration {i}' * 100)
            success_count += 1
        except Exception as e:
            errors.append(f"writer {thread_id}: {e}")

def reader(thread_id):
    global success_count
    for i in range(20):
        try:
            path = f'/data/concurrent_{thread_id % 3}.txt'
            if os.path.exists(path):
                with open(path, 'r') as f:
                    data = f.read()
                success_count += 1
        except FileNotFoundError:
            pass  # Expected sometimes
        except Exception as e:
            errors.append(f"reader {thread_id}: {e}")

def deleter():
    for i in range(20):
        try:
            for tid in range(3):
                path = f'/data/concurrent_{tid}.txt'
                if os.path.exists(path):
                    os.remove(path)
        except FileNotFoundError:
            pass
        except Exception as e:
            errors.append(f"deleter: {e}")
        time.sleep(0.01)

# Start threads
threads = []
for i in range(3):
    threads.append(threading.Thread(target=writer, args=(i,)))
    threads.append(threading.Thread(target=reader, args=(i,)))
threads.append(threading.Thread(target=deleter))

for t in threads:
    t.start()
for t in threads:
    t.join(timeout=10)

if errors:
    print(f"Errors during concurrent access: {errors[:5]}")
else:
    print(f"EXPECTED: Concurrent access handled safely, {success_count} operations")

# Check for any corruption
for tid in range(3):
    try:
        path = f'/data/concurrent_{tid}.txt'
        if os.path.exists(path):
            with open(path, 'r') as f:
                data = f.read()
            if 'thread' not in data:
                print(f"WARNING: Possible corruption in {path}")
    except:
        pass

print("Concurrent access test completed")
"#,
        "concurrent_vfs_access",
    )
    .await;
    assert!(safe, "Concurrent VFS access should be safe: {}", output);
}

/// Test that hardlink attacks are blocked
#[tokio::test]
async fn test_hardlink_attack() {
    let (safe, output) = run_adversarial_test(
        r#"
import os

# Attack: Try to create hard links to escape sandbox or access host files
hardlink_attacks = [
    ('/etc/passwd', '/data/passwd_hardlink'),
    ('/etc/shadow', '/data/shadow_hardlink'),
    ('/data/../etc/passwd', '/data/escape_hardlink'),
]

for src, dst in hardlink_attacks:
    try:
        os.link(src, dst)
        print(f"WARNING: Created hardlink {src} -> {dst}")

        # Try to read through the hardlink
        try:
            with open(dst, 'r') as f:
                content = f.read(100)
            if 'root:' in content:
                print(f"SECURITY ISSUE: Read {src} via hardlink: {content[:50]}")
        except Exception as e:
            print(f"Hardlink exists but unreadable: {e}")
    except (OSError, PermissionError) as e:
        print(f"EXPECTED: Cannot create hardlink to {src}: {type(e).__name__}")
    except Exception as e:
        print(f"EXPECTED: Hardlink blocked: {type(e).__name__}: {e}")

# Try creating hardlinks within VFS (should work)
try:
    with open('/data/original.txt', 'w') as f:
        f.write('original content')
    os.link('/data/original.txt', '/data/linked.txt')

    with open('/data/linked.txt', 'r') as f:
        content = f.read()
    if content == 'original content':
        print("OK: Hardlinks within VFS work correctly")
    else:
        print(f"WARNING: Hardlink content mismatch: {content}")
except OSError as e:
    print(f"NOTE: Hardlinks within VFS not supported: {e}")
except Exception as e:
    print(f"NOTE: Hardlink test result: {type(e).__name__}: {e}")
"#,
        "hardlink_attack",
    )
    .await;
    assert!(safe, "Hardlink attacks should be blocked: {}", output);
}
