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
        Arc::new(
            PythonExecutor::from_file(runtime_wasm_path())
                .expect("Failed to load runtime")
                .with_python_stdlib(&stdlib_path)
                .expect("Failed to set stdlib"),
        )
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
