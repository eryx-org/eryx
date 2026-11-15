//! Deep recursion in a guest must fail the execution, not overflow the async
//! fiber.
//!
//! Host code called from the guest runs on the same fiber and only gets
//! `async_stack_size - max_wasm_stack` of it. With the two equal, these
//! scripts overflowed the fiber and wasmtime aborted the whole process, so
//! each runs in a child process: a regression fails the test instead of
//! killing the test runner. The allocator is process-global, hence one child
//! per allocator.

#![cfg(feature = "embedded")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

use eryx::Sandbox;

const CHILD_ENV: &str = "ERYX_STACK_OVERFLOW_CHILD";

const SCRIPTS: &[&str] = &[
    r#"import json; json.loads("[" * 10**6 + "]" * 10**6)"#,
    "d = []\nfor _ in range(10**6): d = [d]\nrepr(d)",
];

#[test]
fn deep_guest_recursion_traps_instead_of_aborting() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_scripts();
        return;
    }
    run_in_children("deep_guest_recursion_traps_instead_of_aborting");
}

/// QuickJS gets the same fiber: deep JavaScript recursion must fail the
/// execution too, not abort the process.
#[cfg(feature = "javascript")]
#[test]
fn deep_javascript_recursion_fails_instead_of_aborting() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_javascript();
        return;
    }
    run_in_children("deep_javascript_recursion_fails_instead_of_aborting");
}

/// Re-run the test `name` in a child process per allocator.
fn run_in_children(name: &str) {
    for allocator in ["pooling", "on-demand"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--nocapture"])
            .env(CHILD_ENV, "1")
            .env("ERYX_ALLOCATOR", allocator)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child with ERYX_ALLOCATOR={allocator} failed ({}):\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[tokio::main(flavor = "current_thread")]
async fn run_scripts() {
    let sandbox = Sandbox::embedded().build().unwrap();
    for script in SCRIPTS {
        let err = sandbox
            .execute(script)
            .await
            .expect_err("deep recursion should fail");
        assert!(
            matches!(err.kind(), eryx::ErrorKind::Execution(_)),
            "unexpected error for {script:?}: {err:?}"
        );
        assert!(
            err.to_string().contains("call stack exhausted"),
            "unexpected error for {script:?}: {err}"
        );
        // The trap must leave the sandbox usable.
        let after = sandbox.execute("print('after')").await.unwrap();
        assert_eq!(after.stdout_text(), "after\n");
    }
}

#[cfg(feature = "javascript")]
#[tokio::main(flavor = "current_thread")]
async fn run_javascript() {
    let sandbox = Sandbox::embedded().javascript().build().unwrap();
    let err = sandbox
        .execute("function f(n) { return n ? f(n - 1) + 1 : 0 }\nf(1e6)")
        .await
        .expect_err("deep recursion should fail");
    // Like the Python scripts, it ends in the wasm stack-limit trap.
    assert!(
        matches!(err.kind(), eryx::ErrorKind::Execution(_)),
        "unexpected error: {err:?}"
    );
    assert!(
        err.to_string().contains("call stack exhausted"),
        "unexpected error: {err}"
    );
    // The trap must leave the sandbox usable.
    let after = sandbox.execute("console.log('after')").await.unwrap();
    assert_eq!(after.stdout_text(), "after\n");
}
