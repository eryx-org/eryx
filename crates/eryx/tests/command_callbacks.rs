//! Command callbacks in the shell (#1241): host callbacks run as real
//! commands, with their arguments as given and their stdin, returning
//! stdout, stderr and an exit code.
//!
//! ```sh
//! mise run precompile-runtime-shell
//! cargo nextest run -p eryx --features embedded,shell --test command_callbacks
//! ```
#![cfg(all(feature = "embedded", feature = "shell"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use eryx::{
    CallbackError, CommandCallback, CommandOutput, DynamicCallback, MAX_COMMAND_OUTPUT_BYTES,
    MAX_COMMAND_STDIN_BYTES, Sandbox,
};
use serde_json::json;

/// `probe`: prints its arguments as JSON and then its stdin, writes `err` to
/// stderr, and exits with the number after `--exit`, if any. `size`: prints
/// as many bytes as its argument says, or how many it got on stdin. `boom`:
/// fails.
fn callbacks(calls: &Arc<AtomicU32>) -> Vec<Box<dyn eryx::Callback>> {
    let counted = Arc::clone(calls);
    let probe = CommandCallback::new("probe", "test", move |input| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let exit_code = input
                .args
                .iter()
                .position(|a| a == "--exit")
                .and_then(|i| input.args.get(i + 1)?.parse().ok())
                .unwrap_or(0);
            let mut stdout = serde_json::to_vec(&input.args).unwrap();
            stdout.push(b'\n');
            stdout.extend_from_slice(&input.stdin);
            Ok(CommandOutput {
                stdout,
                stderr: b"err\n".to_vec(),
                exit_code,
            })
        })
    });
    let size = CommandCallback::new("size", "test", |input| {
        Box::pin(async move {
            let stdout = match input.args.first() {
                Some(n) => vec![b'x'; n.parse().unwrap()],
                None => format!("{}\n", input.stdin.len()).into_bytes(),
            };
            Ok(CommandOutput {
                stdout,
                ..CommandOutput::default()
            })
        })
    });
    let boom = CommandCallback::new("boom", "test", |_| {
        Box::pin(async { Err(CallbackError::ExecutionFailed("no such thing".into())) })
    });
    let add = DynamicCallback::builder("add", "adds", |args| {
        Box::pin(async move {
            Ok(json!(
                args["a"].as_i64().unwrap() + args["b"].as_i64().unwrap()
            ))
        })
    })
    .param("a", "integer", "", true)
    .param("b", "integer", "", true)
    .build();
    vec![
        Box::new(probe),
        Box::new(size),
        Box::new(boom),
        Box::new(add),
    ]
}

/// Run `code`, returning stdout and stderr.
async fn run(code: &str) -> (String, String) {
    let calls = Arc::new(AtomicU32::new(0));
    let sandbox = Sandbox::embedded()
        .shell()
        .with_callbacks(callbacks(&calls))
        .build()
        .unwrap();
    let out = sandbox.execute(code).await.unwrap();
    (
        out.stdout_utf8().unwrap().to_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test]
async fn arguments_are_given_as_is() {
    let (stdout, stderr) = run(r#"
probe a --b c --json
probe --help -- '' 'two words' --k=v
invoke probe 007 '{"a": 1}'
"#)
    .await;
    assert_eq!(
        stdout,
        concat!(
            r#"["a","--b","c","--json"]"#,
            "\n",
            r#"["--help","--","","two words","--k=v"]"#,
            "\n",
            r#"["007","{\"a\": 1}"]"#,
            "\n",
        )
    );
    assert_eq!(stderr, "err\nerr\nerr\n");
}

#[tokio::test]
async fn stdin_and_stdout_pipe_both_ways() {
    let (stdout, _) = run(r#"
echo hi | probe
probe x | wc -c
probe
printf 'a\nb\n' | probe | probe y | tail -n 3
"#)
    .await;
    assert_eq!(stdout, "[]\nhi\n6\n[]\n[]\na\nb\n");
}

#[tokio::test]
async fn redirects_substitution_and_stderr() {
    let (stdout, stderr) = run(r#"
probe a > /tmp/out 2> /tmp/err
echo "out=$(cat /tmp/out) err=$(cat /tmp/err)"
probe b < /tmp/out
v=$(probe c 2>/dev/null); echo "[$v]"
probe d 2>&1 >/dev/null | tr a-z A-Z
"#)
    .await;
    assert_eq!(
        stdout,
        "out=[\"a\"] err=err\n[\"b\"]\n[\"a\"]\n[[\"c\"]]\nERR\n"
    );
    assert_eq!(stderr, "err\n");
}

#[tokio::test]
async fn exit_codes_are_the_commands() {
    let (stdout, stderr) = run(r#"
probe --exit 3 >/dev/null 2>&1; echo "status=$?"
probe --exit 257 >/dev/null 2>&1; echo "status=$?"
if ! probe --exit 1 >/dev/null 2>&1; then echo failed; fi
probe --exit 0 >/dev/null 2>&1 && echo ok
boom; echo "boom=$?"
(set -e; probe --exit 4 >/dev/null 2>&1; echo not reached); echo "e=$?"
"#)
    .await;
    assert_eq!(stdout, "status=3\nstatus=1\nfailed\nok\nboom=1\ne=4\n");
    assert!(stderr.starts_with("boom: "), "{stderr}");
    assert!(stderr.contains("no such thing"), "{stderr}");
}

#[tokio::test]
async fn nested_shells_and_builtins_that_run_commands() {
    let (stdout, _) = run(r#"
bash -c 'echo in | probe a' 2>/dev/null
sh -c 'probe --exit 5 >/dev/null 2>&1; echo "sh=$?"'
echo 1 2 | xargs probe 2>/dev/null
env probe e 2>/dev/null
timeout 10 probe t 2>/dev/null
bash -c 'bash -c "probe deep"' 2>/dev/null
"#)
    .await;
    assert_eq!(
        stdout,
        "[\"a\"]\nin\nsh=5\n[\"1\",\"2\"]\n[\"e\"]\n[\"t\"]\n[\"deep\"]\n"
    );
}

#[tokio::test]
async fn stdin_and_output_are_capped() {
    let code = format!(
        r#"
size {max_in} | size
{{ size {max_in}; echo; }} | probe; echo "in=$?"
size {max_out} | wc -c
size {over_out}; echo "out=$?"
"#,
        max_in = MAX_COMMAND_STDIN_BYTES,
        max_out = MAX_COMMAND_OUTPUT_BYTES,
        over_out = MAX_COMMAND_OUTPUT_BYTES + 1,
    );
    let calls = Arc::new(AtomicU32::new(0));
    let sandbox = Sandbox::embedded()
        .shell()
        .with_callbacks(callbacks(&calls))
        .build()
        .unwrap();
    let out = sandbox.execute(&code).await.unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        format!("{MAX_COMMAND_STDIN_BYTES}\nin=1\n{MAX_COMMAND_OUTPUT_BYTES}\nout=1\n")
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("probe: stdin is over the 8 MiB limit"),
        "{stderr}"
    );
    assert!(
        stderr.contains("output is over the 8 MiB limit"),
        "{stderr}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "not called past the cap");
}

#[tokio::test]
async fn ordinary_callbacks_still_parse_their_arguments() {
    let (stdout, _) =
        run("add 2 3; add --a 4 --b 5; echo x | add --json '{\"a\": 1, \"b\": 1}'").await;
    assert_eq!(stdout, "5\n9\n2\n");
}

#[tokio::test]
async fn a_command_callback_may_replace_a_command() {
    let cat = CommandCallback::new("cat", "not cat", |input| {
        Box::pin(async move {
            Ok(CommandOutput {
                stdout: input.stdin.to_ascii_uppercase(),
                ..CommandOutput::default()
            })
        })
    });
    let sandbox = Sandbox::embedded()
        .shell()
        .with_callback(cat)
        .build()
        .unwrap();
    let out = sandbox.execute("echo hi | cat").await.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "HI\n");
    // A builtin's name is still refused.
    let echo = CommandCallback::new("echo", "", |_| {
        Box::pin(async { Ok(CommandOutput::default()) })
    });
    assert!(
        Sandbox::embedded()
            .shell()
            .with_callback(echo)
            .build()
            .is_err()
    );
}

#[tokio::test]
async fn command_callbacks_are_journaled_and_replayed() {
    let calls = Arc::new(AtomicU32::new(0));
    let sandbox = Sandbox::embedded()
        .shell()
        .with_callbacks(callbacks(&calls))
        .build()
        .unwrap();
    const SCRIPT: &str = "echo in | probe a --b";
    let first = sandbox.execute_with_journal(SCRIPT, None).await;
    assert_eq!(
        first.result.unwrap().stdout_utf8().unwrap(),
        "[\"a\",\"--b\"]\nin\n"
    );
    let entry = &first.journal.entries[0];
    assert_eq!(entry.name, "probe");
    assert_eq!(entry.args_json, r#"{"args":["a","--b"],"stdin":"aW4K"}"#);

    let second = sandbox
        .execute_with_journal(SCRIPT, Some(first.journal))
        .await;
    assert_eq!(second.replayed_callbacks, 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "replayed, not called again"
    );
    let out = second.result.unwrap();
    assert_eq!(out.stdout_utf8().unwrap(), "[\"a\",\"--b\"]\nin\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
}
