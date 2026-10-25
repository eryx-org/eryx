//! Guest runtime: `asyncio.run()` inside the sandbox, and user-code line
//! numbers when the host injects a preamble.
#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use eryx::{
    CallbackError, Error, InProcessSession, RuntimeLibrary, Sandbox, Session, TypedCallback,
};
use serde_json::{Value, json};

/// A callback that really suspends the guest before answering.
struct SlowPing;

impl TypedCallback for SlowPing {
    type Args = ();

    fn name(&self) -> &str {
        "slow_ping"
    }

    fn description(&self) -> &str {
        "Returns pong after a short sleep"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(json!("pong"))
        })
    }
}

fn sandbox() -> Sandbox {
    Sandbox::embedded().with_callback(SlowPing).build().unwrap()
}

fn script_error(result: Result<eryx::ExecuteResult, Error>) -> String {
    match result {
        Err(e @ Error::PythonException(_)) => e.to_string(),
        other => panic!("expected a script exception, got {other:?}"),
    }
}

// =============================================================================
// asyncio.run()
// =============================================================================

#[tokio::test]
async fn asyncio_run_awaits_on_the_sandbox_loop() {
    let out = sandbox()
        .execute(
            "import asyncio\n\
             async def main():\n    await asyncio.sleep(0)\n    return 42\n\
             print(asyncio.run(main()))",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, b"42\n");
}

#[tokio::test]
async fn asyncio_run_main_guard_with_callbacks() {
    let out = sandbox()
        .execute(
            "import asyncio\n\
             async def main():\n    a, b = await asyncio.gather(slow_ping(), slow_ping())\n    return a + b\n\
             if __name__ == '__main__':\n    result = asyncio.run(main(), debug=True)\n    print(result)",
        )
        .await
        .unwrap();
    assert_eq!(out.stdout, b"pongpong\n");
}

#[tokio::test]
async fn asyncio_run_inside_a_function_explains_itself() {
    let err = script_error(
        sandbox()
            .execute(
                "import asyncio\n\
                 async def main():\n    return 1\n\
                 def sync_entry():\n    return asyncio.run(main())\n\
                 sync_entry()",
            )
            .await,
    );
    assert!(err.contains("only works as a top-level statement"), "{err}");
}

// =============================================================================
// Preamble line numbers
// =============================================================================

fn sandbox_with_preamble() -> Sandbox {
    Sandbox::embedded()
        .with_library(RuntimeLibrary::new().with_preamble("a = 1\nb = 2\nc = 3\n"))
        .build()
        .unwrap()
}

const FAILS_ON_LINE_3: &str = "x = a + b + c\nprint(x)\nraise ValueError('boom')\n";

#[tokio::test]
async fn preamble_does_not_shift_traceback_line_numbers() {
    let err = script_error(sandbox_with_preamble().execute(FAILS_ON_LINE_3).await);
    assert!(err.contains(r#"File "<user>", line 3"#), "{err}");
}

#[tokio::test]
async fn preamble_does_not_shift_line_numbers_with_top_level_await() {
    let sandbox = Sandbox::embedded()
        .with_callback(SlowPing)
        .with_library(RuntimeLibrary::new().with_preamble("a = 1\nb = 2\nc = 3\n"))
        .build()
        .unwrap();
    let err = script_error(
        sandbox
            .execute("x = await slow_ping()\nprint(x)\nraise ValueError(x)\n")
            .await,
    );
    assert!(err.contains(r#"File "<user>", line 3"#), "{err}");
}

#[tokio::test]
async fn session_preamble_does_not_shift_traceback_line_numbers() {
    let sandbox = sandbox_with_preamble();
    let mut session = InProcessSession::new(&sandbox).await.unwrap();
    let err = script_error(session.execute(FAILS_ON_LINE_3).await);
    assert!(err.contains(r#"File "<user>", line 3"#), "{err}");
}

#[tokio::test]
async fn preamble_errors_are_attributed_to_the_preamble() {
    let sandbox = Sandbox::embedded()
        .with_library(RuntimeLibrary::new().with_preamble("ok = 1\nraise KeyError('setup')\n"))
        .build()
        .unwrap();
    let err = script_error(sandbox.execute("print('unreached')").await);
    assert!(err.contains(r#"File "<preamble>", line 2"#), "{err}");
}
