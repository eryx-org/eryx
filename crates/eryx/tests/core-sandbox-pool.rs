//! Regression tests for sandbox construction, execution handles and the pool.

#![cfg(feature = "embedded")]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::io::Write;
use std::pin::Pin;

use eryx::{
    CallbackError, ErrorKind, PackageFormat, PoolConfig, PoolError, Sandbox, SandboxPool,
    TypedCallback,
};

/// A pure-Python wheel containing a single `corepkg` module.
fn tiny_wheel() -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut buf);
    writer
        .start_file(
            "corepkg/__init__.py",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    writer.write_all(b"VALUE = 42\n").unwrap();
    writer.finish().unwrap();
    buf.into_inner()
}

#[tokio::test]
async fn executor_keeps_packages_after_sandbox_is_dropped() {
    let sandbox = Sandbox::embedded()
        .with_package_bytes(&tiny_wheel(), PackageFormat::Wheel, "corepkg")
        .unwrap()
        .build()
        .unwrap();
    let executor = sandbox.executor();
    drop(sandbox);

    let output = executor
        .execute("import corepkg\nprint(corepkg.VALUE)")
        .run()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[tokio::test]
async fn execution_handle_reports_completion() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let handle = sandbox.execute_cancellable("x = 1 + 1");
    let token = handle.cancellation_token();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while handle.is_running() {
        assert!(
            std::time::Instant::now() < deadline,
            "still running after completing"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!token.is_cancelled());
    handle.wait().await.unwrap();
    // Waiting to completion must not cancel the shared token.
    assert!(!token.is_cancelled());
}

#[tokio::test]
async fn dropping_execution_handle_cancels_execution() {
    let sandbox = Sandbox::embedded().build().unwrap();
    let handle = sandbox.execute_cancellable("while True: pass");
    let token = handle.cancellation_token();
    drop(handle);
    assert!(token.is_cancelled());
}

struct PanickingCallback;

impl TypedCallback for PanickingCallback {
    type Args = ();

    fn name(&self) -> &str {
        "boom"
    }

    fn description(&self) -> &str {
        "Panics"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, CallbackError>> + Send + '_>> {
        Box::pin(async { panic!("callback panicked") })
    }
}

#[tokio::test]
async fn panicking_callback_fails_the_execution() {
    let sandbox = Sandbox::embedded()
        .with_callback(PanickingCallback)
        .build()
        .unwrap();
    // The script swallows the callback error, so only the host can tell that
    // the callback handler died.
    let result = sandbox
        .execute("try:\n    await boom()\nexcept BaseException:\n    pass")
        .await;
    assert!(
        matches!(result.as_ref().map_err(|e| e.kind()), Err(ErrorKind::Execution(msg)) if msg.contains("callback handler")),
        "expected a callback handler failure, got {result:?}"
    );
}

#[tokio::test]
async fn secret_names_needing_escapes_do_not_break_execution() {
    let sandbox = Sandbox::embedded()
        .with_secret("ODD\u{1b}\"NAME\n", "real-value", vec![])
        .scrub_stdout(false)
        .build()
        .unwrap();
    let result = sandbox
        .execute("import os\nprint(os.environ['ODD\\x1b\"NAME\\n'].startswith('ERYX_SECRET_PLACEHOLDER_'))")
        .await
        .unwrap();
    assert_eq!(result.stdout_text().trim(), "True");
}

fn small_pool() -> PoolConfig {
    PoolConfig::default().with_max_size(1).with_min_idle(1)
}

#[tokio::test]
async fn pool_rejects_builder_settings_it_would_drop() {
    let builder = Sandbox::embedded().with_secret("API_KEY", "real", vec![]);
    let err = SandboxPool::new(builder, small_pool()).await.unwrap_err();
    assert!(
        matches!(&err, PoolError::Creation(msg) if msg.contains("secrets")),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn pool_accepts_precompiled_bytes() {
    let cwasm = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../eryx-runtime/runtime.cwasm"
    ))
    .unwrap();
    // SAFETY: the workspace's own precompiled runtime.
    let builder = unsafe { Sandbox::builder().with_precompiled_bytes(cwasm) }
        .with_embedded_stdlib()
        .unwrap();
    let pool = SandboxPool::new(builder, small_pool()).await.unwrap();
    let sandbox = pool.acquire().await.unwrap();
    let result = sandbox.execute("print(6 * 7)").await.unwrap();
    assert_eq!(result.stdout_text().trim(), "42");
}

#[tokio::test]
async fn pool_acquire_timeout_covers_sandbox_creation() {
    let config = PoolConfig::default()
        .with_max_size(1)
        .with_min_idle(0)
        .with_acquire_timeout(std::time::Duration::from_millis(100));
    let pool = SandboxPool::with_builder(
        || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            Sandbox::embedded().build()
        },
        config,
    )
    .await
    .unwrap();

    let start = std::time::Instant::now();
    let result = pool.acquire().await;
    assert!(matches!(result, Err(PoolError::Timeout(_))), "{result:?}");
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
}

#[tokio::test]
async fn pool_try_acquire_never_creates_a_sandbox() {
    let config = PoolConfig::default().with_max_size(1).with_min_idle(0);
    let pool = SandboxPool::new(Sandbox::embedded(), config).await.unwrap();
    assert!(pool.try_acquire().unwrap().is_none());
    assert_eq!(pool.stats().total_creations, 0);
}

/// A session on a pooled sandbox reads and writes the storage set with
/// `with_vfs_storage`, so the caller can restore and save its files.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn session_uses_the_requests_vfs_storage() {
    use eryx::session::{InProcessSession, Session};
    use eryx::vfs::{InMemoryStorage, VfsStorage};
    use std::sync::Arc;

    let pool = SandboxPool::new(Sandbox::embedded(), small_pool())
        .await
        .unwrap();
    let storage = Arc::new(InMemoryStorage::new());
    storage.mkdir("/data").await.unwrap();
    storage.write("/data/in.txt", b"restored").await.unwrap();

    let sandbox = pool
        .acquire()
        .await
        .unwrap()
        .with_vfs_storage(storage.clone());
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    session
        .execute("open('/data/out.txt', 'w').write(open('/data/in.txt').read() + '!')")
        .await
        .unwrap();
    let result = session
        .execute("print(open('/data/out.txt').read())")
        .await
        .unwrap();
    assert_eq!(result.stdout_text().trim(), "restored!");
    assert_eq!(storage.read("/data/out.txt").await.unwrap(), b"restored!");
}

/// A built sandbox given storage with `set_vfs_storage` uses it for
/// `execute` and for sessions, so a caller can keep files without a pool.
#[cfg(feature = "unstable-vfs")]
#[tokio::test]
async fn built_sandbox_uses_its_vfs_storage() {
    use eryx::session::{InProcessSession, Session};
    use eryx::vfs::{InMemoryStorage, VfsStorage};
    use std::sync::Arc;

    let storage = Arc::new(InMemoryStorage::new());
    storage.mkdir("/data").await.unwrap();
    storage.write("/data/in.txt", b"restored").await.unwrap();
    let mut sandbox = Sandbox::embedded().build().unwrap();
    sandbox.set_vfs_storage(storage.clone());

    let result = sandbox
        .execute("open('/data/run.txt', 'w').write(open('/data/in.txt').read() + '!')")
        .await
        .unwrap();
    assert!(result.stderr_text().is_empty(), "{}", result.stderr_text());
    assert_eq!(storage.read("/data/run.txt").await.unwrap(), b"restored!");

    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    session
        .execute("open('/data/session.txt', 'w').write(open('/data/run.txt').read() + '?')")
        .await
        .unwrap();
    assert_eq!(
        storage.read("/data/session.txt").await.unwrap(),
        b"restored!?"
    );
}

/// A pooled sandbox takes networking and secrets for one request, a secret
/// limited to a header is only sent there, and the next request on the same
/// sandbox has neither.
#[tokio::test]
async fn pooled_sandbox_takes_network_and_secrets_per_request() {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(Mutex::new(Vec::<u8>::new()));
    let server_received = Arc::clone(&received);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 65536];
            if let Ok(n) = socket.read(&mut buf).await {
                server_received.lock().unwrap().extend_from_slice(&buf[..n]);
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                    .await;
            }
        }
    });

    let pool = SandboxPool::new(Sandbox::embedded(), small_pool())
        .await
        .unwrap();
    let secret =
        eryx::SecretConfig::new("real-key", vec!["127.0.0.1".into()]).only_in_header("X-Api-Key");
    let sandbox = pool
        .acquire()
        .await
        .unwrap()
        .with_network(
            eryx::NetConfig::permissive().allow_plaintext_secrets(true),
            HashMap::from([("API_KEY".to_string(), secret)]),
        )
        .unwrap();
    let code = format!(
        r#"
import os, socket
def send(header):
    sock = socket.create_connection(("127.0.0.1", {port}))
    try:
        sock.send(f"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n{{header}}: {{os.environ['API_KEY']}}\r\n\r\n".encode())
        return sock.recv(4096).split(b"\r\n")[0].decode()
    except OSError as e:
        return "refused"
    finally:
        sock.close()
print(send("X-Api-Key"), send("Authorization"))
"#
    );
    let result = sandbox.execute(&code).await.unwrap();
    assert_eq!(result.stdout_text().trim(), "HTTP/1.1 200 OK refused");
    let received = String::from_utf8(received.lock().unwrap().clone()).unwrap();
    assert!(received.contains("X-Api-Key: real-key"), "{received}");
    assert!(!received.contains("Authorization"), "{received}");
    drop(sandbox);

    let sandbox = pool.acquire().await.unwrap();
    let result = sandbox
        .execute(&format!(
            r#"
import os, socket
try:
    socket.create_connection(("127.0.0.1", {port}))
    connected = True
except OSError:
    connected = False
print(os.environ.get("API_KEY"), connected)
"#
        ))
        .await
        .unwrap();
    assert_eq!(result.stdout_text().trim(), "None False");
    assert_eq!(pool.stats().total_creations, 1);
}

/// `with_network` checks secrets as `build` does.
#[tokio::test]
async fn pooled_sandbox_rejects_a_secret_with_no_hosts() {
    let pool = SandboxPool::new(Sandbox::embedded(), small_pool())
        .await
        .unwrap();
    let secret = eryx::SecretConfig::new("real-key", vec![]);
    let err = pool
        .acquire()
        .await
        .unwrap()
        .with_network(
            eryx::NetConfig::permissive(),
            std::collections::HashMap::from([("API_KEY".to_string(), secret)]),
        )
        .unwrap_err();
    assert!(err.to_string().contains("API_KEY"), "{err}");
}

/// A server on loopback that answers each connection with `body_len` bytes
async fn sending_server(body_len: usize) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&vec![b'x'; body_len]).await;
        }
    });
    port
}

/// Stats count the bytes a run sent and received, and `with_max_bytes`
/// stops it there
#[tokio::test]
async fn network_bytes_are_counted_and_limited() {
    let port = sending_server(10_000).await;
    let code = format!(
        r#"
import socket
sock = socket.create_connection(("127.0.0.1", {port}))
sock.send(b"hello")
got = 0
try:
    while chunk := sock.recv(4096):
        got += len(chunk)
except OSError as e:
    print("stopped:", e)
print(got)
"#
    );

    let sandbox = Sandbox::embedded()
        .with_network(eryx::NetConfig::permissive())
        .build()
        .unwrap();
    let result = sandbox.execute(&code).await.unwrap();
    assert_eq!(result.stdout_text().trim(), "10000");
    assert_eq!(result.stats.network_bytes_sent, 5);
    assert_eq!(result.stats.network_bytes_received, 10_000);

    let limited = Sandbox::embedded()
        .with_network(eryx::NetConfig::permissive().with_max_bytes(1_000))
        .build()
        .unwrap();
    let result = limited.execute(&code).await.unwrap();
    let stdout = result.stdout_text();
    assert!(stdout.contains("network limit reached"), "{stdout}");
    assert!(stdout.trim_end().ends_with("995"), "{stdout}");
    assert_eq!(result.stats.network_bytes_sent, 5);
    assert_eq!(result.stats.network_bytes_received, 995);
}
