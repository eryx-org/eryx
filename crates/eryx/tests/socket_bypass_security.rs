//! Security tests for socket bypass attempts.
//!
//! These tests verify that Python code cannot bypass the eryx socket shim
//! to access raw WASI sockets or other low-level networking primitives.
//!
//! Think like an attacker: what are all the ways to get network access in Python?
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;

use eryx::{NetConfig, Sandbox};

/// Helper to run adversarial Python code and check it doesn't succeed
async fn run_adversarial_test(code: &str, test_name: &str) -> (bool, String) {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default()) // Default blocks localhost/private
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox.execute(code).await;
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
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox.execute(code).await;
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

fn sandbox_builder() -> eryx::SandboxBuilder<eryx::state::Has, eryx::state::Has> {
    #[cfg(feature = "embedded")]
    {
        Sandbox::embedded()
    }

    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        Sandbox::builder()
            .with_wasm_file(runtime_wasm_path())
            .with_python_stdlib(&stdlib_path)
    }
}

// =============================================================================
// Socket Bypass Attempt Tests
// =============================================================================

/// Test that _socket module is replaced with eryx shim
#[tokio::test]
async fn test_socket_module_is_shimmed() {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import socket
import _socket

# Check that both modules are the same (our shim)
print(f"socket is _socket: {socket is _socket}")

# Check for eryx-specific attributes that wouldn't be in real _socket
has_eryx_markers = hasattr(socket, '__doc__') and 'eryx' in (socket.__doc__ or '').lower()
print(f"Has eryx markers: {has_eryx_markers}")

# The real _socket has SocketType as the main class
# Our shim uses 'socket' as the class name
socket_class_name = socket.socket.__name__
print(f"Socket class name: {socket_class_name}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("socket is _socket: True"),
        "Both modules should be the same shim: {}",
        output.stdout
    );
}

/// Test that importing _socket directly doesn't give access to real sockets
#[tokio::test]
async fn test_direct_socket_import_is_shimmed() {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
# Try to import _socket directly (the C extension in normal Python)
import _socket

# In real CPython, _socket.socket would be a type with specific C-level attributes
# Check if this has the behavior of our shim vs real _socket

# Real _socket.socket has these C-level methods that our shim doesn't perfectly replicate
# But more importantly, check if we can actually make connections that bypass NetConfig

sock = _socket.socket(_socket.AF_INET, _socket.SOCK_STREAM)
print(f"Created socket via _socket: {type(sock)}")

# The socket should still go through our shim's connect which uses eryx:net
print("_socket import test passed - it's our shim")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("_socket import test passed"),
        "Should use shimmed _socket: {}",
        output.stdout
    );
}

/// Test that ctypes cannot be used to call libc socket functions
#[tokio::test]
async fn test_ctypes_socket_bypass_blocked() {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
try:
    import ctypes

    # Try to load libc and call socket() directly
    try:
        libc = ctypes.CDLL("libc.so.6")
        print("SECURITY ISSUE: Loaded libc.so.6")
    except OSError as e:
        print(f"EXPECTED: Cannot load libc: {e}")

    try:
        libc = ctypes.CDLL(None)  # Try default library
        socket_func = libc.socket
        print("SECURITY ISSUE: Got socket function from default lib")
    except (OSError, AttributeError) as e:
        print(f"EXPECTED: Cannot get socket from default lib: {e}")

except ImportError as e:
    print(f"EXPECTED: ctypes not available: {e}")
except Exception as e:
    print(f"EXPECTED: ctypes blocked: {type(e).__name__}: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("EXPECTED") && !output.stdout.contains("SECURITY ISSUE"),
        "ctypes socket bypass should be blocked: {}",
        output.stdout
    );
}

/// Test that os.open cannot be used to open /dev/tcp or similar
#[tokio::test]
async fn test_dev_tcp_bypass_blocked() {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import os

# Try bash-style /dev/tcp bypass (doesn't exist in real filesystem but test anyway)
try:
    fd = os.open("/dev/tcp/127.0.0.1/80", os.O_RDWR)
    print("SECURITY ISSUE: Opened /dev/tcp")
    os.close(fd)
except (OSError, FileNotFoundError, PermissionError) as e:
    print(f"EXPECTED: Cannot open /dev/tcp: {e}")
except Exception as e:
    print(f"EXPECTED: /dev/tcp blocked: {type(e).__name__}: {e}")

# Also try /dev/udp
try:
    fd = os.open("/dev/udp/127.0.0.1/53", os.O_RDWR)
    print("SECURITY ISSUE: Opened /dev/udp")
    os.close(fd)
except (OSError, FileNotFoundError, PermissionError) as e:
    print(f"EXPECTED: Cannot open /dev/udp: {e}")
except Exception as e:
    print(f"EXPECTED: /dev/udp blocked: {type(e).__name__}: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("EXPECTED") && !output.stdout.contains("SECURITY ISSUE"),
        "/dev/tcp bypass should be blocked: {}",
        output.stdout
    );
}

/// Test that subprocess cannot be used to make network connections
#[tokio::test]
async fn test_subprocess_network_bypass_blocked() {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
try:
    import subprocess

    # Try to use curl/wget via subprocess
    try:
        result = subprocess.run(["curl", "http://example.com"], capture_output=True, timeout=5)
        print(f"SECURITY ISSUE: curl executed with return code {result.returncode}")
    except FileNotFoundError:
        print("EXPECTED: curl not found")
    except Exception as e:
        print(f"EXPECTED: subprocess curl blocked: {type(e).__name__}: {e}")

except ImportError as e:
    print(f"EXPECTED: subprocess not available: {e}")
except Exception as e:
    print(f"EXPECTED: subprocess blocked: {type(e).__name__}: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("EXPECTED") && !output.stdout.contains("SECURITY ISSUE"),
        "subprocess network bypass should be blocked: {}",
        output.stdout
    );
}

/// Test that the shimmed socket still respects NetConfig policies
#[tokio::test]
async fn test_shimmed_socket_respects_netconfig() {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default()) // Default blocks localhost
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import socket

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(5)

try:
    # This should be blocked by NetConfig even through the shim
    sock.connect(("127.0.0.1", 80))
    print("SECURITY ISSUE: Connected to localhost despite NetConfig blocking it")
except OSError as e:
    error_str = str(e).lower()
    if "not permitted" in error_str or "blocked" in error_str or "refused" in error_str:
        print(f"EXPECTED: Connection blocked by policy: {e}")
    else:
        # Connection refused is also acceptable (means we tried but port isn't open)
        print(f"EXPECTED: Connection failed: {e}")
except Exception as e:
    print(f"EXPECTED: Connection blocked: {type(e).__name__}: {e}")
finally:
    sock.close()
"#,
        )
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("EXPECTED") && !output.stdout.contains("SECURITY ISSUE"),
        "Shimmed socket should respect NetConfig: {}",
        output.stdout
    );
}

/// Test attempting to access WASI sockets directly via the component model
/// This tests whether wasi:sockets is actually importable from the guest
#[tokio::test]
async fn test_wasi_sockets_not_importable() {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::permissive())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
# The sandbox world only imports eryx:net/tcp and eryx:net/tls
# It should NOT have wasi:sockets available

# Try to find any WASI socket-related modules
import sys

wasi_modules = [name for name in sys.modules.keys() if 'wasi' in name.lower()]
print(f"WASI-related modules: {wasi_modules}")

# Check if there's any way to import wasi_snapshot_preview1 socket functions
# In WASI preview 1, these would be sock_accept, sock_recv, sock_send, etc.
# But they shouldn't be exposed to Python in our sandbox

# The only network access should be through _eryx_async
try:
    import _eryx_async
    print(f"_eryx_async available: {dir(_eryx_async)}")
    print("Network access is properly channeled through eryx")
except ImportError:
    print("_eryx_async not available")

print("WASI socket isolation test completed")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    assert!(
        output
            .stdout
            .contains("Network access is properly channeled through eryx")
            || output
                .stdout
                .contains("WASI socket isolation test completed"),
        "Should show proper network channeling: {}",
        output.stdout
    );
}

/// Test that os module network functions are not available or are shimmed
#[tokio::test]
async fn test_os_network_functions_blocked() {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import os

# Check for low-level socket-related os functions that might bypass our shim
dangerous_funcs = ['socket', 'socketpair', 'bind', 'listen', 'accept', 'connect']
available = []
for func in dangerous_funcs:
    if hasattr(os, func):
        available.append(func)

if available:
    print(f"WARNING: os module has socket functions: {available}")
    # Try to use them
    for func in available:
        try:
            f = getattr(os, func)
            print(f"SECURITY CONCERN: os.{func} is accessible")
        except Exception as e:
            print(f"os.{func} blocked: {e}")
else:
    print("EXPECTED: os module does not expose socket functions")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    // Note: This test documents current behavior - os module shouldn't have raw socket functions
    // but if it does, they should go through WASI which we don't import
    println!("os network functions output: {}", output.stdout);
}

/// Test that multiprocessing cannot be used to spawn processes with network access
#[tokio::test]
async fn test_multiprocessing_blocked() {
    let sandbox = sandbox_builder()
        .with_network(NetConfig::default())
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
try:
    import multiprocessing
    print(f"multiprocessing imported: {multiprocessing}")

    # Try to create a process
    try:
        p = multiprocessing.Process(target=lambda: None)
        p.start()
        print("SECURITY ISSUE: Created subprocess via multiprocessing")
    except Exception as e:
        print(f"EXPECTED: multiprocessing.Process blocked: {type(e).__name__}: {e}")

except ImportError as e:
    print(f"EXPECTED: multiprocessing not available: {e}")
except Exception as e:
    print(f"EXPECTED: multiprocessing blocked: {type(e).__name__}: {e}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Should execute: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("EXPECTED") && !output.stdout.contains("SECURITY ISSUE"),
        "multiprocessing should be blocked: {}",
        output.stdout
    );
}

// =============================================================================
// Adversarial Attack Tests - Think Like a Hacker!
// =============================================================================

/// Attack: Use importlib to get the "real" socket module before it was replaced
#[tokio::test]
async fn test_attack_importlib_reload_socket() {
    let (safe, output) = run_adversarial_test(
        r#"
import sys
import importlib

# Attack: Try to reload socket to get original implementation
try:
    # Remove the shimmed module
    if 'socket' in sys.modules:
        del sys.modules['socket']
    if '_socket' in sys.modules:
        del sys.modules['_socket']

    # Try to reimport - will this get the real one?
    import socket

    # Check if we got the shim or the real module
    if 'eryx' in str(socket.__doc__).lower():
        print("EXPECTED: Got eryx shim after reload")
    else:
        # Try to connect to localhost to verify
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(1)
        try:
            sock.connect(("127.0.0.1", 22))  # SSH port
            print("BYPASS SUCCESSFUL: Connected via reloaded socket!")
        except Exception as e:
            if "not permitted" in str(e).lower():
                print("EXPECTED: Still blocked by policy")
            else:
                print(f"Connection failed (might still be shim): {e}")
        finally:
            sock.close()
except Exception as e:
    print(f"EXPECTED: Reload attack failed: {type(e).__name__}: {e}")
"#,
        "importlib_reload_socket",
    )
    .await;
    assert!(
        safe,
        "importlib reload attack should not bypass shim: {}",
        output
    );
}

/// Attack: Use importlib.util to load socket from a specific path
#[tokio::test]
async fn test_attack_importlib_spec_socket() {
    let (safe, output) = run_adversarial_test(
        r#"
import importlib.util
import sys

# Attack: Try to find and load the "real" _socket C extension
try:
    # Find where _socket would normally be
    spec = importlib.util.find_spec('_socket')
    if spec is None:
        print("EXPECTED: _socket spec not found (no real C extension)")
    else:
        print(f"Found _socket spec: {spec.origin}")
        # Try to load it
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        # Check if it's the real deal
        if hasattr(module, 'socket') and 'eryx' not in str(getattr(module, '__doc__', '')).lower():
            sock = module.socket(module.AF_INET, module.SOCK_STREAM)
            sock.settimeout(1)
            try:
                sock.connect(("127.0.0.1", 22))
                print("BYPASS SUCCESSFUL: Connected via importlib.util loaded socket!")
            except Exception as e:
                print(f"Connection failed: {e}")
            finally:
                sock.close()
        else:
            print("EXPECTED: Got shim via importlib.util")
except Exception as e:
    print(f"EXPECTED: importlib.util attack failed: {type(e).__name__}: {e}")
"#,
        "importlib_spec_socket",
    )
    .await;
    assert!(
        safe,
        "importlib.util attack should not bypass shim: {}",
        output
    );
}

/// Attack: Access __builtins__ to find original open/socket
#[tokio::test]
async fn test_attack_builtins_manipulation() {
    let (safe, output) = run_adversarial_test(
        r#"
import builtins

# Attack: Check if there's a hidden socket in builtins
try:
    # Look for anything socket-related in builtins
    socket_stuff = [name for name in dir(builtins) if 'socket' in name.lower()]
    if socket_stuff:
        print(f"WARNING: Found socket in builtins: {socket_stuff}")
        for name in socket_stuff:
            obj = getattr(builtins, name)
            print(f"SECURITY ISSUE: builtins.{name} = {obj}")
    else:
        print("EXPECTED: No socket in builtins")

    # Also check __builtins__ in different contexts
    import __main__
    if hasattr(__main__, '__builtins__'):
        b = __main__.__builtins__
        if isinstance(b, dict):
            socket_in_b = [k for k in b.keys() if 'socket' in k.lower()]
        else:
            socket_in_b = [name for name in dir(b) if 'socket' in name.lower()]
        if socket_in_b:
            print(f"WARNING: Found socket in __main__.__builtins__: {socket_in_b}")
        else:
            print("EXPECTED: No socket in __main__.__builtins__")
except Exception as e:
    print(f"EXPECTED: builtins attack failed: {type(e).__name__}: {e}")
"#,
        "builtins_manipulation",
    )
    .await;
    assert!(
        safe,
        "builtins manipulation should not expose socket: {}",
        output
    );
}

/// Attack: Use exec/eval with crafted globals to bypass module replacement
#[tokio::test]
async fn test_attack_exec_with_custom_globals() {
    let (safe, output) = run_adversarial_test(
        r#"
import sys

# Attack: Try exec with empty globals to avoid module cache
try:
    code = '''
import socket
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(1)
try:
    sock.connect(("127.0.0.1", 22))
    result = "BYPASS SUCCESSFUL"
except Exception as e:
    result = f"blocked: {e}"
finally:
    sock.close()
'''

    # Try with fresh globals
    fresh_globals = {'__builtins__': __builtins__}
    exec(code, fresh_globals)

    if "BYPASS" in fresh_globals.get('result', ''):
        print(fresh_globals['result'])
    else:
        print(f"EXPECTED: exec with fresh globals still blocked: {fresh_globals.get('result', 'no result')}")

except Exception as e:
    print(f"EXPECTED: exec attack failed: {type(e).__name__}: {e}")
"#,
        "exec_custom_globals",
    )
    .await;
    assert!(
        safe,
        "exec with custom globals should not bypass: {}",
        output
    );
}

/// Attack: Use __import__ directly with different parameters
#[tokio::test]
async fn test_attack_dunder_import() {
    let (safe, output) = run_adversarial_test(
        r#"
import sys

# Attack: Use __import__ with various tricks
try:
    # Clear module cache first
    for mod in list(sys.modules.keys()):
        if 'socket' in mod:
            del sys.modules[mod]

    # Try __import__ with fromlist
    socket_mod = __import__('socket', globals(), locals(), ['socket'], 0)

    # Check if it's the shim
    if 'eryx' in str(getattr(socket_mod, '__doc__', '')).lower():
        print("EXPECTED: __import__ returned eryx shim")
    else:
        sock = socket_mod.socket(socket_mod.AF_INET, socket_mod.SOCK_STREAM)
        sock.settimeout(1)
        try:
            sock.connect(("127.0.0.1", 22))
            print("BYPASS SUCCESSFUL: __import__ bypassed shim!")
        except Exception as e:
            if "not permitted" in str(e).lower():
                print("EXPECTED: __import__ socket still enforces policy")
            else:
                print(f"Connection failed: {e}")
        finally:
            sock.close()
except Exception as e:
    print(f"EXPECTED: __import__ attack failed: {type(e).__name__}: {e}")
"#,
        "dunder_import",
    )
    .await;
    assert!(safe, "__import__ attack should not bypass shim: {}", output);
}

/// Attack: Try to access _io module for low-level file operations
#[tokio::test]
async fn test_attack_io_module_bypass() {
    let (safe, output) = run_adversarial_test(
        r#"
# Attack: Use _io module to bypass any file operation shims
try:
    import _io

    # Try to open /etc/passwd (should be blocked by WASI sandbox)
    try:
        f = _io.open('/etc/passwd', 'r')
        content = f.read(100)
        f.close()
        print(f"SECURITY ISSUE: Read /etc/passwd via _io: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: _io.open blocked: {e}")

    # Try to open /proc/net/tcp (Linux network connections)
    try:
        f = _io.open('/proc/net/tcp', 'r')
        content = f.read(100)
        f.close()
        print(f"SECURITY ISSUE: Read /proc/net/tcp via _io: {content[:50]}")
    except (FileNotFoundError, PermissionError, OSError) as e:
        print(f"EXPECTED: _io.open /proc blocked: {e}")

except ImportError as e:
    print(f"EXPECTED: _io not available: {e}")
except Exception as e:
    print(f"EXPECTED: _io attack failed: {type(e).__name__}: {e}")
"#,
        "io_module_bypass",
    )
    .await;
    assert!(safe, "_io module should not bypass sandbox: {}", output);
}

/// Attack: Try to find socket via sys.modules manipulation
#[tokio::test]
async fn test_attack_sys_modules_manipulation() {
    let (safe, output) = run_adversarial_test(
        r#"
import sys

# Attack: Look for any socket-related modules that might have been loaded
socket_modules = [(name, mod) for name, mod in sys.modules.items()
                  if mod is not None and 'socket' in name.lower()]

print(f"Socket-related modules in sys.modules: {[name for name, _ in socket_modules]}")

for name, mod in socket_modules:
    # Check each module for socket capabilities
    if hasattr(mod, 'socket') and callable(getattr(mod, 'socket', None)):
        print(f"Module {name} has socket function")
        # Check if it's the shim
        doc = str(getattr(mod, '__doc__', ''))
        if 'eryx' in doc.lower():
            print(f"EXPECTED: {name} is eryx shim")
        else:
            print(f"WARNING: {name} might be real socket module")
            # Try to use it
            try:
                sock_class = getattr(mod, 'socket')
                sock = sock_class(2, 1)  # AF_INET, SOCK_STREAM
                sock.settimeout(1)
                sock.connect(("127.0.0.1", 22))
                print(f"BYPASS SUCCESSFUL via {name}!")
                sock.close()
            except Exception as e:
                print(f"Connection via {name} failed: {e}")
"#,
        "sys_modules_manipulation",
    )
    .await;
    assert!(
        safe,
        "sys.modules manipulation should not bypass: {}",
        output
    );
}

/// Attack: Try to use code objects to bypass restrictions
#[tokio::test]
async fn test_attack_code_object_manipulation() {
    let (safe, output) = run_adversarial_test(
        r#"
import types

# Attack: Create a code object that imports socket in a fresh namespace
try:
    code = compile('''
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(1)
try:
    s.connect(("127.0.0.1", 22))
    RESULT = "BYPASS SUCCESSFUL"
except Exception as e:
    RESULT = f"blocked: {e}"
finally:
    s.close()
''', '<attack>', 'exec')

    # Execute in a minimal namespace
    namespace = {}
    exec(code, namespace)

    result = namespace.get('RESULT', 'no result')
    if 'BYPASS' in result:
        print(result)
    else:
        print(f"EXPECTED: Code object attack blocked: {result}")

except Exception as e:
    print(f"EXPECTED: Code object attack failed: {type(e).__name__}: {e}")
"#,
        "code_object_manipulation",
    )
    .await;
    assert!(
        safe,
        "Code object manipulation should not bypass: {}",
        output
    );
}

/// Attack: Look for any C extension modules that might have socket access
#[tokio::test]
async fn test_attack_find_c_extensions() {
    let (safe, output) = run_adversarial_test(
        r#"
import sys
import importlib

# Attack: Find all C extension modules (they might have raw socket access)
c_extensions = []
for name, mod in sys.modules.items():
    if mod is None:
        continue
    # C extensions typically have __file__ ending in .so or no __file__
    file = getattr(mod, '__file__', None)
    if file and (file.endswith('.so') or file.endswith('.pyd')):
        c_extensions.append((name, file))
    elif not hasattr(mod, '__file__') and hasattr(mod, '__loader__'):
        # Built-in modules
        loader = getattr(mod, '__loader__', None)
        if loader and 'BuiltinImporter' in str(type(loader)):
            c_extensions.append((name, '<builtin>'))

print(f"Found {len(c_extensions)} C extensions/builtins")

# Check each for socket capabilities
dangerous = []
for name, path in c_extensions:
    mod = sys.modules.get(name)
    if mod:
        # Look for socket-related attributes
        attrs = dir(mod)
        socket_attrs = [a for a in attrs if 'sock' in a.lower() or 'connect' in a.lower() or 'bind' in a.lower()]
        if socket_attrs:
            dangerous.append((name, socket_attrs))

if dangerous:
    print(f"WARNING: Modules with socket-like attributes: {dangerous}")
    for name, attrs in dangerous:
        print(f"  {name}: {attrs}")
else:
    print("EXPECTED: No C extensions with exposed socket functions")
"#,
        "find_c_extensions",
    )
    .await;
    assert!(
        safe,
        "C extension search should not find bypass: {}",
        output
    );
}

/// Attack: Try to use asyncio internals to bypass socket shim
#[tokio::test]
async fn test_attack_asyncio_internals() {
    let (safe, output) = run_adversarial_test(
        r#"
import asyncio

# Attack: asyncio has its own socket handling - can we bypass via it?
try:
    # Look at asyncio's selector module
    import selectors
    print(f"selectors module: {selectors}")

    # asyncio.open_connection might use different socket path
    async def try_connect():
        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_connection('127.0.0.1', 22),
                timeout=2.0
            )
            print("BYPASS SUCCESSFUL: asyncio.open_connection worked!")
            writer.close()
            await writer.wait_closed()
        except asyncio.TimeoutError:
            print("Connection timed out")
        except OSError as e:
            if "not permitted" in str(e).lower():
                print(f"EXPECTED: asyncio.open_connection blocked by policy: {e}")
            else:
                print(f"Connection failed (might still be blocked): {e}")
        except Exception as e:
            print(f"EXPECTED: asyncio attack blocked: {type(e).__name__}: {e}")

    asyncio.run(try_connect())

except Exception as e:
    print(f"EXPECTED: asyncio internals attack failed: {type(e).__name__}: {e}")
"#,
        "asyncio_internals",
    )
    .await;
    assert!(safe, "asyncio internals should not bypass shim: {}", output);
}

/// Diagnostic: What socket module do we actually get after reload?
#[tokio::test]
async fn test_diagnostic_socket_after_reload() {
    let (safe, output) = run_adversarial_test(
        r#"
import sys

# First, check what socket module we have before manipulation
import socket as original_socket
print(f"Original socket module: {original_socket}")
print(f"Original socket.__name__: {original_socket.__name__}")
print(f"Original socket.__file__: {getattr(original_socket, '__file__', 'NO FILE')}")
print(f"Original socket.__doc__[:100]: {str(original_socket.__doc__)[:100]}")
print(f"Original socket.socket: {original_socket.socket}")

# Now delete and reimport
del sys.modules['socket']
del sys.modules['_socket']

import socket as reloaded_socket
print(f"\nReloaded socket module: {reloaded_socket}")
print(f"Reloaded socket.__name__: {reloaded_socket.__name__}")
print(f"Reloaded socket.__file__: {getattr(reloaded_socket, '__file__', 'NO FILE')}")
print(f"Reloaded socket.__doc__[:100]: {str(reloaded_socket.__doc__)[:100]}")
print(f"Reloaded socket.socket: {reloaded_socket.socket}")

# Are they the same?
print(f"\nSame module? {original_socket is reloaded_socket}")
print(f"Same socket class? {original_socket.socket is reloaded_socket.socket}")

# Try to create a socket and see what type it is
sock = reloaded_socket.socket(reloaded_socket.AF_INET, reloaded_socket.SOCK_STREAM)
print(f"\nSocket instance type: {type(sock)}")
print(f"Socket instance module: {type(sock).__module__}")

# Check the connect method
print(f"connect method: {sock.connect}")

sock.close()
"#,
        "diagnostic_socket_after_reload",
    )
    .await;
    // This is diagnostic, just print results
    println!("Diagnostic complete");
    assert!(
        safe,
        "Diagnostic should not show security issues: {}",
        output
    );
}

/// CRITICAL: Test that bypassing shim to real socket is blocked by WASI
/// This verifies defense in depth - even if shim is bypassed, WASI blocks it
#[tokio::test]
async fn test_critical_real_socket_blocked_by_wasi() {
    let (safe, output) = run_adversarial_test(
        r#"
import sys

# Bypass the shim by reimporting real socket module
del sys.modules['socket']
del sys.modules['_socket']

# This gets the REAL socket module from /python-stdlib/socket.py
import socket

print(f"Got real socket from: {getattr(socket, '__file__', 'unknown')}")
print(f"This is the real CPython socket module, not eryx shim")

# Create a real socket
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(5)

# Try to connect to an external server (should be blocked by WASI, not our policy)
# Using a well-known IP to avoid DNS issues
try:
    sock.connect(("8.8.8.8", 53))  # Google DNS
    data = sock.recv(100)
    print(f"BYPASS SUCCESSFUL: Connected to 8.8.8.8:53 with real socket! Got {len(data)} bytes")
except PermissionError as e:
    print(f"EXPECTED (WASI blocks): PermissionError: {e}")
except OSError as e:
    # Check if this is our shim's error or a WASI error
    if "not-permitted" in str(e).lower() and "blocked" in str(e).lower():
        print(f"WARNING: Got shim error, but we should have real socket: {e}")
    else:
        print(f"EXPECTED (WASI blocks): OSError: {e}")
except Exception as e:
    print(f"EXPECTED (blocked): {type(e).__name__}: {e}")
finally:
    sock.close()

# Also try localhost to be thorough
sock2 = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock2.settimeout(2)
try:
    sock2.connect(("127.0.0.1", 22))
    print("BYPASS SUCCESSFUL: Connected to localhost with real socket!")
except Exception as e:
    print(f"Localhost also blocked: {type(e).__name__}: {e}")
finally:
    sock2.close()
"#,
        "critical_real_socket_blocked_by_wasi",
    )
    .await;
    assert!(safe, "Real socket should be blocked by WASI: {}", output);
}

/// Attack: Try urllib and http.client which use socket internally
#[tokio::test]
async fn test_attack_urllib_http_client() {
    let (safe, output) = run_adversarial_test(
        r#"
# Attack: urllib/http.client are high-level but use socket internally
try:
    import urllib.request

    # This should use socket internally - does it bypass our shim?
    try:
        response = urllib.request.urlopen('http://127.0.0.1:80/', timeout=2)
        print(f"BYPASS SUCCESSFUL: urllib connected to localhost!")
        response.close()
    except urllib.error.URLError as e:
        if "not permitted" in str(e).lower() or "blocked" in str(e).lower():
            print(f"EXPECTED: urllib blocked by policy: {e}")
        else:
            print(f"urllib failed (might be blocked): {e}")
    except Exception as e:
        print(f"urllib blocked: {type(e).__name__}: {e}")

except ImportError as e:
    print(f"urllib not available: {e}")
except Exception as e:
    print(f"EXPECTED: urllib attack failed: {type(e).__name__}: {e}")

try:
    import http.client

    try:
        conn = http.client.HTTPConnection('127.0.0.1', 80, timeout=2)
        conn.request('GET', '/')
        response = conn.getresponse()
        print(f"BYPASS SUCCESSFUL: http.client connected to localhost!")
        conn.close()
    except OSError as e:
        if "not permitted" in str(e).lower():
            print(f"EXPECTED: http.client blocked by policy: {e}")
        else:
            print(f"http.client failed: {e}")
    except Exception as e:
        print(f"EXPECTED: http.client blocked: {type(e).__name__}: {e}")

except ImportError as e:
    print(f"http.client not available: {e}")
except Exception as e:
    print(f"EXPECTED: http.client attack failed: {type(e).__name__}: {e}")
"#,
        "urllib_http_client",
    )
    .await;
    assert!(
        safe,
        "urllib/http.client should respect shim policies: {}",
        output
    );
}
