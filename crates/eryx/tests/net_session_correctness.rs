//! Regression tests for `InProcessSession` networking and for sessions whose
//! execute future is dropped mid-run.
#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eryx::{DynamicCallback, InProcessSession, NetConfig, RuntimeLibrary, Sandbox, Session};
use tokio::io::AsyncWriteExt;

/// A session built from a sandbox with networking can use the network.
#[tokio::test]
async fn session_uses_sandbox_network_config() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"hi").await.unwrap();
    });

    let sandbox = Sandbox::builder()
        .with_embedded_runtime()
        .with_network(NetConfig::permissive())
        .build()
        .unwrap();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();

    let code = format!(
        r#"
import _eryx_async
h = await _eryx_async.await_tcp_connect("127.0.0.1", {port})
print(await _eryx_async.await_tcp_read(h, 2))
"#
    );
    let output = session.execute(&code).await.unwrap();
    assert_eq!(output.stdout_text().trim(), "b'hi'");
    server.await.unwrap();
}

/// Sets its flag when dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Dropping an execute future mid-run stops the callbacks it started, and
/// the session starts over (preamble included) instead of failing every
/// later call.
#[tokio::test]
async fn session_recovers_after_cancelled_execute() {
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&dropped);
    let hang = DynamicCallback::builder("hang", "Never returns", move |_args| {
        let guard = DropFlag(Arc::clone(&flag));
        Box::pin(async move {
            let _guard = guard;
            std::future::pending().await
        })
    })
    .build();

    let sandbox = Sandbox::builder()
        .with_embedded_runtime()
        .with_callback(hang)
        .with_library(RuntimeLibrary::new().with_preamble("base = 41"))
        .build()
        .unwrap();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();

    let cancelled =
        tokio::time::timeout(Duration::from_millis(500), session.execute("await hang()")).await;
    assert!(cancelled.is_err(), "the call should still be running");

    tokio::time::timeout(Duration::from_secs(5), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the in-flight callback kept running after its call was dropped");

    let output = session.execute("print(base + 1)").await.unwrap();
    assert_eq!(output.stdout_text().trim(), "42");
}
