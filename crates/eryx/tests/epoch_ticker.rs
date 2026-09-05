//! Regression coverage for the shared epoch ticker lifecycle.
#![cfg(all(target_os = "linux", feature = "embedded"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use eryx::PythonExecutor;

fn epoch_ticker_thread_count() -> Option<usize> {
    Some(
        std::fs::read_dir("/proc/self/task")
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_to_string(entry.path().join("comm")).ok())
            // Linux limits the comm field to 15 bytes, truncating the full name.
            .filter(|name| name.trim().starts_with("eryx-epoch-tick"))
            .count(),
    )
}

async fn wait_for_ticker_count(expected: usize) -> Option<usize> {
    for _ in 0..100 {
        let count = epoch_ticker_thread_count()?;
        if count == expected {
            return Some(count);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    epoch_ticker_thread_count()
}

#[tokio::test]
async fn test_epoch_ticker_starts_lazily_and_is_shared() {
    let executor = PythonExecutor::from_embedded_runtime().expect("embedded runtime to load");
    let Some(initial_count) = epoch_ticker_thread_count() else {
        eprintln!("skipping ticker-thread assertion: /proc/self/task is unavailable");
        return;
    };
    assert_eq!(
        initial_count, 0,
        "engine/executor creation must not start the epoch ticker"
    );

    let untimed = executor.execute("print('untimed')").run().await;
    assert!(
        untimed.is_ok(),
        "untimed execution should succeed: {untimed:?}"
    );
    assert_eq!(
        epoch_ticker_thread_count(),
        Some(0),
        "untimed execution must not start the epoch ticker"
    );

    let timed = executor
        .execute("while True: pass")
        .with_timeout(std::time::Duration::from_millis(100))
        .run()
        .await;
    assert!(matches!(timed, Err(eryx::Error::Timeout(_))));
    assert_eq!(wait_for_ticker_count(1).await, Some(1));

    for _ in 0..3 {
        let timed = executor
            .execute("while True: pass")
            .with_timeout(std::time::Duration::from_millis(50))
            .run()
            .await;
        assert!(matches!(timed, Err(eryx::Error::Timeout(_))));

        let python_error = executor
            .execute("raise ValueError('unwind')")
            .with_timeout(std::time::Duration::from_millis(50))
            .run()
            .await;
        assert!(matches!(python_error, Err(eryx::Error::PythonException(_))));
        assert_eq!(epoch_ticker_thread_count(), Some(1));
    }
}
