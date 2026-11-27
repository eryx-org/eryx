//! End-to-end gRPC tests for the eryx server.
//!
//! These tests start a tonic server in-process, connect a client, and verify
//! the full bidirectional streaming flow: execute request → callback round-trip
//! → output streaming → trace events → final result.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Extension trait for bytes fields in proto messages to enable string-like operations.
trait BytesExt {
    fn to_lossy(&self) -> String;
    fn contains_str(&self, needle: &str) -> bool;
}
impl BytesExt for Vec<u8> {
    fn to_lossy(&self) -> String {
        String::from_utf8_lossy(self).into_owned()
    }
    fn contains_str(&self, needle: &str) -> bool {
        String::from_utf8_lossy(self).contains(needle)
    }
}

use eryx::{PoolConfig, Sandbox, SandboxPool};
use eryx_server::proto::eryx::v1::eryx_client::EryxClient;
use eryx_server::proto::eryx::v1::{
    CallbackDeclaration, CallbackJournal, CallbackOutcome, ClientMessage, ExecuteRequest,
    ExecuteResult, ParameterDeclaration, ResourceLimits, callback_response, client_message,
    server_message,
};
use eryx_server::service::EryxService;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

/// Start an in-process gRPC server on a random port and return the channel.
async fn start_server() -> Channel {
    start_server_with_signer(eryx_server::replay::JournalSigner::random()).await
}

async fn start_server_with_signer(signer: eryx_server::replay::JournalSigner) -> Channel {
    start_server_with(4, |pool| EryxService::with_signer(pool, signer)).await
}

/// Start a server whose pool holds at most `max_size` sandboxes, with the
/// service built by `service`.
async fn start_server_with(
    max_size: usize,
    service: impl FnOnce(Arc<SandboxPool>) -> EryxService,
) -> Channel {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let pool_config = PoolConfig::default()
        .with_max_size(max_size)
        .with_min_idle(1);

    let pool = SandboxPool::new(Sandbox::embedded(), pool_config)
        .await
        .expect("failed to create sandbox pool");
    let pool = Arc::new(pool);

    let service = service(pool).into_server();
    tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    // Give the server a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;

    Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

#[tokio::test]
async fn execute_simple_print() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "print('hello from eryx')".to_string(),
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(server_message::Message::ExecuteResult(result)) = msg.message {
            assert!(result.success, "execution failed: {}", result.error);
            assert!(
                result.stdout.contains_str("hello from eryx"),
                "stdout missing expected output: {:?}",
                result.stdout
            );
            got_result = true;
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

/// Run `code` in `language` and return the final result, or the gRPC
/// status the server rejected the request with.
async fn execute_in(
    channel: Channel,
    language: eryx_server::proto::eryx::v1::Language,
    code: &str,
) -> Result<eryx_server::proto::eryx::v1::ExecuteResult, tonic::Status> {
    execute_request(
        channel,
        ExecuteRequest {
            code: code.to_string(),
            language: language as i32,
            ..Default::default()
        },
    )
    .await
}

/// Send `request` and return the final result, or the gRPC status the server
/// rejected it with.
async fn execute_request(
    channel: Channel,
    request: ExecuteRequest,
) -> Result<eryx_server::proto::eryx::v1::ExecuteResult, tonic::Status> {
    let mut client = EryxClient::new(channel);
    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(request))),
    })
    .await
    .unwrap();
    let mut stream = client.execute(ReceiverStream::new(rx)).await?.into_inner();
    while let Some(msg) = stream.message().await? {
        if let Some(server_message::Message::ExecuteResult(result)) = msg.message {
            return Ok(*result);
        }
    }
    panic!("never received ExecuteResult");
}

/// A server built without the `javascript` feature rejects JavaScript,
/// naming the feature.
#[cfg(not(feature = "javascript"))]
#[tokio::test]
async fn a_server_without_javascript_rejects_it() {
    use eryx_server::proto::eryx::v1::Language;

    let status = execute_in(start_server().await, Language::Javascript, "1")
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("`javascript` cargo feature"),
        "{status:?}"
    );
}

/// A request picks its pool by language, created on first use; a language
/// without one is rejected.
#[cfg(feature = "javascript")]
#[tokio::test]
async fn execute_selects_the_language_pool() {
    use eryx_server::proto::eryx::v1::Language;

    // JavaScript's pool is created on its first request.
    let created = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = Arc::clone(&created);
    let create: eryx_server::service::PoolFactory = Arc::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(2);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().javascript(), config).await?,
            ))
        })
    });
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::JavaScript, create)
    })
    .await;
    assert_eq!(created.load(std::sync::atomic::Ordering::SeqCst), 0);

    let result = execute_in(channel.clone(), Language::Javascript, "console.log(6 * 7)")
        .await
        .unwrap();
    assert!(result.success, "execution failed: {}", result.error);
    assert_eq!(result.stdout, b"42\n");
    // The second request reuses it.
    execute_in(channel.clone(), Language::Javascript, "1")
        .await
        .unwrap();
    assert_eq!(created.load(std::sync::atomic::Ordering::SeqCst), 1);

    // JavaScript reads a secret's placeholder, never its value, from
    // `process.env`.
    let result = execute_request(
        channel.clone(),
        ExecuteRequest {
            code: "console.log(process.env.TOKEN !== undefined, process.env.TOKEN === 's3cret')"
                .to_string(),
            language: Language::Javascript as i32,
            secrets: vec![eryx_server::proto::eryx::v1::SecretConfig {
                name: "TOKEN".to_string(),
                value: "s3cret".to_string(),
                allowed_hosts: vec![],
                ..Default::default()
            }],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result.stdout, b"true false\n");

    // Unspecified is Python.
    let result = execute_in(channel.clone(), Language::Unspecified, "print(6 * 7)")
        .await
        .unwrap();
    assert_eq!(result.stdout, b"42\n");

    let status = execute_in(channel, Language::Shell, "echo hi")
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("shell"), "{status:?}");
}

/// The fixture packages of the eryx crate's JavaScript environment tests
/// (`crates/eryx/tests/fixtures/js-environment`), as an environment named
/// `fixtures` in `dir`.
#[cfg(feature = "javascript")]
fn fixture_environment(dir: &std::path::Path) -> eryx::environment::Environment {
    use eryx::environment::{Builder, Environment, Inputs, Language, Manifest};
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../eryx/tests/fixtures/js-environment");
    let modules = dir.join("modules");
    std::fs::create_dir_all(&modules).unwrap();
    for entry in std::fs::read_dir(fixtures.join("modules")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), modules.join(entry.file_name())).unwrap();
    }
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("bundle.json")).unwrap()).unwrap();
    let exports = report["entries"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, file)| (name.clone(), format!("modules/{}", file.as_str().unwrap())))
        .collect();
    Manifest::new(
        "fixtures",
        Language::JavaScript,
        Inputs::new(vec!["zod@4.6.7".into()]),
        Builder::new("test"),
        dir,
        "modules",
        exports,
    )
    .unwrap()
    .write(dir)
    .unwrap();
    Environment::load(dir).unwrap()
}

/// A request names an environment to mount; its pool is separate from the
/// language's, and a name the server doesn't have, or one for another
/// language, is refused.
#[cfg(feature = "javascript")]
#[tokio::test]
async fn execute_mounts_the_requested_environment() {
    use eryx_server::proto::eryx::v1::Language;

    let dir = tempfile::tempdir().unwrap();
    let environment = fixture_environment(dir.path());
    let served = environment.clone();
    let create =
        |environment: Option<eryx::environment::Environment>| -> eryx_server::service::PoolFactory {
            Arc::new(move || {
                let environment = environment.clone();
                Box::pin(async move {
                    let mut builder = Sandbox::embedded().javascript();
                    if let Some(environment) = &environment {
                        builder = builder.with_environment(environment);
                    }
                    let config = PoolConfig::default().with_max_size(2);
                    Ok(Arc::new(SandboxPool::new(builder, config).await?))
                })
            })
        };
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool)
            .with_lazy_pool(eryx::Language::JavaScript, create(None))
            .with_environment_pool(&served, create(Some(environment)))
    })
    .await;
    let request = |code: &str, language: Language, environment: &str| ExecuteRequest {
        code: code.to_string(),
        language: language as i32,
        environment: environment.to_string(),
        ..Default::default()
    };
    let import = r#"import { z } from "zod";
console.log(z.string().parse("ok"));"#;

    let result = execute_request(
        channel.clone(),
        request(import, Language::Javascript, "fixtures"),
    )
    .await
    .unwrap();
    assert!(
        result.success,
        "execution failed: {} {}",
        result.error,
        result.stderr.to_lossy()
    );
    assert_eq!(result.stdout, b"ok\n");

    // Without the environment, there's nothing to import.
    let result = execute_request(channel.clone(), request(import, Language::Javascript, ""))
        .await
        .unwrap();
    assert!(!result.success);

    let status = execute_request(channel.clone(), request("1", Language::Javascript, "nope"))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("no environment \"nope\""),
        "{status:?}"
    );

    let status = execute_request(channel, request("print(1)", Language::Python, "fixtures"))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("is for javascript, not python"),
        "{status:?}"
    );
}

/// A JavaScript session's state survives a snapshot round trip through the
/// server, and the variables it couldn't keep are named.
#[cfg(feature = "javascript")]
#[tokio::test]
async fn javascript_session_state_round_trips() {
    use eryx_server::proto::eryx::v1::Language;

    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(2);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().javascript(), config).await?,
            ))
        })
    });
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::JavaScript, create)
    })
    .await;

    let first = execute_request(
        channel.clone(),
        ExecuteRequest {
            code: "var seed = 7; const scale = { by: 6 }; function helper() {}".to_string(),
            language: Language::Javascript as i32,
            persist_state: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(first.success, "first run failed: {}", first.error);
    assert!(!first.state_snapshot.is_empty(), "no state snapshot");
    assert_eq!(first.state_skipped_globals, ["helper"]);

    let second = execute_request(
        channel,
        ExecuteRequest {
            code: "result = seed * scale.by".to_string(),
            language: Language::Javascript as i32,
            persist_state: true,
            state_snapshot: first.state_snapshot,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(second.success, "second run failed: {}", second.error);
    assert_eq!(second.result, "42");
}

/// `ExecuteRequest.globals` become JavaScript globals, before leading imports
/// are rewritten and without shifting line numbers or displacing a leading
/// `'use strict'` or hashbang; a nested `__proto__` stays data.
#[cfg(feature = "javascript")]
#[tokio::test]
async fn javascript_globals_are_injected() {
    use eryx_server::proto::eryx::v1::Language;

    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(2);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().javascript(), config).await?,
            ))
        })
    });
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::JavaScript, create)
    })
    .await;
    let request = |code: &str| ExecuteRequest {
        code: code.to_string(),
        language: Language::Javascript as i32,
        globals: r#"{"inputs": [1, 2], "o": {"__proto__": 5}}"#.to_string(),
        ..Default::default()
    };

    let result = execute_request(
        channel.clone(),
        request(
            "import { Buffer } from 'node:buffer';\n\
             console.log(inputs[0] + inputs[1], Object.keys(o).join(), \
             Object.getPrototypeOf(o) === Object.prototype, Buffer.from('a').length)",
        ),
    )
    .await
    .unwrap();
    assert!(
        result.success,
        "execution failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"3 __proto__ true 1\n");

    // The preamble runs on its own, so a directive or hashbang the code
    // starts with is still first.
    let result = execute_request(
        channel.clone(),
        request("'use strict';\nundeclared = inputs.length"),
    )
    .await
    .unwrap();
    assert!(!result.success, "sloppy mode: {:?}", result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("ReferenceError"), "{stderr}");
    let result = execute_request(
        channel.clone(),
        request("#!/usr/bin/env node\nconsole.log(inputs.length)"),
    )
    .await
    .unwrap();
    assert!(result.success, "hashbang: {}", result.error);
    assert_eq!(result.stdout, b"2\n");

    let result = execute_request(
        channel,
        request(
            "import { Buffer } from 'node:buffer';\n\nthrow new Error('boom ' + inputs.length)",
        ),
    )
    .await
    .unwrap();
    assert!(!result.success);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("boom 2"), "{stderr}");
    assert!(stderr.contains(":3:"), "{stderr}");
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

/// `ExecuteRequest.env` sets `os.environ`, a value with quotes and newlines
/// arriving as it was sent; names the sandbox sets, a secret's name and bad
/// names are refused. With persisted state, Python starts each request
/// from only that request's variables.
#[tokio::test]
async fn python_env_is_set() {
    use eryx_server::proto::eryx::v1::SecretConfig;

    let channel = start_server().await;
    let hostile = "it's \"quoted\"\n'); import os; os._exit(3) #";
    let request =
        |code: &str, env: BTreeMap<String, String>, state_snapshot: Vec<u8>| ExecuteRequest {
            code: code.to_string(),
            env,
            persist_state: true,
            state_snapshot,
            ..Default::default()
        };

    let first = execute_request(
        channel.clone(),
        request(
            &format!(
                "import os\nprint(os.environ['GIT_AUTHOR_NAME'])\n\
                 print(os.environ['HOSTILE'] == {})\nkept = 1",
                serde_json::to_string(hostile).unwrap()
            ),
            env(&[("GIT_AUTHOR_NAME", "Eryx"), ("HOSTILE", hostile)]),
            vec![],
        ),
    )
    .await
    .unwrap();
    assert!(first.success, "execution failed: {}", first.error);
    assert_eq!(first.stdout.to_lossy(), "Eryx\nTrue\n");

    let code = "import os\nprint(kept, os.environ.get('GIT_AUTHOR_NAME'))";
    let second = execute_request(
        channel.clone(),
        request(code, BTreeMap::new(), first.state_snapshot.clone()),
    )
    .await
    .unwrap();
    assert!(second.success, "execution failed: {}", second.error);
    assert_eq!(second.stdout.to_lossy(), "1 None\n");
    let third = execute_request(
        channel.clone(),
        request(
            code,
            env(&[("GIT_AUTHOR_NAME", "Other")]),
            first.state_snapshot,
        ),
    )
    .await
    .unwrap();
    assert_eq!(third.stdout.to_lossy(), "1 Other\n");

    for (vars, expected) in [
        (
            env(&[("NOT-A-NAME", "x")]),
            "invalid environment variable name",
        ),
        (env(&[("HOME", "/root")]), "set by the sandbox itself"),
        (env(&[("TOKEN", "x")]), "collides with the secret TOKEN"),
        (env(&[("A", "a\0b")]), "NUL byte"),
    ] {
        let status = execute_request(
            channel.clone(),
            ExecuteRequest {
                code: "print(1)".to_string(),
                env: vars,
                secrets: vec![SecretConfig {
                    name: "TOKEN".to_string(),
                    value: "v".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{status:?}");
        assert!(status.message().contains(expected), "{status:?}");
    }
}

/// `ExecuteRequest.env` sets JavaScript's `process.env`, alongside the
/// secrets' placeholders.
#[cfg(feature = "javascript")]
#[tokio::test]
async fn javascript_env_is_set() {
    use eryx_server::proto::eryx::v1::{Language, SecretConfig};

    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(2);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().javascript(), config).await?,
            ))
        })
    });
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::JavaScript, create)
    })
    .await;
    let hostile = "it's \"quoted\"\n`${process.exit(3)}`')); process.exit(3); //";
    let result = execute_request(
        channel.clone(),
        ExecuteRequest {
            code: format!(
                "console.log(process.env.GIT_AUTHOR_NAME, process.env.HOSTILE === {}, \
                 process.env.TOKEN.startsWith('ERYX_SECRET_PLACEHOLDER_'))",
                serde_json::to_string(hostile).unwrap()
            ),
            language: Language::Javascript as i32,
            env: env(&[("GIT_AUTHOR_NAME", "Eryx"), ("HOSTILE", hostile)]),
            secrets: vec![SecretConfig {
                name: "TOKEN".to_string(),
                value: "v".to_string(),
                ..Default::default()
            }],
            persist_state: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(result.success, "execution failed: {}", result.error);
    assert_eq!(result.stdout.to_lossy(), "Eryx true true\n");

    // Restored state doesn't bring them back: `process` is the host's.
    let restored = execute_request(
        channel,
        ExecuteRequest {
            code: "console.log(typeof process)".to_string(),
            language: Language::Javascript as i32,
            persist_state: true,
            state_snapshot: result.state_snapshot,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(restored.success, "execution failed: {}", restored.error);
    assert_eq!(restored.stdout.to_lossy(), "undefined\n");
}

/// `ExecuteRequest.env` is exported in the shell, so its commands see it.
/// With persisted state, the shell keeps what an earlier request exported,
/// and a later request's variables replace it.
#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_env_is_exported() {
    use eryx_server::proto::eryx::v1::Language;

    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(2);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().shell(), config).await?,
            ))
        })
    });
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::Shell, create)
    })
    .await;
    let hostile = "it's \"quoted\"\n$(exit 3)`exit 3`'; exit 3; '";
    let request =
        |code: &str, env: BTreeMap<String, String>, state_snapshot: Vec<u8>| ExecuteRequest {
            code: code.to_string(),
            language: Language::Shell as i32,
            env,
            persist_state: true,
            state_snapshot,
            ..Default::default()
        };

    let first = execute_request(
        channel.clone(),
        request(
            "printenv GIT_AUTHOR_NAME; printf '%s' \"$HOSTILE\"",
            env(&[("GIT_AUTHOR_NAME", "Eryx"), ("HOSTILE", hostile)]),
            vec![],
        ),
    )
    .await
    .unwrap();
    assert!(first.success, "execution failed: {}", first.error);
    assert_eq!(first.stdout.to_lossy(), format!("Eryx\n{hostile}"));

    let code = "printenv GIT_AUTHOR_NAME";
    let second = execute_request(
        channel.clone(),
        request(code, BTreeMap::new(), first.state_snapshot.clone()),
    )
    .await
    .unwrap();
    assert_eq!(second.stdout.to_lossy(), "Eryx\n", "{}", second.error);
    let third = execute_request(
        channel,
        request(
            code,
            env(&[("GIT_AUTHOR_NAME", "Other")]),
            first.state_snapshot,
        ),
    )
    .await
    .unwrap();
    assert_eq!(third.stdout.to_lossy(), "Other\n", "{}", third.error);
}

/// With persisted JavaScript state, a request's globals replace a restored
/// `let` of the same name rather than hiding behind it; a restored `const`
/// can't be replaced, so the execution fails.
#[cfg(feature = "javascript")]
#[tokio::test]
async fn javascript_globals_replace_restored_state() {
    use eryx_server::proto::eryx::v1::Language;

    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(2);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().javascript(), config).await?,
            ))
        })
    });
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::JavaScript, create)
    })
    .await;
    let request = |code: &str, globals: &str, state_snapshot: Vec<u8>| ExecuteRequest {
        code: code.to_string(),
        language: Language::Javascript as i32,
        globals: globals.to_string(),
        persist_state: true,
        state_snapshot,
        ..Default::default()
    };

    let first = execute_request(
        channel.clone(),
        request("let total = 1; const fixed = 1", "", vec![]),
    )
    .await
    .unwrap();
    assert!(first.success, "first run failed: {}", first.error);

    let second = execute_request(
        channel.clone(),
        request(
            "console.log(total)",
            r#"{"total": 5}"#,
            first.state_snapshot.clone(),
        ),
    )
    .await
    .unwrap();
    assert!(second.success, "second run failed: {}", second.error);
    assert_eq!(second.stdout, b"5\n");

    let third = execute_request(
        channel,
        request(
            "console.log(fixed)",
            r#"{"fixed": 5}"#,
            first.state_snapshot,
        ),
    )
    .await
    .unwrap();
    assert!(!third.success, "a restored const was replaced");
}

/// A shell request runs the embedded coreutils, and its result carries the
/// script's exit status: 0 on success, N for `exit N`. Python leaves it unset.
#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_results_carry_the_exit_code() {
    use eryx_server::proto::eryx::v1::{FailureKind, Language};

    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(2);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().shell(), config).await?,
            ))
        })
    });
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::Shell, create)
    })
    .await;

    let ok = execute_in(
        channel.clone(),
        Language::Shell,
        "printf 'pear\\napple\\n' | sort | head -n 1",
    )
    .await
    .unwrap();
    assert!(ok.success, "{}", ok.error);
    assert_eq!(ok.stdout, b"apple\n");
    assert_eq!(ok.exit_code, Some(0));

    let failed = execute_in(channel.clone(), Language::Shell, "echo before; exit 3")
        .await
        .unwrap();
    assert!(!failed.success);
    assert_eq!(failed.failure_kind, FailureKind::ScriptFailed as i32);
    assert_eq!(failed.exit_code, Some(3));
    assert_eq!(failed.stdout, b"before\n");

    let python = execute_in(channel, Language::Python, "raise SystemExit(2)")
        .await
        .unwrap();
    assert_eq!(python.exit_code, None);
}

/// `allowed_commands` narrows one shell request's commands: set, only those
/// run and others exit 127; set but empty, none run; unset, all do. Python
/// requests can't set it.
#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_requests_narrow_their_commands() {
    use eryx_server::proto::eryx::v1::{AllowedCommands, Language};

    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(1);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().shell(), config).await?,
            ))
        })
    });
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::Shell, create)
    })
    .await;
    let code = "echo a | cat; echo \"cat=$?\"; echo b | sort >/dev/null; echo \"sort=$?\"; \
                echo c | grep c >/dev/null 2>&1; echo \"grep=$?\"";
    let run = |allowed: Option<Vec<&str>>| {
        execute_request(
            channel.clone(),
            ExecuteRequest {
                code: code.to_string(),
                language: Language::Shell as i32,
                allowed_commands: allowed.map(|names| AllowedCommands {
                    names: names.into_iter().map(str::to_owned).collect(),
                }),
                ..Default::default()
            },
        )
    };

    let only_cat = run(Some(vec!["cat"])).await.unwrap();
    assert_eq!(
        only_cat.stdout, b"a\ncat=0\nsort=127\ngrep=127\n",
        "{only_cat:?}"
    );
    let none = run(Some(vec![])).await.unwrap();
    assert_eq!(none.stdout, b"cat=127\nsort=127\ngrep=127\n", "{none:?}");
    // The same pooled sandbox, unnarrowed again.
    let all = run(None).await.unwrap();
    assert_eq!(all.stdout, b"a\ncat=0\nsort=0\ngrep=0\n", "{all:?}");

    let status = execute_request(
        channel,
        ExecuteRequest {
            code: "pass".to_string(),
            allowed_commands: Some(AllowedCommands::default()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument, "{status:?}");
}

/// `commands` changes what one shell request's commands get, and may only
/// take away: a name the server doesn't have, network for a command when the
/// request gives children none, or `commands` on a Python request is
/// INVALID_ARGUMENT. `resource_limits.max_child_host_bytes` and
/// `max_child_memory_bytes` reach the children, and shell state persists across requests like any guest's.
#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_requests_set_command_options_and_child_limits() {
    use eryx_server::proto::eryx::v1::{
        ChildNetwork, CommandOptions, Language, ResourceLimits, child_network::Mode,
    };

    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(1);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().shell(), config).await?,
            ))
        })
    });
    let channel = start_server_with(4, |pool| {
        EryxService::new(pool)
            .with_lazy_pool(eryx::Language::Shell, create)
            .with_shell_commands(
                eryx::embedded::COREUTILS_TOOLS
                    .iter()
                    .map(|tool| (*tool).to_owned()),
            )
    })
    .await;
    let shell = |code: &str, commands: Vec<(&str, CommandOptions)>| ExecuteRequest {
        code: code.to_string(),
        language: Language::Shell as i32,
        commands: commands
            .into_iter()
            .map(|(name, options)| (name.to_owned(), options))
            .collect(),
        ..Default::default()
    };
    let offline = CommandOptions {
        network: Some(ChildNetwork {
            mode: Mode::None as i32,
            narrow: None,
        }),
        no_tunnels: true,
        no_spawn: true,
    };

    // Taking away what an offline request's command doesn't have is fine.
    let ok = execute_request(
        channel.clone(),
        shell("echo hi | cat", vec![("cat", offline)]),
    )
    .await
    .unwrap();
    assert!(ok.success, "{ok:?}");
    assert_eq!(ok.stdout, b"hi\n");

    let inherit = CommandOptions {
        network: Some(ChildNetwork {
            mode: Mode::Inherit as i32,
            narrow: None,
        }),
        ..Default::default()
    };
    for request in [
        shell("true", vec![("nope", CommandOptions::default())]),
        shell("true", vec![("cat", inherit)]),
        ExecuteRequest {
            code: "pass".to_string(),
            commands: [("cat".to_owned(), CommandOptions::default())].into(),
            ..Default::default()
        },
    ] {
        let status = execute_request(channel.clone(), request).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{status:?}");
    }

    // A child's arguments and environment over the request's limit.
    let limited = execute_request(
        channel.clone(),
        ExecuteRequest {
            resource_limits: Some(ResourceLimits {
                max_child_host_bytes: 16,
                ..Default::default()
            }),
            ..shell("echo hi | cat; echo \"cat=$?\"", vec![])
        },
    )
    .await
    .unwrap();
    assert!(
        String::from_utf8_lossy(&limited.stderr).contains("cat: limit-exceeded")
            && !String::from_utf8_lossy(&limited.stdout).contains("cat=0"),
        "{limited:?}"
    );

    // Children over the request's memory pool: a coreutils command reserves
    // about 4 MiB, so three of eight stages fit 16 MiB.
    let pipeline = format!("{}; true", ["sleep 0.2"; 8].join(" | "));
    let pooled = |mib: u64| {
        execute_request(
            channel.clone(),
            ExecuteRequest {
                resource_limits: Some(ResourceLimits {
                    max_child_memory_bytes: mib << 20,
                    ..Default::default()
                }),
                ..shell(&pipeline, vec![])
            },
        )
    };
    let full = pooled(16).await.unwrap();
    assert!(
        String::from_utf8_lossy(&full.stderr).contains("(child memory pool full: ")
            && String::from_utf8_lossy(&full.stderr).contains(" of 16 MiB in use; "),
        "{full:?}"
    );
    let roomy = pooled(64).await.unwrap();
    assert!(roomy.stderr.is_empty(), "{roomy:?}");

    // Shell state carries over through the snapshot.
    let first = execute_request(
        channel.clone(),
        ExecuteRequest {
            persist_state: true,
            ..shell("greeting=hello", vec![])
        },
    )
    .await
    .unwrap();
    assert!(first.success, "{first:?}");
    let second = execute_request(
        channel,
        ExecuteRequest {
            persist_state: true,
            state_snapshot: first.state_snapshot,
            ..shell("echo \"$greeting\"", vec![])
        },
    )
    .await
    .unwrap();
    assert_eq!(second.stdout, b"hello\n", "{second:?}");
}

/// An uncaught exception over gRPC. The traceback (which Python writes to
/// stderr) is surfaced as CPython would — over stderr, not as a sandbox error:
///   - streamed live as STDERR `OutputEvent`s while the code runs,
///   - in the final `ExecuteResult.stderr` field, and
///   - classified via `failure_kind = SCRIPT_FAILED`, with `error` left empty
///     (it is reserved for sandbox-level failures) and `success = false`.
#[tokio::test]
async fn execute_uncaught_exception_returns_error_not_stderr() {
    use eryx_server::proto::eryx::v1::{FailureKind, OutputStream};

    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                // Print to stdout first, then raise: both captured streams must
                // survive onto the final result.
                code: "print('before the boom')\nraise Exception()".to_string(),
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    let mut stderr_events: Vec<Vec<u8>> = Vec::new();
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::OutputEvent(event)) => {
                if event.stream == OutputStream::Stderr as i32 {
                    stderr_events.push(event.data);
                }
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(
                    !result.success,
                    "uncaught exception should report success=false"
                );
                // Classified as a script exception, not a sandbox failure.
                assert_eq!(
                    result.failure_kind,
                    FailureKind::ScriptFailed as i32,
                    "uncaught exception should be FAILURE_KIND_SCRIPT_FAILED"
                );
                // `error` is reserved for sandbox failures, so it stays empty.
                assert!(
                    result.error.is_empty(),
                    "error should be empty for a script exception, got: {:?}",
                    result.error
                );
                // The traceback is surfaced through stderr (matching the stream).
                assert!(
                    result.stderr.contains_str("Traceback")
                        && result.stderr.contains_str("Exception"),
                    "stderr field should carry the traceback, got: {:?}",
                    result.stderr
                );
                // stdout produced before the exception is preserved too.
                assert!(
                    result.stdout.contains_str("before the boom"),
                    "stdout field should carry pre-exception output, got: {:?}",
                    result.stdout
                );
                got_result = true;
            }
            _ => {}
        }
    }

    assert!(got_result, "never received ExecuteResult");
    // The traceback IS delivered live as STDERR output events during execution.
    let streamed = stderr_events.concat();
    assert!(
        streamed.contains_str("Traceback") && streamed.contains_str("Exception"),
        "expected the traceback to be streamed as STDERR events, got: {:?}",
        stderr_events
    );
}

/// A wall-clock timeout is a sandbox failure, not a script failure: it is
/// classified `FAILURE_KIND_TIMEOUT` with a populated `error` (the limit isn't
/// something CPython surfaces, so it belongs in `error`, not stderr).
#[tokio::test]
async fn execute_timeout_is_sandbox_failure() {
    use eryx_server::proto::eryx::v1::FailureKind;

    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                // Busy loop that never returns before the tiny timeout.
                code: "while True:\n    pass".to_string(),
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 200,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(server_message::Message::ExecuteResult(result)) = msg.message {
            assert!(!result.success, "timeout should report success=false");
            assert_eq!(
                result.failure_kind,
                FailureKind::Timeout as i32,
                "timeout should be FAILURE_KIND_TIMEOUT"
            );
            assert!(
                !result.error.is_empty(),
                "a sandbox failure should populate the error field"
            );
            got_result = true;
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

/// NUL bytes in the code are rejected host-side before reaching the guest, and
/// classified as a sandbox/input failure rather than a script exception.
#[tokio::test]
async fn execute_null_bytes_is_sandbox_failure() {
    use eryx_server::proto::eryx::v1::FailureKind;

    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "x = 1\0y = 2".to_string(),
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(server_message::Message::ExecuteResult(result)) = msg.message {
            assert!(!result.success, "null bytes should report success=false");
            assert_eq!(
                result.failure_kind,
                FailureKind::SandboxError as i32,
                "null bytes should be FAILURE_KIND_SANDBOX_ERROR, not a script exception"
            );
            assert!(
                !result.error.is_empty(),
                "a sandbox failure should populate the error field"
            );
            got_result = true;
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

/// A secret surfaced via an uncaught exception's traceback must not leak. Two
/// guarantees compound here: Python only ever sees a random *placeholder* (never
/// the real value), and stderr scrubbing replaces that placeholder with
/// `[REDACTED]`. Verified across the streamed events, the `stderr` field, and
/// the (empty) `error` field.
#[tokio::test]
async fn execute_secret_in_uncaught_exception_is_scrubbed() {
    use eryx_server::proto::eryx::v1::{FailureKind, OutputStream, SecretConfig};

    const REAL_SECRET: &str = "s3cr3t-value-do-not-leak-42";

    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "import os\nraise Exception(os.environ['MY_SECRET'])".to_string(),
                secrets: vec![SecretConfig {
                    name: "MY_SECRET".to_string(),
                    value: REAL_SECRET.to_string(),
                    allowed_hosts: vec![],
                    ..Default::default()
                }],
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    let mut stderr_events: Vec<Vec<u8>> = Vec::new();
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::OutputEvent(event)) => {
                if event.stream == OutputStream::Stderr as i32 {
                    stderr_events.push(event.data);
                }
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(!result.success);
                assert_eq!(result.failure_kind, FailureKind::ScriptFailed as i32);
                // The real secret never reaches Python, so it cannot appear
                // anywhere, and the placeholder must be scrubbed from stderr.
                assert!(
                    !result.stderr.contains_str(REAL_SECRET),
                    "real secret leaked into stderr: {:?}",
                    result.stderr
                );
                assert!(
                    !result.stderr.contains_str("ERYX_SECRET_PLACEHOLDER_"),
                    "unscrubbed placeholder leaked into stderr: {:?}",
                    result.stderr
                );
                assert!(
                    result.stderr.contains_str("[REDACTED]"),
                    "expected the placeholder to be redacted in the traceback: {:?}",
                    result.stderr
                );
                // `error` is empty for a script exception — nothing to leak there.
                assert!(result.error.is_empty());
                got_result = true;
            }
            _ => {}
        }
    }

    assert!(got_result, "never received ExecuteResult");
    // The real secret can never appear in the live stream either — Python only
    // ever holds the placeholder. (We don't assert placeholder-absence on the
    // raw stream: per-chunk scrubbing can't redact a placeholder split across
    // two writes. The final `stderr` field, scrubbed whole, is the guarantee.)
    let streamed = stderr_events.concat();
    assert!(
        !streamed.contains_str(REAL_SECRET),
        "real secret leaked into streamed stderr events: {:?}",
        stderr_events
    );
}

#[tokio::test]
async fn execute_with_echo_callback() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    // Send the execute request with an echo callback declaration.
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: r#"
result = await echo(message="hello callback")
print(f"got: {result}")
"#
                .to_string(),
                callbacks: vec![CallbackDeclaration {
                    name: "echo".to_string(),
                    description: "Echoes the message back".to_string(),
                    parameters: vec![ParameterDeclaration {
                        name: "message".to_string(),
                        json_type: "string".to_string(),
                        description: "The message to echo".to_string(),
                        required: true,
                    }],
                }],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::CallbackRequest(req)) => {
                assert_eq!(req.name, "echo");
                // Parse the arguments and echo back the message.
                let args: serde_json::Value = serde_json::from_str(&req.arguments_json).unwrap();
                let message = args["message"].as_str().unwrap();
                let response_json = serde_json::json!({ "echoed": message }).to_string();

                tx.send(ClientMessage {
                    message: Some(client_message::Message::CallbackResponse(
                        eryx_server::proto::eryx::v1::CallbackResponse {
                            request_id: req.request_id,
                            outcome: 0,
                            result: Some(callback_response::Result::JsonResult(response_json)),
                        },
                    )),
                })
                .await
                .unwrap();
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.success, "execution failed: {}", result.error);
                assert!(
                    result.stdout.contains_str("got:"),
                    "stdout missing callback result: {:?}",
                    result.stdout
                );
                assert!(
                    result.stdout.contains_str("echoed"),
                    "stdout missing echoed value: {:?}",
                    result.stdout
                );
                got_result = true;
            }
            Some(server_message::Message::OutputEvent(_)) => {
                // Output events are expected; just consume them.
            }
            _ => {}
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

/// With `callback_namespace`, the script calls `tools.echo` and the client
/// is asked for `echo`, its declared name (#964).
#[tokio::test]
async fn execute_with_callback_namespace() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel.clone());
    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "print(await tools.echo(message='hi'))\nprint('echo' in globals())"
                    .to_string(),
                callbacks: vec![echo_declaration()],
                callback_namespace: "tools".to_string(),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();
    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    let mut stdout = None;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::CallbackRequest(req)) => {
                assert_eq!(req.name, "echo");
                tx.send(ClientMessage {
                    message: Some(client_message::Message::CallbackResponse(
                        eryx_server::proto::eryx::v1::CallbackResponse {
                            request_id: req.request_id,
                            outcome: 0,
                            result: Some(callback_response::Result::JsonResult(req.arguments_json)),
                        },
                    )),
                })
                .await
                .unwrap();
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.success, "execution failed: {}", result.error);
                stdout = Some(result.stdout);
            }
            _ => {}
        }
    }
    assert_eq!(
        stdout.expect("never received ExecuteResult"),
        b"{'message': 'hi'}\nFalse\n"
    );

    let status = execute_request(
        channel,
        ExecuteRequest {
            code: "pass".to_string(),
            callbacks: vec![echo_declaration()],
            callback_namespace: "to.ols".to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument, "{status}");
}

#[tokio::test]
async fn execute_with_callback_error() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: r#"
try:
    await failing_op()
    print("should not reach here")
except Exception as e:
    print(f"caught: {e}")
"#
                .to_string(),
                callbacks: vec![CallbackDeclaration {
                    name: "failing_op".to_string(),
                    description: "Always fails".to_string(),
                    parameters: vec![],
                }],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::CallbackRequest(req)) => {
                assert_eq!(req.name, "failing_op");
                // Respond with an error.
                tx.send(ClientMessage {
                    message: Some(client_message::Message::CallbackResponse(
                        eryx_server::proto::eryx::v1::CallbackResponse {
                            request_id: req.request_id,
                            outcome: 0,
                            result: Some(callback_response::Result::Error(
                                "datasource unavailable".to_string(),
                            )),
                        },
                    )),
                })
                .await
                .unwrap();
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(
                    result.success,
                    "execution should succeed (error was caught)"
                );
                assert!(
                    result.stdout.contains_str("caught:"),
                    "stdout should contain caught error: {:?}",
                    result.stdout
                );
                got_result = true;
            }
            _ => {}
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

#[tokio::test]
async fn execute_streams_output_events() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: r#"
for i in range(3):
    print(f"line {i}")
"#
                .to_string(),
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut output_events = Vec::new();
    let mut got_result = false;

    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::OutputEvent(event)) => {
                output_events.push(event.data);
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.success, "execution failed: {}", result.error);
                // Final result should also have the complete stdout.
                assert!(result.stdout.contains_str("line 0"));
                assert!(result.stdout.contains_str("line 2"));
                got_result = true;
            }
            _ => {}
        }
    }

    assert!(got_result, "never received ExecuteResult");
    // We should have received at least one output event.
    assert!(!output_events.is_empty(), "expected output events");
    let all_output: Vec<u8> = output_events.concat();
    assert!(
        all_output.contains_str("line 0"),
        "output events missing expected content: {:?}",
        String::from_utf8_lossy(&all_output)
    );
}

#[tokio::test]
async fn execute_reports_stats() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "x = 1 + 1".to_string(),
                callbacks: vec![],
                resource_limits: None,
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(server_message::Message::ExecuteResult(result)) = msg.message {
            assert!(result.success);
            let stats = result.stats.expect("should have stats");
            assert!(stats.duration_ms > 0, "duration should be non-zero");
        }
    }
}

#[tokio::test]
async fn execute_with_tracing_streams_trace_events() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: r#"
x = 1
y = 2
print(x + y)
"#
                .to_string(),
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: true,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut trace_events = Vec::new();
    let mut got_result = false;

    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::TraceEvent(event)) => {
                // Every trace event reaches the client before the result.
                assert!(
                    !got_result,
                    "trace event after the ExecuteResult: {event:?}"
                );
                trace_events.push(event);
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.success, "execution failed: {}", result.error);
                assert!(
                    result.stdout.contains_str("3"),
                    "stdout missing expected output: {:?}",
                    result.stdout
                );
                got_result = true;
            }
            _ => {}
        }
    }

    assert!(got_result, "never received ExecuteResult");
    // We should have received trace events when tracing is enabled.
    assert!(
        !trace_events.is_empty(),
        "expected trace events when enable_tracing is true"
    );
    // Should have at least some "line" events.
    let line_events: Vec<_> = trace_events
        .iter()
        .filter(|e| e.event_type == "line")
        .collect();
    assert!(
        !line_events.is_empty(),
        "expected at least one 'line' trace event, got: {:?}",
        trace_events
            .iter()
            .map(|e| &e.event_type)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn execute_without_tracing_no_trace_events() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "x = 1 + 1".to_string(),
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut trace_events = Vec::new();

    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(server_message::Message::TraceEvent(event)) = msg.message {
            trace_events.push(event);
        }
    }

    assert!(
        trace_events.is_empty(),
        "should not receive trace events when enable_tracing is false, got {}",
        trace_events.len()
    );
}

/// Verify that trace event line numbers are adjusted to account for the
/// injected preamble (BUILTINS_INJECT). Line 1 in user code should report
/// lineno=1, not lineno=5 (preamble is 4 lines).
#[tokio::test]
async fn execute_trace_linenos_adjusted_for_preamble() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    // User code: 3 lines, no leading newline. Line 1 = x = 1, line 2 = y = 2, line 3 = print.
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "x = 1\ny = 2\nprint(x + y)".to_string(),
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: true,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut trace_events = Vec::new();
    let mut got_result = false;

    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::TraceEvent(event)) => {
                trace_events.push(event);
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.success, "execution failed: {}", result.error);
                got_result = true;
            }
            _ => {}
        }
    }

    assert!(got_result, "never received ExecuteResult");

    // Extract "line" events and their line numbers.
    let line_nos: Vec<u32> = trace_events
        .iter()
        .filter(|e| e.event_type == "line")
        .map(|e| e.lineno)
        .collect();

    assert!(!line_nos.is_empty(), "expected line trace events, got none");

    // All line numbers should be within the user code range (1..=3).
    // None should reference preamble lines (which would be > 3 without adjustment).
    for lineno in &line_nos {
        assert!(
            *lineno >= 1 && *lineno <= 3,
            "trace lineno {} is outside user code range 1..=3 (preamble not adjusted?), all line events: {:?}",
            lineno,
            line_nos
        );
    }

    // We should see line events for all 3 user lines.
    assert!(
        line_nos.contains(&1),
        "missing trace for line 1, got: {:?}",
        line_nos
    );
    assert!(
        line_nos.contains(&2),
        "missing trace for line 2, got: {:?}",
        line_nos
    );
    assert!(
        line_nos.contains(&3),
        "missing trace for line 3, got: {:?}",
        line_nos
    );
}

/// Same as above but with a leading newline in user code (common pattern from LLMs).
/// Line numbers should still be relative to user code, with the leading blank line
/// counting as line 1.
#[tokio::test]
async fn execute_trace_linenos_adjusted_with_leading_newline() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    // Leading newline: line 1 = empty, line 2 = x = 42, line 3 = print.
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "\nx = 42\nprint(x)".to_string(),
                callbacks: vec![],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: true,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut trace_events = Vec::new();
    let mut got_result = false;

    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::TraceEvent(event)) => {
                trace_events.push(event);
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.success, "execution failed: {}", result.error);
                assert!(result.stdout.contains_str("42"));
                got_result = true;
            }
            _ => {}
        }
    }

    assert!(got_result, "never received ExecuteResult");

    let line_nos: Vec<u32> = trace_events
        .iter()
        .filter(|e| e.event_type == "line")
        .map(|e| e.lineno)
        .collect();

    assert!(!line_nos.is_empty(), "expected line trace events");

    // User code is 3 lines (empty + x=42 + print). Line numbers should be 1..=3.
    for lineno in &line_nos {
        assert!(
            *lineno >= 1 && *lineno <= 3,
            "trace lineno {} is outside user code range 1..=3, all: {:?}",
            lineno,
            line_nos
        );
    }

    // Should see line 2 (x = 42) and line 3 (print).
    assert!(
        line_nos.contains(&2),
        "missing trace for line 2, got: {:?}",
        line_nos
    );
    assert!(
        line_nos.contains(&3),
        "missing trace for line 3, got: {:?}",
        line_nos
    );
}

/// Verify that callbacks are correctly re-initialized when a pooled sandbox
/// handles requests with different callbacks. This tests that the WASM runtime
/// does NOT cache stale callback state across pool reuse.
#[tokio::test]
async fn execute_pool_reuse_different_callbacks() {
    let channel = start_server().await;

    // --- Request 1: call "alpha" callback ---
    {
        let mut client = EryxClient::new(channel.clone());
        let (tx, rx) = mpsc::channel(16);

        tx.send(ClientMessage {
            message: Some(client_message::Message::ExecuteRequest(Box::new(
                ExecuteRequest {
                    code: r#"
result = await alpha(value="first")
print(f"alpha: {result}")
"#
                    .to_string(),
                    callbacks: vec![CallbackDeclaration {
                        name: "alpha".to_string(),
                        description: "Alpha callback".to_string(),
                        parameters: vec![ParameterDeclaration {
                            name: "value".to_string(),
                            json_type: "string".to_string(),
                            description: "A value".to_string(),
                            required: true,
                        }],
                    }],
                    resource_limits: Some(ResourceLimits {
                        execution_timeout_ms: 30_000,
                        ..Default::default()
                    }),
                    enable_tracing: false,
                    persist_state: false,
                    state_snapshot: vec![],
                    files: vec![],
                    network_config: None,
                    ..Default::default()
                },
            ))),
        })
        .await
        .unwrap();

        let mut stream = client
            .execute(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        let mut got_result = false;
        while let Some(msg) = stream.message().await.unwrap() {
            match msg.message {
                Some(server_message::Message::CallbackRequest(req)) => {
                    assert_eq!(req.name, "alpha");
                    let response_json = serde_json::json!({ "from": "alpha" }).to_string();
                    tx.send(ClientMessage {
                        message: Some(client_message::Message::CallbackResponse(
                            eryx_server::proto::eryx::v1::CallbackResponse {
                                request_id: req.request_id,
                                outcome: 0,
                                result: Some(callback_response::Result::JsonResult(response_json)),
                            },
                        )),
                    })
                    .await
                    .unwrap();
                }
                Some(server_message::Message::ExecuteResult(result)) => {
                    assert!(result.success, "request 1 failed: {}", result.error);
                    assert!(
                        result.stdout.contains_str("alpha"),
                        "request 1 stdout missing alpha: {:?}",
                        result.stdout
                    );
                    got_result = true;
                }
                _ => {}
            }
        }
        assert!(got_result, "never received ExecuteResult for request 1");
    }

    // --- Request 2: call "beta" callback (different from request 1) ---
    {
        let mut client = EryxClient::new(channel.clone());
        let (tx, rx) = mpsc::channel(16);

        tx.send(ClientMessage {
            message: Some(client_message::Message::ExecuteRequest(Box::new(
                ExecuteRequest {
                    code: r#"
# list_callbacks should be available and show beta (not alpha)
cbs = list_callbacks()
cb_names = [cb['name'] for cb in cbs]
print(f"callbacks: {cb_names}")
assert 'beta' in cb_names, f"beta not in callbacks: {cb_names}"

result = await beta(value="second")
print(f"beta: {result}")
"#
                    .to_string(),
                    callbacks: vec![CallbackDeclaration {
                        name: "beta".to_string(),
                        description: "Beta callback".to_string(),
                        parameters: vec![ParameterDeclaration {
                            name: "value".to_string(),
                            json_type: "string".to_string(),
                            description: "A value".to_string(),
                            required: true,
                        }],
                    }],
                    resource_limits: Some(ResourceLimits {
                        execution_timeout_ms: 30_000,
                        ..Default::default()
                    }),
                    enable_tracing: false,
                    persist_state: false,
                    state_snapshot: vec![],
                    files: vec![],
                    network_config: None,
                    ..Default::default()
                },
            ))),
        })
        .await
        .unwrap();

        let mut stream = client
            .execute(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        let mut got_result = false;
        while let Some(msg) = stream.message().await.unwrap() {
            match msg.message {
                Some(server_message::Message::CallbackRequest(req)) => {
                    assert_eq!(req.name, "beta");
                    let response_json = serde_json::json!({ "from": "beta" }).to_string();
                    tx.send(ClientMessage {
                        message: Some(client_message::Message::CallbackResponse(
                            eryx_server::proto::eryx::v1::CallbackResponse {
                                request_id: req.request_id,
                                outcome: 0,
                                result: Some(callback_response::Result::JsonResult(response_json)),
                            },
                        )),
                    })
                    .await
                    .unwrap();
                }
                Some(server_message::Message::ExecuteResult(result)) => {
                    assert!(
                        result.success,
                        "request 2 failed: {} | stdout: {}",
                        result.error,
                        result.stdout.to_lossy()
                    );
                    assert!(
                        result.stdout.contains_str("beta"),
                        "request 2 stdout missing beta: {:?}",
                        result.stdout
                    );
                    got_result = true;
                }
                _ => {}
            }
        }
        assert!(got_result, "never received ExecuteResult for request 2");
    }
}

/// Test that callback results containing special characters (triple quotes,
/// backslashes, JSON with nested escapes) are handled correctly.
///
/// This verifies the fix for the fragile triple-quoted string interpolation
/// in `set_async_import_result`, which previously broke when the callback
/// result contained `'''` or certain escape sequences.
#[tokio::test]
async fn execute_callback_with_special_characters_in_result() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: r#"
result = await get_data()
# Verify we can access the data correctly
print(f"name={result['name']}")
print(f"desc={result['description']}")
print(f"path={result['path']}")
"#
                .to_string(),
                callbacks: vec![CallbackDeclaration {
                    name: "get_data".to_string(),
                    description: "Returns data with special characters".to_string(),
                    parameters: vec![],
                }],
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                enable_tracing: false,
                persist_state: false,
                state_snapshot: vec![],
                files: vec![],
                network_config: None,
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::CallbackRequest(req)) => {
                assert_eq!(req.name, "get_data");
                // Return JSON with triple quotes, backslashes, and nested escapes
                let response_json = serde_json::json!({
                    "name": "test'''datasource",
                    "description": "A datasource with '''triple quotes''' and \\backslashes\\",
                    "path": "C:\\Users\\test\\config.yaml",
                })
                .to_string();

                tx.send(ClientMessage {
                    message: Some(client_message::Message::CallbackResponse(
                        eryx_server::proto::eryx::v1::CallbackResponse {
                            request_id: req.request_id,
                            outcome: 0,
                            result: Some(callback_response::Result::JsonResult(response_json)),
                        },
                    )),
                })
                .await
                .unwrap();
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(
                    result.success,
                    "execution failed: {} (stderr: {})",
                    result.error,
                    result.stderr.to_lossy()
                );
                assert!(
                    result.stdout.contains_str("name=test'''datasource"),
                    "stdout missing name with triple quotes: {:?}",
                    result.stdout
                );
                assert!(
                    result
                        .stdout
                        .contains_str("path=C:\\Users\\test\\config.yaml"),
                    "stdout missing path with backslashes: {:?}",
                    result.stdout
                );
                got_result = true;
            }
            _ => {}
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

#[tokio::test]
async fn execute_captures_result() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "result = {\"a\": 1, \"b\": [2, 3]}".to_string(),
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(server_message::Message::ExecuteResult(result)) = msg.message {
            assert!(result.success, "execution failed: {}", result.error);
            assert_eq!(result.result, "{\"a\": 1, \"b\": [2, 3]}");
            assert_eq!(result.result_error, "");
            got_result = true;
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

#[tokio::test]
async fn execute_captures_custom_result_variable() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "out = 42\nresult = 1".to_string(),
                result_variable: "out".to_string(),
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        if let Some(server_message::Message::ExecuteResult(result)) = msg.message {
            assert!(result.success, "execution failed: {}", result.error);
            assert_eq!(result.result, "42");
            got_result = true;
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

// Guards the ordering of the per-request result-variable override relative to
// state restore: a request that both restores a snapshot AND sets a custom
// result_variable must still capture the custom variable. Two steps: capture a
// snapshot, then restore it in a second request with a custom variable name.
#[tokio::test]
async fn execute_custom_result_variable_with_state_restore() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    // Step 1: run with persist_state to obtain a state snapshot.
    let (tx1, rx1) = mpsc::channel(16);
    tx1.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "seed = 7".to_string(),
                persist_state: true,
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();
    let mut stream1 = client
        .execute(ReceiverStream::new(rx1))
        .await
        .unwrap()
        .into_inner();
    let mut snapshot = vec![];
    while let Some(msg) = stream1.message().await.unwrap() {
        if let Some(server_message::Message::ExecuteResult(r)) = msg.message {
            assert!(r.success, "step 1 failed: {}", r.error);
            snapshot = r.state_snapshot;
        }
    }
    assert!(
        !snapshot.is_empty(),
        "expected a state snapshot from step 1"
    );

    // Step 2: restore the snapshot AND request a custom result variable.
    let (tx2, rx2) = mpsc::channel(16);
    tx2.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "out = seed * 6".to_string(),
                persist_state: true,
                state_snapshot: snapshot,
                result_variable: "out".to_string(),
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 30_000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();
    let mut stream2 = client
        .execute(ReceiverStream::new(rx2))
        .await
        .unwrap()
        .into_inner();
    let mut got = false;
    while let Some(msg) = stream2.message().await.unwrap() {
        if let Some(server_message::Message::ExecuteResult(r)) = msg.message {
            assert!(r.success, "step 2 failed: {}", r.error);
            assert_eq!(r.result, "42", "custom result var lost across restore");
            got = true;
        }
    }
    assert!(got, "never received ExecuteResult in step 2");
}

/// Build the `echo` callback declaration used by the replay tests.
fn echo_declaration() -> CallbackDeclaration {
    CallbackDeclaration {
        name: "echo".to_string(),
        description: "Echoes the message back".to_string(),
        parameters: vec![ParameterDeclaration {
            name: "message".to_string(),
            json_type: "string".to_string(),
            description: "The message to echo".to_string(),
            required: true,
        }],
    }
}

/// A first run with an (empty) journal records the callback; a second run with
/// that journal replays it — the server never dispatches the callback again.
#[tokio::test]
async fn replay_across_requests_skips_live_callback() {
    let signer = eryx_server::replay::JournalSigner::random();
    let channel = start_server_with_signer(signer.clone()).await;
    let mut client = EryxClient::new(channel);

    let code = r#"
result = await echo(message="hello callback")
print(f"got: {result}")
"#;

    // ---- Run 1: empty journal present => journaling on, nothing to replay. ----
    let (tx1, rx1) = mpsc::channel(16);
    tx1.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: code.to_string(),
                callbacks: vec![echo_declaration()],
                // Presence of the (empty) journal opts into replay/journaling.
                callback_journal: Some(CallbackJournal {
                    entries: vec![],
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream1 = client
        .execute(ReceiverStream::new(rx1))
        .await
        .unwrap()
        .into_inner();

    let mut live_callbacks = 0;
    let mut recorded_journal = None;
    while let Some(msg) = stream1.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::CallbackRequest(req)) => {
                live_callbacks += 1;
                let args: serde_json::Value = serde_json::from_str(&req.arguments_json).unwrap();
                let message = args["message"].as_str().unwrap();
                let response_json = serde_json::json!({ "echoed": message }).to_string();
                tx1.send(ClientMessage {
                    message: Some(client_message::Message::CallbackResponse(
                        eryx_server::proto::eryx::v1::CallbackResponse {
                            request_id: req.request_id,
                            outcome: 0,
                            result: Some(callback_response::Result::JsonResult(response_json)),
                        },
                    )),
                })
                .await
                .unwrap();
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.success, "run 1 failed: {}", result.error);
                assert_eq!(
                    result.stats.as_ref().map(|s| s.replayed_callbacks),
                    Some(0),
                    "nothing should be replayed on the first run"
                );
                recorded_journal = result.callback_journal;
            }
            _ => {}
        }
    }
    assert_eq!(live_callbacks, 1, "callback dispatched live once on run 1");
    let journal = recorded_journal.expect("run 1 returned a journal");
    assert_eq!(journal.entries.len(), 1, "one callback journaled");
    assert_eq!(journal.entries[0].name, "echo");

    // A replacement with the same key must preserve completed callbacks.
    let mut client = EryxClient::new(start_server_with_signer(signer).await);
    let (tx2, rx2) = mpsc::channel(16);
    tx2.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: code.to_string(),
                callbacks: vec![echo_declaration()],
                callback_journal: Some(journal),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream2 = client
        .execute(ReceiverStream::new(rx2))
        .await
        .unwrap()
        .into_inner();

    let mut live_callbacks2 = 0;
    let mut got_result = false;
    while let Some(msg) = stream2.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::CallbackRequest(_)) => live_callbacks2 += 1,
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.success, "run 2 failed: {}", result.error);
                assert!(
                    result.stdout.contains_str("echoed"),
                    "replayed value missing from stdout: {:?}",
                    result.stdout
                );
                assert_eq!(
                    result.stats.as_ref().map(|s| s.replayed_callbacks),
                    Some(1),
                    "the callback should have been replayed"
                );
                got_result = true;
            }
            _ => {}
        }
    }
    assert_eq!(
        live_callbacks2, 0,
        "replayed callback must not be dispatched to the client"
    );
    assert!(got_result, "run 2 never produced a result");
}

/// A client replying with CALLBACK_OUTCOME_SUSPEND stops execution cleanly: the
/// result reports the suspension (with the suspended callback details), the
/// guest is halted (fuel poisoned) before any later callback is dispatched, and
/// the journal of callbacks that completed before the suspension is preserved.
#[tokio::test]
async fn callback_suspension_reported_with_prefix_journal() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    // `marker` is dispatched *after* `approve` in the script. It must never reach
    // the client: the suspend gate rejects callbacks initiated after a suspension
    // and the fuel-poison halt traps the guest before it can dispatch `marker`.
    let code = r#"
a = await echo(message="first")
b = await approve(amount=100)
c = await marker(message="after-suspend")
print("should not reach here")
"#;
    let approve_decl = CallbackDeclaration {
        name: "approve".to_string(),
        description: "Requires approval".to_string(),
        parameters: vec![ParameterDeclaration {
            name: "amount".to_string(),
            json_type: "integer".to_string(),
            description: "Amount to approve".to_string(),
            required: true,
        }],
    };
    let marker_decl = CallbackDeclaration {
        name: "marker".to_string(),
        description: "Must never be dispatched after suspension".to_string(),
        parameters: vec![ParameterDeclaration {
            name: "message".to_string(),
            json_type: "string".to_string(),
            description: "The message".to_string(),
            required: true,
        }],
    };

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: code.to_string(),
                callbacks: vec![echo_declaration(), approve_decl, marker_decl],
                callback_journal: Some(CallbackJournal {
                    entries: vec![],
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    let mut marker_dispatched = false;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::CallbackRequest(req)) => {
                let (outcome, payload) = if req.name == "approve" {
                    // Defer: signal suspension with an opaque reason.
                    (
                        CallbackOutcome::Suspend as i32,
                        callback_response::Result::Error("needs human approval".to_string()),
                    )
                } else {
                    if req.name == "marker" {
                        marker_dispatched = true;
                    }
                    let response_json = serde_json::json!({ "echoed": "first" }).to_string();
                    (0, callback_response::Result::JsonResult(response_json))
                };
                tx.send(ClientMessage {
                    message: Some(client_message::Message::CallbackResponse(
                        eryx_server::proto::eryx::v1::CallbackResponse {
                            request_id: req.request_id,
                            outcome,
                            result: Some(payload),
                        },
                    )),
                })
                .await
                .unwrap();
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.suspended, "execution should be suspended");
                let suspended = result
                    .suspended_callback
                    .expect("suspended_callback should be set");
                assert_eq!(suspended.name, "approve");
                assert_eq!(suspended.reason, "needs human approval");
                // Only the completed `echo` callback is journaled.
                let journal = result.callback_journal.expect("journal returned");
                assert_eq!(journal.entries.len(), 1);
                assert_eq!(journal.entries[0].name, "echo");
                got_result = true;
            }
            _ => {}
        }
    }
    assert!(got_result, "never received ExecuteResult");
    assert!(
        !marker_dispatched,
        "callback after the suspension point must not be dispatched to the client"
    );
}

/// A client may reply CALLBACK_OUTCOME_SUSPEND even when the request did NOT opt
/// into replay (no `callback_journal`). The result must still report the
/// suspension — `suspended` true, `failure_kind` SUSPENDED, the reason carried —
/// rather than being mis-surfaced as an ordinary sandbox error.
#[tokio::test]
async fn suspend_without_replay_opt_in_is_reported_as_suspended() {
    use eryx_server::proto::eryx::v1::FailureKind;

    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let code = r#"
b = await approve(amount=100)
print("should not reach here")
"#;
    let approve_decl = CallbackDeclaration {
        name: "approve".to_string(),
        description: "Requires approval".to_string(),
        parameters: vec![ParameterDeclaration {
            name: "amount".to_string(),
            json_type: "integer".to_string(),
            description: "Amount to approve".to_string(),
            required: true,
        }],
    };

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: code.to_string(),
                callbacks: vec![approve_decl],
                // NOTE: no callback_journal — replay/journaling is NOT enabled.
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::CallbackRequest(req)) => {
                assert_eq!(req.name, "approve");
                tx.send(ClientMessage {
                    message: Some(client_message::Message::CallbackResponse(
                        eryx_server::proto::eryx::v1::CallbackResponse {
                            request_id: req.request_id,
                            outcome: CallbackOutcome::Suspend as i32,
                            result: Some(callback_response::Result::Error(
                                "needs human approval".to_string(),
                            )),
                        },
                    )),
                })
                .await
                .unwrap();
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(
                    result.suspended,
                    "suspension must be reported even without replay opt-in"
                );
                assert!(!result.success);
                assert_eq!(
                    result.failure_kind,
                    FailureKind::Suspended as i32,
                    "suspension must not be classified as a sandbox error"
                );
                let suspended = result
                    .suspended_callback
                    .expect("suspended_callback should carry the reason");
                assert_eq!(suspended.reason, "needs human approval");
                // No journal is returned when replay was not enabled.
                assert!(
                    result.callback_journal.is_none(),
                    "no journal without replay opt-in"
                );
                got_result = true;
            }
            _ => {}
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

/// A suspension on a stateful (persist_state) session must NOT attempt a state
/// snapshot: the guest's fuel is poisoned to 0 by the suspension, so the
/// snapshot export would trap and yield a misleading empty snapshot. The result
/// reports the suspension and returns an empty `state_snapshot` cleanly.
#[tokio::test]
async fn suspended_stateful_session_skips_snapshot() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let code = r#"
saved = 42
b = await approve(amount=100)
print("should not reach here")
"#;
    let approve_decl = CallbackDeclaration {
        name: "approve".to_string(),
        description: "Requires approval".to_string(),
        parameters: vec![ParameterDeclaration {
            name: "amount".to_string(),
            json_type: "integer".to_string(),
            description: "Amount to approve".to_string(),
            required: true,
        }],
    };

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: code.to_string(),
                callbacks: vec![approve_decl],
                persist_state: true,
                callback_journal: Some(CallbackJournal {
                    entries: vec![],
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();

    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();

    let mut got_result = false;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::CallbackRequest(req)) => {
                assert_eq!(req.name, "approve");
                tx.send(ClientMessage {
                    message: Some(client_message::Message::CallbackResponse(
                        eryx_server::proto::eryx::v1::CallbackResponse {
                            request_id: req.request_id,
                            outcome: CallbackOutcome::Suspend as i32,
                            result: Some(callback_response::Result::Error(
                                "needs human approval".to_string(),
                            )),
                        },
                    )),
                })
                .await
                .unwrap();
            }
            Some(server_message::Message::ExecuteResult(result)) => {
                assert!(result.suspended, "execution should be suspended");
                assert!(
                    result.state_snapshot.is_empty(),
                    "no snapshot should be captured for a suspended session"
                );
                got_result = true;
            }
            _ => {}
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

/// Invalid replay must fail before the RPC can expose any script output or
/// callback. Otherwise a replica/key change can repeat already-completed work.
#[tokio::test]
async fn rejects_invalid_replay_before_execution() {
    use eryx_server::proto::eryx::v1::CallbackJournalEntry;
    use eryx_server::replay::{JournalBinding, JournalSigner};

    let signer = JournalSigner::random();
    let channel = start_server_with_signer(signer.clone()).await;
    let mut client = EryxClient::new(channel);
    let code = "print('script must not execute')";
    let original = CallbackJournal {
        entries: vec![CallbackJournalEntry {
            name: "echo".to_string(),
            args_json: "{}".to_string(),
            value: "42".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ttl = Duration::from_secs(60);
    let no_env = BTreeMap::new();
    let env = BTreeMap::from([("A".to_string(), "1".to_string())]);
    let bound = |code| JournalBinding {
        code,
        state: None,
        callbacks: &[],
        environment: None,
        globals: "",
        callback_namespace: "",
        env: &no_env,
    };
    let echo = [echo_declaration()];
    for scenario in [
        "unsigned",
        "other replica",
        "empty signed journal",
        "changed script",
        "changed result",
        "changed state",
        "changed callbacks",
        "changed globals",
        "changed env",
        "expired",
        "extended expiry",
    ] {
        let mut journal = original.clone();
        match scenario {
            "unsigned" => {}
            "other replica" => JournalSigner::random().sign(&mut journal, bound(code), ttl),
            "empty signed journal" => {
                journal.entries.clear();
                JournalSigner::random().sign(&mut journal, bound(code), ttl);
            }
            "changed script" => signer.sign(&mut journal, bound("different script"), ttl),
            "changed result" => {
                signer.sign(&mut journal, bound(code), ttl);
                journal.entries[0].value = "43".to_string();
            }
            "changed state" => signer.sign(
                &mut journal,
                JournalBinding {
                    state: Some(b"other state"),
                    ..bound(code)
                },
                ttl,
            ),
            "changed callbacks" => signer.sign(
                &mut journal,
                JournalBinding {
                    callbacks: &echo,
                    ..bound(code)
                },
                ttl,
            ),
            "changed globals" => signer.sign(
                &mut journal,
                JournalBinding {
                    globals: r#"{"a": 1}"#,
                    ..bound(code)
                },
                ttl,
            ),
            "changed env" => signer.sign(
                &mut journal,
                JournalBinding {
                    env: &env,
                    ..bound(code)
                },
                ttl,
            ),
            "expired" => signer.sign(&mut journal, bound(code), Duration::ZERO),
            "extended expiry" => {
                signer.sign(&mut journal, bound(code), ttl);
                journal.expires_at_unix_secs += 3600;
            }
            _ => unreachable!(),
        }
        let (tx, rx) = mpsc::channel(1);
        tx.send(ClientMessage {
            message: Some(client_message::Message::ExecuteRequest(Box::new(
                ExecuteRequest {
                    code: code.to_string(),
                    callback_journal: Some(journal),
                    ..Default::default()
                },
            ))),
        })
        .await
        .unwrap();
        let error = client.execute(ReceiverStream::new(rx)).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{scenario}");
        assert!(
            error.message().contains("replay was not executed"),
            "{scenario}"
        );
    }
}

/// `ExecuteRequest.globals` become Python globals: a script can still start
/// with a `__future__` import, line numbers don't shift, numbers keep their
/// exact value, and with persisted state a later request's globals replace
/// the earlier ones.
#[tokio::test]
async fn python_globals_are_injected() {
    let channel = start_server().await;
    let request = |code: &str, globals: &str| ExecuteRequest {
        code: code.to_string(),
        globals: globals.to_string(),
        ..Default::default()
    };

    let result = execute_request(
        channel.clone(),
        request(
            "from __future__ import annotations\nprint(sum(inputs), big)",
            r#"{"inputs": [1, 2], "big": 123456789012345678901234567890}"#,
        ),
    )
    .await
    .unwrap();
    assert!(result.success, "execution failed: {}", result.error);
    assert_eq!(result.stdout, b"3 123456789012345678901234567890\n");

    let result = execute_request(
        channel.clone(),
        request("x = 1\n\nraise ValueError(inputs)", r#"{"inputs": [1]}"#),
    )
    .await
    .unwrap();
    assert!(!result.success);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("line 3") && stderr.contains("[1]"),
        "{stderr}"
    );

    let first = execute_request(
        channel.clone(),
        ExecuteRequest {
            persist_state: true,
            ..request("kept = g", r#"{"g": 1}"#)
        },
    )
    .await
    .unwrap();
    assert!(first.success, "first run failed: {}", first.error);
    let second = execute_request(
        channel,
        ExecuteRequest {
            persist_state: true,
            state_snapshot: first.state_snapshot,
            ..request("print(kept, g)", r#"{"g": 2}"#)
        },
    )
    .await
    .unwrap();
    assert!(second.success, "second run failed: {}", second.error);
    assert_eq!(second.stdout, b"1 2\n");
}

/// Globals that aren't a JSON object of fresh identifiers are rejected
/// before anything runs.
#[tokio::test]
async fn invalid_globals_are_rejected() {
    let channel = start_server().await;
    for (globals, expected) in [
        ("{", "JSON object"),
        ("[1]", "JSON object"),
        (r#"{"a b": 1}"#, "invalid global name"),
        (r#"{"print": 1}"#, "Python builtin"),
        (r#"{"__name__": 1}"#, "Python builtin"),
        (r#"{"__file__": 1}"#, "dunder"),
        (r#"{"echo": 1}"#, "callback"),
    ] {
        let status = execute_request(
            channel.clone(),
            ExecuteRequest {
                code: "pass".to_string(),
                globals: globals.to_string(),
                callbacks: vec![echo_declaration()],
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{globals}");
        assert!(status.message().contains(expected), "{globals}: {status:?}");
    }
}

/// Under a callback namespace, a global may not take the namespace's name,
/// which the callbacks are installed under, but may take a callback's own
/// name, which is free (#964, #1159).
#[tokio::test]
async fn globals_collide_with_the_callback_namespace() {
    let channel = start_server().await;
    let request = |globals: &str| ExecuteRequest {
        code: "print(get_time, type(tools).__name__)".to_string(),
        globals: globals.to_string(),
        callbacks: vec![CallbackDeclaration {
            name: "get_time".to_string(),
            ..Default::default()
        }],
        callback_namespace: "tools".to_string(),
        ..Default::default()
    };
    let status = execute_request(channel.clone(), request(r#"{"tools": 1}"#))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("collides with the callback"),
        "{status:?}"
    );

    let result = execute_request(channel, request(r#"{"get_time": 7}"#))
        .await
        .unwrap();
    assert!(result.success, "{}", result.error);
    assert_eq!(result.stdout, b"7 _EryxNamespace\n");
}

/// A malformed CIDR in `NetworkConfig` is a client error, not a silently
/// weaker network policy.
#[tokio::test]
async fn execute_invalid_network_cidr_is_rejected() {
    use eryx_server::proto::eryx::v1::NetworkConfig;

    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(1);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "pass".to_string(),
                network_config: Some(NetworkConfig {
                    allowed_networks: vec!["10.0.0.0/99".to_string()],
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();
    let error = client.execute(ReceiverStream::new(rx)).await.unwrap_err();
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    assert!(
        error
            .message()
            .contains("allowed_networks: invalid CIDR '10.0.0.0/99'"),
        "{}",
        error.message()
    );
}

/// Send `req` as the only client message, half-closing the request stream
/// right after it (like grpc-go's `CloseSend()`), and return the final
/// `ExecuteResult` or the RPC error.
async fn execute_half_closed(
    client: &mut EryxClient<Channel>,
    req: ExecuteRequest,
) -> Result<ExecuteResult, tonic::Status> {
    let request = tokio_stream::once(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(req))),
    });
    let mut stream = client.execute(request).await?.into_inner();
    let mut result = None;
    while let Some(msg) = stream.message().await? {
        if let Some(server_message::Message::ExecuteResult(r)) = msg.message {
            result = Some(*r);
        }
    }
    Ok(result.expect("stream ended without ExecuteResult"))
}

/// Half-closing the request stream is not a disconnect: the client is still
/// reading responses, so execution must run to completion.
#[tokio::test]
async fn half_closed_request_stream_completes_execution() {
    let mut client = EryxClient::new(start_server().await);
    let result = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "import time\ntime.sleep(0.2)\nprint('done')".to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(result.success, "execution failed: {}", result.error);
    assert!(result.stdout.contains_str("done"), "{:?}", result.stdout);
}

/// A state snapshot larger than gRPC's 4 MiB default message size can be both
/// returned and sent back.
#[tokio::test]
async fn large_state_snapshot_round_trips() {
    let mut client = EryxClient::new(start_server().await)
        .max_decoding_message_size(eryx_server::service::MAX_MESSAGE_SIZE)
        .max_encoding_message_size(eryx_server::service::MAX_MESSAGE_SIZE);
    let first = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "big = 'x' * (5 * 1024 * 1024)".to_string(),
            persist_state: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(first.success, "step 1 failed: {}", first.error);
    assert!(first.state_snapshot.len() > 5 * 1024 * 1024);

    let second = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "print(len(big))".to_string(),
            persist_state: true,
            state_snapshot: first.state_snapshot,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(second.success, "step 2 failed: {}", second.error);
    assert_eq!(second.state_error, "");
    assert!(second.stdout.contains_str("5242880"), "{:?}", second.stdout);
}

/// Globals the snapshot leaves out are named in `state_skipped_globals`.
#[tokio::test]
async fn state_snapshot_reports_skipped_globals() {
    let mut client = EryxClient::new(start_server().await);
    let result = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "x = 1\ngen = (i for i in range(3))".to_string(),
            persist_state: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(result.success, "execution failed: {}", result.error);
    assert!(!result.state_snapshot.is_empty());
    assert_eq!(result.state_skipped_globals, ["gen"]);
}

/// A snapshot that cannot be restored still runs the code against a clean
/// state, but says so in `state_error`.
#[tokio::test]
async fn unrestorable_state_snapshot_reports_state_error() {
    let mut client = EryxClient::new(start_server().await);
    for snapshot in [b"short".to_vec(), b"not a real snapshot at all".to_vec()] {
        let result = execute_half_closed(
            &mut client,
            ExecuteRequest {
                code: "print('ran')".to_string(),
                persist_state: true,
                state_snapshot: snapshot,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.success, "execution failed: {}", result.error);
        assert!(result.stdout.contains_str("ran"));
        assert!(
            result
                .state_error
                .contains("failed to restore state snapshot"),
            "{:?}",
            result.state_error
        );
        // The fresh state is still captured for the caller to keep.
        assert!(!result.state_snapshot.is_empty());
    }
}

/// Invalid supporting files, callback declarations and result variable names
/// are rejected before execution instead of being silently ignored.
#[tokio::test]
async fn invalid_request_inputs_are_rejected() {
    use eryx_server::proto::eryx::v1::{FileKind, SupportingFile};

    let file = |name: &str| SupportingFile {
        name: name.to_string(),
        content: "x = 1".to_string(),
        kind: FileKind::Module as i32,
    };
    let cases = [
        ExecuteRequest {
            files: vec![file("../escape.py")],
            ..Default::default()
        },
        ExecuteRequest {
            files: vec![file("dup.py"), file("dup.py")],
            ..Default::default()
        },
        ExecuteRequest {
            callbacks: vec![echo_declaration(), echo_declaration()],
            ..Default::default()
        },
        ExecuteRequest {
            callbacks: vec![CallbackDeclaration {
                name: "not-an-identifier".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        },
        ExecuteRequest {
            result_variable: "1bad".to_string(),
            ..Default::default()
        },
    ];
    let mut client = EryxClient::new(start_server().await);
    for req in cases {
        let req = ExecuteRequest {
            code: "pass".to_string(),
            ..req
        };
        let err = execute_half_closed(&mut client, req.clone())
            .await
            .expect_err(&format!("{req:?} was accepted"));
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err}");
    }
}

/// Failed executions report stats too, not only replay-enabled ones.
#[tokio::test]
async fn failed_execution_reports_stats() {
    let mut client = EryxClient::new(start_server().await);
    let result = execute_half_closed(
        &mut client,
        ExecuteRequest {
            // Grow guest memory first so peak memory is non-zero.
            code: "x = 'a' * (64 * 1024 * 1024)\nraise ValueError('boom')".to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!result.success);
    let stats = result.stats.expect("stats missing on failure");
    assert!(stats.peak_memory_bytes > 0, "{stats:?}");
    assert!(stats.fuel_consumed > 0, "{stats:?}");
}

/// A callback invoked after the client half-closed its request stream can never
/// get a response, so it fails at once instead of waiting for callback_timeout.
#[tokio::test]
async fn callback_after_half_close_fails_fast() {
    let mut client = EryxClient::new(start_server().await);
    let started = std::time::Instant::now();
    let result = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "import time\ntime.sleep(0.2)\nawait echo(message='hi')".to_string(),
            callbacks: vec![echo_declaration()],
            resource_limits: Some(ResourceLimits {
                execution_timeout_ms: 60_000,
                callback_timeout_ms: 60_000,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!result.success);
    assert!(
        result
            .stderr
            .contains_str("client closed the request stream"),
        "{:?}",
        result.stderr.to_lossy()
    );
    assert!(started.elapsed() < Duration::from_secs(20));
}

/// The message size limit is configurable.
#[tokio::test]
async fn max_message_size_is_configurable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let pool = SandboxPool::new(Sandbox::embedded(), PoolConfig::default())
        .await
        .unwrap();
    let service = EryxService::new(Arc::new(pool)).with_max_message_size(1024);
    tokio::spawn(
        Server::builder()
            .add_service(service.into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let mut client = EryxClient::connect(format!("http://{addr}")).await.unwrap();
    let ok = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "pass".to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(ok.success, "{}", ok.error);
    let err = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: format!("x = '{}'", "a".repeat(2048)),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), tonic::Code::OutOfRange, "{err}");
}

/// Run `code` with `limits` and return the result.
async fn run_with_limits(
    client: &mut EryxClient<Channel>,
    code: &str,
    limits: ResourceLimits,
) -> ExecuteResult {
    execute_half_closed(
        client,
        ExecuteRequest {
            code: code.to_string(),
            resource_limits: Some(limits),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

const ALLOCATE_200_MIB: &str = "x = bytearray(200 * 1024 * 1024)\nprint('allocated')";

/// The request's `max_memory_bytes` bounds guest memory.
#[tokio::test]
async fn memory_limit_is_enforced() {
    let mut client = EryxClient::new(start_server().await);
    let small = ResourceLimits {
        max_memory_bytes: 64 * 1024 * 1024,
        ..Default::default()
    };
    let result = run_with_limits(&mut client, ALLOCATE_200_MIB, small).await;
    assert!(!result.success, "allocation should fail: {result:?}");
    assert!(
        result.stderr.contains_str("MemoryError"),
        "{:?}",
        result.stderr.to_lossy()
    );

    let large = ResourceLimits {
        max_memory_bytes: 512 * 1024 * 1024,
        ..Default::default()
    };
    let result = run_with_limits(&mut client, ALLOCATE_200_MIB, large).await;
    assert!(
        result.success,
        "{}: {}",
        result.error,
        result.stderr.to_lossy()
    );
}

/// Request limits above the server's ceilings are clamped down to them.
#[tokio::test]
async fn request_limits_are_clamped_to_server_ceilings() {
    let limits = eryx_server::service::ServerLimits {
        max_memory_bytes: 64 * 1024 * 1024,
        max_execution_timeout: Duration::from_secs(1),
        ..Default::default()
    };
    let mut client = EryxClient::new(
        start_server_with(2, |pool| EryxService::new(pool).with_limits(limits)).await,
    );
    let generous = ResourceLimits {
        max_memory_bytes: 4 * 1024 * 1024 * 1024,
        execution_timeout_ms: 600_000,
        ..Default::default()
    };
    let result = run_with_limits(&mut client, ALLOCATE_200_MIB, generous).await;
    assert!(!result.success, "allocation should fail: {result:?}");
    assert!(
        result.stderr.contains_str("MemoryError"),
        "{:?}",
        result.stderr.to_lossy()
    );

    let started = std::time::Instant::now();
    let result = run_with_limits(&mut client, "while True: pass", generous).await;
    assert_eq!(
        result.failure_kind,
        eryx_server::proto::eryx::v1::FailureKind::Timeout as i32,
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(20));
}

/// Output past the server's cap is dropped from both the stream and the
/// result, with a marker and `output_truncated` set.
#[tokio::test]
async fn output_is_capped() {
    use eryx_server::proto::eryx::v1::OutputEvent;

    let limits = eryx_server::service::ServerLimits {
        max_output_bytes: 1000,
        ..Default::default()
    };
    let mut client = EryxClient::new(
        start_server_with(2, |pool| EryxService::new(pool).with_limits(limits)).await,
    );
    let request = tokio_stream::once(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "for _ in range(100):\n    print('x' * 99)".to_string(),
                ..Default::default()
            },
        ))),
    });
    let mut stream = client.execute(request).await.unwrap().into_inner();
    let mut streamed = Vec::new();
    let mut result = None;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::OutputEvent(OutputEvent { data, .. })) => {
                streamed.extend(data);
            }
            Some(server_message::Message::ExecuteResult(r)) => result = Some(*r),
            _ => {}
        }
    }
    let result = result.unwrap();
    assert!(result.success, "{}", result.error);
    assert!(result.output_truncated);
    assert_eq!(streamed, result.stdout);
    let marker = b"\n[output truncated: exceeded 1000 bytes]\n";
    assert_eq!(result.stdout.len(), 1000 + marker.len());
    assert!(
        result.stdout.ends_with(marker),
        "{:?}",
        result.stdout.to_lossy()
    );
}

/// Output the sandbox's own stream limit dropped (here, many tiny writes,
/// each charged an overhead) is reported as truncated even below the
/// server's cap.
#[tokio::test]
async fn sandbox_stream_limit_sets_output_truncated() {
    let mut client = EryxClient::new(start_server().await);
    let result = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "import _eryx\nfor _ in range(400_000):\n    _eryx._eryx_report_output(0, b'x')"
                .to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(result.success, "{}", result.error);
    assert!(result.stdout.len() < 400_000, "{}", result.stdout.len());
    assert!(result.output_truncated);
}

/// A run that fails after the sandbox's stream limit dropped output is still
/// reported as truncated.
#[tokio::test]
async fn failed_execution_reports_output_truncated() {
    let mut client = EryxClient::new(start_server().await);
    let result = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "import _eryx\nfor _ in range(400_000):\n    _eryx._eryx_report_output(0, b'x')\nraise ValueError('boom')"
                .to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!result.success);
    assert!(result.output_truncated);
}

/// Trace events dropped by the sandbox's stream limit do not mark complete
/// output as truncated.
#[tokio::test]
async fn dropped_trace_events_do_not_set_output_truncated() {
    let mut client = EryxClient::new(start_server().await);
    let request = tokio_stream::once(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "for i in range(300_000):\n    x = i\nprint('done')".to_string(),
                enable_tracing: true,
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 120_000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    });
    let mut stream = client.execute(request).await.unwrap().into_inner();
    let mut trace_events = 0usize;
    let mut result = None;
    while let Some(msg) = stream.message().await.unwrap() {
        match msg.message {
            Some(server_message::Message::TraceEvent(_)) => trace_events += 1,
            Some(server_message::Message::ExecuteResult(r)) => result = Some(*r),
            _ => {}
        }
    }
    let result = result.unwrap();
    assert!(result.success, "{}", result.error);
    // Two line events per iteration would be 600k; the budget stops far short.
    assert!(trace_events < 300_000, "{trace_events} trace events");
    assert_eq!(result.stdout.to_lossy().trim(), "done");
    assert!(!result.output_truncated);
}

/// A snapshot whose restore runs past the time bound loses the session's
/// state, but a request that also overrides `result_variable` still runs on a
/// clean state rather than failing.
#[tokio::test]
async fn restore_timeout_with_result_variable_runs_on_clean_state() {
    let mut client = EryxClient::new(start_server().await);
    let first = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "class Spin:\n    def __reduce__(self):\n        return (exec, ('while True: pass',))\nspin = Spin()"
                .to_string(),
            persist_state: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(first.success, "step 1 failed: {}", first.error);

    let second = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "out = 'spin' in globals()".to_string(),
            persist_state: true,
            state_snapshot: first.state_snapshot,
            result_variable: "out".to_string(),
            resource_limits: Some(ResourceLimits {
                execution_timeout_ms: 1_000,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(second.success, "step 2 failed: {}", second.error);
    assert_eq!(second.result, "false");
    assert!(
        second
            .state_error
            .contains("failed to restore state snapshot"),
        "{:?}",
        second.state_error
    );
}

/// A client disconnect stops CPU-bound code instead of leaving it to run
/// until the execution timeout.
#[tokio::test]
async fn disconnect_cancels_cpu_bound_execution() {
    let mut client = EryxClient::new(start_server_with(1, EryxService::new).await);
    let (tx, rx) = mpsc::channel(1);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "print('spinning', flush=True)\nwhile True: pass".to_string(),
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 120_000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();
    let mut stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    // Wait until the script is running, then drop both directions.
    stream.message().await.unwrap().unwrap();
    drop(stream);
    drop(tx);

    // The pool's only sandbox must come back well before the timeout.
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(25),
        execute_half_closed(
            &mut client,
            ExecuteRequest {
                code: "print('next')".to_string(),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("sandbox was not released after disconnect")
    .unwrap();
    assert!(result.success, "{}", result.error);
    assert!(started.elapsed() < Duration::from_secs(25));
}

/// A client disconnect also stops a slow state restore.
#[tokio::test]
async fn disconnect_cancels_state_restore() {
    let mut client = EryxClient::new(start_server_with(1, EryxService::new).await);
    let first = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "class Spin:\n    def __reduce__(self):\n        return (exec, ('while True: pass',))\nspin = Spin()"
                .to_string(),
            persist_state: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(first.success, "step 1 failed: {}", first.error);

    let (tx, rx) = mpsc::channel(1);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "pass".to_string(),
                persist_state: true,
                state_snapshot: first.state_snapshot,
                resource_limits: Some(ResourceLimits {
                    execution_timeout_ms: 120_000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))),
    })
    .await
    .unwrap();
    let stream = client
        .execute(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    // Let the restore start, then drop both directions.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(stream);
    drop(tx);

    // The pool's only sandbox must come back well before the restore's
    // 120 s bound.
    let result = tokio::time::timeout(
        Duration::from_secs(25),
        execute_half_closed(
            &mut client,
            ExecuteRequest {
                code: "print('next')".to_string(),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("sandbox was not released after disconnect")
    .unwrap();
    assert!(result.success, "{}", result.error);
}

/// A secret needs a host it can be sent to: with no `allowed_hosts` of its own
/// under `allow_all_hosts`, the request is rejected before execution.
#[tokio::test]
async fn secret_without_allowed_hosts_is_rejected() {
    use eryx_server::proto::eryx::v1::{NetworkConfig, SecretConfig};

    let secret = |allowed_hosts: Vec<String>| SecretConfig {
        name: "TOKEN".to_string(),
        value: "value".to_string(),
        allowed_hosts,
        ..Default::default()
    };
    let request = |secret, network_config| ExecuteRequest {
        code: "print('ran')".to_string(),
        secrets: vec![secret],
        network_config: Some(network_config),
        ..Default::default()
    };
    let allow_all = NetworkConfig {
        allow_all_hosts: true,
        ..Default::default()
    };
    let mut client = EryxClient::new(start_server().await);

    let err = execute_half_closed(&mut client, request(secret(vec![]), allow_all.clone()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err}");
    assert!(err.message().contains("TOKEN"), "{}", err.message());

    // Its own host list, or the network allow list, is enough.
    let ok = [
        request(secret(vec!["api.example.com".to_string()]), allow_all),
        request(
            secret(vec![]),
            NetworkConfig {
                allowed_hosts: vec!["api.example.com".to_string()],
                ..Default::default()
            },
        ),
    ];
    for req in ok {
        let result = execute_half_closed(&mut client, req).await.unwrap();
        assert!(result.success, "{}", result.error);
    }
}

/// On a server started with `--allow-plaintext-secrets`, secrets go over plain
/// TCP with the placeholder in place unless the request sets
/// `allow_plaintext_secrets`.
#[tokio::test]
async fn plaintext_secret_substitution_is_opt_in() {
    use eryx_server::proto::eryx::v1::{NetworkConfig, SecretConfig};
    use tokio::io::AsyncReadExt;

    const REAL_SECRET: &str = "plaintext-real-secret-77";
    let limits = eryx_server::service::ServerLimits {
        allow_plaintext_secrets: true,
        ..Default::default()
    };
    let mut client = EryxClient::new(
        start_server_with(2, |pool| EryxService::new(pool).with_limits(limits)).await,
    );

    for allow_plaintext_secrets in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            buf.truncate(n);
            buf
        });

        let code = format!(
            "import os, socket\n\
             s = socket.create_connection(('127.0.0.1', {port}))\n\
             s.sendall(f\"GET / HTTP/1.1\\r\\nHost: 127.0.0.1\\r\\nAuthorization: {{os.environ['TOKEN']}}\\r\\n\\r\\n\".encode())\n\
             s.close()"
        );
        let result = execute_half_closed(
            &mut client,
            ExecuteRequest {
                code,
                secrets: vec![SecretConfig {
                    name: "TOKEN".to_string(),
                    value: REAL_SECRET.to_string(),
                    allowed_hosts: vec!["127.0.0.1".to_string()],
                    ..Default::default()
                }],
                network_config: Some(NetworkConfig {
                    allow_all_hosts: true,
                    blocked_hosts: vec!["blocked.invalid".to_string()],
                    allowed_networks: vec!["127.0.0.0/8".to_string()],
                    allow_plaintext_secrets,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.success, "{}", result.error);

        let received = tokio::time::timeout(Duration::from_secs(30), server)
            .await
            .expect("the sandbox never connected")
            .unwrap();
        assert_eq!(
            received.contains_str(REAL_SECRET),
            allow_plaintext_secrets,
            "allow_plaintext_secrets={allow_plaintext_secrets}: {:?}",
            received.to_lossy()
        );
    }
}

/// A secret's `header` and `http_rules` limit where its real value is
/// substituted: a request outside them is refused, never sent. A rule that
/// can't match is rejected up front.
#[tokio::test]
async fn secret_scope_limits_substitution() {
    use eryx_server::proto::eryx::v1::{HttpRule, NetworkConfig, SecretConfig};
    use tokio::io::AsyncReadExt;

    const REAL_SECRET: &str = "scoped-real-secret-42";
    let limits = eryx_server::service::ServerLimits {
        allow_plaintext_secrets: true,
        ..Default::default()
    };
    let mut client = EryxClient::new(
        start_server_with(2, |pool| EryxService::new(pool).with_limits(limits)).await,
    );
    let request = |code, paths: Vec<String>| ExecuteRequest {
        code,
        secrets: vec![SecretConfig {
            name: "TOKEN".to_string(),
            value: REAL_SECRET.to_string(),
            allowed_hosts: vec!["127.0.0.1".to_string()],
            header: "authorization".to_string(),
            http_rules: vec![HttpRule {
                host: "127.0.0.1".to_string(),
                methods: vec!["GET".to_string()],
                paths,
                queries: vec!["service=git-upload-pack".to_string()],
            }],
        }],
        network_config: Some(NetworkConfig {
            allow_all_hosts: true,
            blocked_hosts: vec!["blocked.invalid".to_string()],
            allowed_networks: vec!["127.0.0.0/8".to_string()],
            allow_plaintext_secrets: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let err = execute_half_closed(
        &mut client,
        request("pass".to_string(), vec!["/a/../b".to_string()]),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err}");
    assert!(
        err.message().contains("secret 'TOKEN'"),
        "{}",
        err.message()
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut received = Vec::new();
        for _ in 0..3 {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            sock.read_to_end(&mut buf).await.unwrap();
            received.push(String::from_utf8_lossy(&buf).into_owned());
        }
        received
    });
    let code = format!(
        "import os, socket\n\
         for target, header in (('/repo/info/refs?service=git-receive-pack', 'Authorization'), ('/repo/info/refs?service=git-upload-pack', 'X-Token'), ('/repo/info/refs?service=git-upload-pack', 'Authorization')):\n\
         \x20   s = socket.create_connection(('127.0.0.1', {port}))\n\
         \x20   try:\n\
         \x20       s.sendall(f\"GET {{target}} HTTP/1.1\\r\\nHost: 127.0.0.1\\r\\n{{header}}: {{os.environ['TOKEN']}}\\r\\n\\r\\n\".encode())\n\
         \x20       print(target, header, 'sent')\n\
         \x20   except OSError:\n\
         \x20       print(target, header, 'refused')\n\
         \x20   s.close()"
    );
    let result = execute_half_closed(
        &mut client,
        request(code, vec!["/repo/info/refs".to_string()]),
    )
    .await
    .unwrap();
    assert!(result.success, "{}", result.error);
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.contains("git-receive-pack Authorization refused"),
        "{stdout}"
    );
    assert!(
        stdout.contains("git-upload-pack X-Token refused"),
        "{stdout}"
    );
    assert!(
        stdout.contains("git-upload-pack Authorization sent"),
        "{stdout}"
    );

    let received = tokio::time::timeout(Duration::from_secs(30), server)
        .await
        .expect("the sandbox never connected")
        .unwrap();
    assert_eq!(received[..2], ["", ""], "{received:?}");
    assert!(
        received[2].contains(&format!("Authorization: {REAL_SECRET}")),
        "{received:?}"
    );
}

/// Host patterns can name a port: a host listed with one is reachable only
/// on that port, a secret listed with one is substituted only there, and a
/// pattern whose port isn't one is rejected up front.
#[tokio::test]
async fn host_patterns_can_name_a_port() {
    use eryx_server::proto::eryx::v1::{NetworkConfig, SecretConfig};
    use tokio::io::AsyncReadExt;

    const REAL_SECRET: &str = "port-scoped-real-secret-7";
    let limits = eryx_server::service::ServerLimits {
        allow_plaintext_secrets: true,
        ..Default::default()
    };
    let mut client = EryxClient::new(
        start_server_with(2, |pool| EryxService::new(pool).with_limits(limits)).await,
    );
    let request = |code, allowed_hosts: Vec<String>, secret_hosts: Vec<String>| ExecuteRequest {
        code,
        secrets: vec![SecretConfig {
            name: "TOKEN".to_string(),
            value: REAL_SECRET.to_string(),
            allowed_hosts: secret_hosts,
            ..Default::default()
        }],
        network_config: Some(NetworkConfig {
            allowed_hosts,
            blocked_hosts: vec!["blocked.invalid".to_string()],
            allowed_networks: vec!["127.0.0.0/8".to_string()],
            allow_plaintext_secrets: true,
            ..Default::default()
        }),
        ..Default::default()
    };

    let err = execute_half_closed(
        &mut client,
        request("pass".to_string(), vec!["127.0.0.1:0".to_string()], vec![]),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err}");

    let (a, b) = (
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
    );
    let (port_a, port_b) = (
        a.local_addr().unwrap().port(),
        b.local_addr().unwrap().port(),
    );
    let accept = |listener: TcpListener| {
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            sock.read_to_end(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let (server_a, server_b) = (accept(a), accept(b));
    let code = format!(
        "import os, socket\n\
         for port in ({port_a}, {port_b}):\n\
         \x20   try:\n\
         \x20       s = socket.create_connection(('127.0.0.1', port))\n\
         \x20   except OSError:\n\
         \x20       print(port, 'unreachable')\n\
         \x20       continue\n\
         \x20   try:\n\
         \x20       s.sendall(f\"GET / HTTP/1.1\\r\\nHost: 127.0.0.1\\r\\nAuthorization: {{os.environ['TOKEN']}}\\r\\n\\r\\n\".encode())\n\
         \x20       print(port, 'sent')\n\
         \x20   except OSError:\n\
         \x20       print(port, 'refused')\n\
         \x20   s.close()\n\
         try:\n\
         \x20   socket.create_connection(('127.0.0.1', 1))\n\
         except OSError:\n\
         \x20   print(1, 'unreachable')"
    );
    let result = execute_half_closed(
        &mut client,
        request(
            code,
            vec![format!("127.0.0.1:{port_a}"), format!("127.0.0.1:{port_b}")],
            vec![format!("127.0.0.1:{port_a}")],
        ),
    )
    .await
    .unwrap();
    assert!(result.success, "{}", result.error);
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.contains(&format!("{port_a} sent")), "{stdout}");
    assert!(stdout.contains(&format!("{port_b} refused")), "{stdout}");
    assert!(stdout.contains("1 unreachable"), "{stdout}");

    let timeout = Duration::from_secs(30);
    let received_a = tokio::time::timeout(timeout, server_a)
        .await
        .unwrap()
        .unwrap();
    let received_b = tokio::time::timeout(timeout, server_b)
        .await
        .unwrap()
        .unwrap();
    assert!(
        received_a.contains(&format!("Authorization: {REAL_SECRET}")),
        "{received_a:?}"
    );
    assert_eq!(received_b, "", "{received_b:?}");
}

/// By default the server rejects a request that sets
/// `allow_plaintext_secrets`, instead of running it with the field ignored.
#[tokio::test]
async fn plaintext_secrets_are_rejected_by_default() {
    use eryx_server::proto::eryx::v1::NetworkConfig;

    let request = |allow_plaintext_secrets| ExecuteRequest {
        code: "print('ran')".to_string(),
        network_config: Some(NetworkConfig {
            allowed_hosts: vec!["api.example.com".to_string()],
            allow_plaintext_secrets,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut client = EryxClient::new(start_server().await);

    let err = execute_half_closed(&mut client, request(true))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err}");
    assert!(
        err.message().contains("--allow-plaintext-secrets"),
        "{}",
        err.message()
    );

    let result = execute_half_closed(&mut client, request(false))
        .await
        .unwrap();
    assert!(result.success, "{}", result.error);
}

/// `http_rules` refuse requests they don't allow (by path or query) before
/// anything is sent, and a rule path that can't match or a query pattern
/// that doesn't compile is rejected up front.
#[tokio::test]
async fn http_rules_refuse_requests() {
    use eryx_server::proto::eryx::v1::{HttpRule, NetworkConfig};
    use tokio::io::AsyncReadExt;

    let network_config = |paths: Vec<String>, queries: Vec<String>| NetworkConfig {
        allow_all_hosts: true,
        blocked_hosts: vec!["blocked.invalid".to_string()],
        allowed_networks: vec!["127.0.0.0/8".to_string()],
        http_rules: vec![HttpRule {
            host: "127.0.0.1".to_string(),
            methods: vec!["GET".to_string()],
            paths,
            queries,
        }],
        ..Default::default()
    };
    let mut client = EryxClient::new(start_server().await);

    for (paths, queries) in [
        (vec!["/a/../b".to_string()], vec![]),
        (vec![], vec!["a)|(b".to_string()]),
    ] {
        let err = execute_half_closed(
            &mut client,
            ExecuteRequest {
                code: "pass".to_string(),
                network_config: Some(network_config(paths, queries)),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err}");
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let mut received = Vec::new();
        for _ in 0..3 {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            sock.read_to_end(&mut buf).await.unwrap();
            received.push(String::from_utf8_lossy(&buf).into_owned());
        }
        received
    });
    let code = format!(
        "import socket\n\
         for path in ('/evil', '/ok/x?push', '/ok/x'):\n\
         \x20   s = socket.create_connection(('127.0.0.1', {port}))\n\
         \x20   try:\n\
         \x20       s.sendall(f'GET {{path}} HTTP/1.1\\r\\nHost: 127.0.0.1\\r\\n\\r\\n'.encode())\n\
         \x20       print(path, 'sent')\n\
         \x20   except OSError as e:\n\
         \x20       print(path, 'refused', e)\n\
         \x20   s.close()"
    );
    let result = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code,
            network_config: Some(network_config(
                vec!["/ok".to_string()],
                vec!["(fetch)?".to_string()],
            )),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(result.success, "{}", result.error);
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(stdout.contains("/evil refused"), "{stdout}");
    assert!(stdout.contains("/ok/x?push refused"), "{stdout}");
    assert!(stdout.contains("/ok/x sent"), "{stdout}");

    let received = tokio::time::timeout(Duration::from_secs(30), server)
        .await
        .expect("the sandbox never connected")
        .unwrap();
    assert_eq!(received[..2], ["", ""], "{received:?}");
    assert!(received[2].starts_with("GET /ok/x "), "{received:?}");
}

/// `state_was_reset` is set when restoring the snapshot was interrupted and
/// the code ran on a fresh interpreter, and is false otherwise.
#[tokio::test]
async fn state_was_reset_reports_an_interrupted_restore() {
    let mut client = EryxClient::new(start_server().await);
    // Unpickling this object runs `while True: pass`, so restoring it times out.
    let first = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "class Spin:\n    def __reduce__(self):\n        return (exec, ('while True: pass',))\nspin = Spin()".to_string(),
            persist_state: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(first.success, "{}", first.error);
    assert!(!first.state_was_reset);
    assert!(!first.state_snapshot.is_empty());

    let second = execute_half_closed(
        &mut client,
        ExecuteRequest {
            code: "print('spin' in globals())".to_string(),
            persist_state: true,
            state_snapshot: first.state_snapshot,
            resource_limits: Some(ResourceLimits {
                execution_timeout_ms: 1000,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(second.success, "{}", second.error);
    assert!(!second.state_error.is_empty());
    assert!(
        second.stdout.contains_str("False"),
        "{:?}",
        second.stdout.to_lossy()
    );
    assert!(second.state_was_reset);
}

/// The test-cmd fixture from the eryx crate (crates/eryx/tests/fixtures,
/// which needs `rustup target add wasm32-wasip2`), as a child command.
#[cfg(feature = "shell")]
fn test_cmd() -> eryx::Command {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../eryx/tests/fixtures/test-cmd/Cargo.toml");
    let target_dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("test-cmd");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--release", "--locked"])
        .args(["--target", "wasm32-wasip2", "--manifest-path"])
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .expect("running cargo");
    assert!(status.success(), "building the test-cmd fixture failed");
    eryx::Command::from_file(target_dir.join("wasm32-wasip2/release/eryx-test-cmd.wasm")).unwrap()
}

/// A plain-HTTP upstream on 127.0.0.1 answering 200 to every request.
/// Returns its port and the requests' heads.
#[cfg(feature = "shell")]
async fn http_upstream() -> (u16, Arc<std::sync::Mutex<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::<std::sync::Mutex<String>>::default();
    let received = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let received = Arc::clone(&received);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                received
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&head));
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    (port, seen)
}

/// `network_config.child_network` gives the shell's children the request's
/// network and never more: INHERIT is exactly the request's policy, NARROW
/// only what both allow, and without it children stay offline. Children see
/// a secret only as its placeholder, and the real value isn't sent where the
/// request doesn't allow it (here, plain HTTP).
#[cfg(feature = "shell")]
#[tokio::test]
async fn shell_children_get_the_requests_network_and_no_more() {
    use eryx_server::proto::eryx::v1::child_network::Mode;
    use eryx_server::proto::eryx::v1::{ChildNetwork, Language, NetworkConfig, SecretConfig};

    const REAL: &str = "real-grpc-child-secret-0123456789";
    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let builder = Sandbox::embedded()
                .shell()
                .with_command("tc", test_cmd())
                .with_command("tt", test_cmd().allow_tunnels());
            Ok(Arc::new(
                SandboxPool::new(builder, PoolConfig::default().with_max_size(2)).await?,
            ))
        })
    });
    let channel = start_server_with(1, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::Shell, create)
    })
    .await;
    let (port, seen) = http_upstream().await;

    // Only `host` on loopback (the defaults block it by name and address).
    let allowing = |host: &str| NetworkConfig {
        allowed_hosts: vec![host.to_string()],
        blocked_hosts: vec!["blocked.invalid".to_string()],
        allowed_networks: vec!["127.0.0.0/8".to_string()],
        ..Default::default()
    };
    let with_children = |mut network: NetworkConfig, mode: Mode, narrow: Option<NetworkConfig>| {
        network.child_network = Some(Box::new(ChildNetwork {
            mode: mode as i32,
            narrow: narrow.map(Box::new),
        }));
        network
    };
    let run_script = |code: String, network: Option<NetworkConfig>| {
        let channel = channel.clone();
        async move {
            let result = execute_request(
                channel,
                ExecuteRequest {
                    code,
                    language: Language::Shell as i32,
                    network_config: network,
                    secrets: vec![SecretConfig {
                        name: "TOKEN".to_string(),
                        value: REAL.to_string(),
                        allowed_hosts: vec!["127.0.0.1".to_string()],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            format!("{}{}", result.stdout.to_lossy(), result.stderr.to_lossy())
        }
    };
    let run = |network| {
        run_script(
            format!("tc env TOKEN; tc proxy-get http://127.0.0.1:{port}/x"),
            network,
        )
    };

    let inherit = run(Some(with_children(
        allowing("127.0.0.1"),
        Mode::Inherit,
        None,
    )))
    .await;
    assert!(inherit.contains(" 200 OK"), "{inherit}");
    // The placeholder, scrubbed from the output; it went out as it is.
    assert!(inherit.starts_with("[REDACTED]\n"), "{inherit}");
    let got = seen.lock().unwrap().clone();
    assert!(
        got.contains("Bearer ERYX_SECRET_PLACEHOLDER_") && !got.contains(REAL),
        "{got}"
    );

    let narrow = |parent: &str, child: &str| {
        Some(with_children(
            allowing(parent),
            Mode::Narrow,
            Some(allowing(child)),
        ))
    };
    let out = run(narrow("127.0.0.1", "127.0.0.1")).await;
    assert!(out.contains(" 200 OK"), "{out}");

    // Neither mode reaches past the request's own policy.
    let requests = seen.lock().unwrap().matches("GET ").count();
    for network in [
        with_children(allowing("other.test"), Mode::Inherit, None),
        narrow("other.test", "127.0.0.1").unwrap(),
        narrow("127.0.0.1", "other.test").unwrap(),
    ] {
        let out = run(Some(network)).await;
        assert!(out.contains(" 403 "), "{out}");
    }
    // Without child_network, children have no proxy at all.
    let out = run(Some(allowing("127.0.0.1"))).await;
    assert!(!out.contains(" 200 OK") && out.contains("error:"), "{out}");
    assert_eq!(seen.lock().unwrap().matches("GET ").count(), requests);

    // A command allowed tunnels gets a blind one, and its placeholder goes
    // out as it is; one without gets an inspecting proxy, which expects TLS.
    let inherit = || Some(with_children(allowing("127.0.0.1"), Mode::Inherit, None));
    let out = run_script(format!("tt tunnel-get 127.0.0.1:{port}"), inherit()).await;
    assert!(
        out.contains(" 200 Connection established") && out.contains(" 200 OK"),
        "{out}"
    );
    let got = seen.lock().unwrap().clone();
    assert_eq!(got.matches("GET ").count(), requests + 1, "{got}");
    assert!(!got.contains(REAL), "{got}");
    // It handshakes with the (plain) upstream first: give up on that soon.
    let short = NetworkConfig {
        connect_timeout_ms: 500,
        io_timeout_ms: 500,
        ..allowing("127.0.0.1")
    };
    let out = run_script(
        format!("tc tunnel-get 127.0.0.1:{port}"),
        Some(with_children(short, Mode::Inherit, None)),
    )
    .await;
    assert!(!out.contains(" 200 OK"), "{out}");
    assert_eq!(seen.lock().unwrap().matches("GET ").count(), requests + 1);

    // Only the shell has children.
    let status = execute_request(
        channel.clone(),
        ExecuteRequest {
            code: "pass".to_string(),
            network_config: inherit(),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("only for shell"), "{status:?}");
}

/// A malformed `child_network` is refused before anything runs.
#[tokio::test]
async fn invalid_child_networks_are_rejected() {
    use eryx_server::proto::eryx::v1::child_network::Mode;
    use eryx_server::proto::eryx::v1::{ChildNetwork, NetworkConfig};

    let channel = start_server().await;
    let child = |mode: Mode, narrow: Option<NetworkConfig>| NetworkConfig {
        child_network: Some(Box::new(ChildNetwork {
            mode: mode as i32,
            narrow: narrow.map(Box::new),
        })),
        ..Default::default()
    };
    for (network, message) in [
        (child(Mode::Narrow, None), "MODE_NARROW needs narrow"),
        (
            child(Mode::Inherit, Some(NetworkConfig::default())),
            "narrow is only for MODE_NARROW",
        ),
        (
            child(Mode::Narrow, Some(child(Mode::Inherit, None))),
            "narrow can't set child_network",
        ),
    ] {
        let status = execute_request(
            channel.clone(),
            ExecuteRequest {
                code: "pass".to_string(),
                network_config: Some(network),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains(message), "{status:?}");
    }
}

/// `network_config.header_injections` add their headers to the guest's
/// requests to their hosts, together, replacing the guest's own copies; the
/// guest never sees the values (masked out of the echo, scrubbed from its
/// output). Over plain HTTP (a local server, as TLS here would need a root
/// the request can't give) they go only with `allow_plaintext_secrets`:
/// without it the request is refused, never sent without them.
#[tokio::test]
async fn header_injections_reach_the_guests_requests() {
    use eryx_server::proto::eryx::v1::{HeaderInjection, HttpHeader, HttpRule, NetworkConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const ACCESS: &str = "grpc-access-token-0123456789";
    const ID: &str = "grpc-id-token-9876543210";
    const EVERY: &str = "grpc-every-request-55555";
    let limits = eryx_server::service::ServerLimits {
        allow_plaintext_secrets: true,
        ..Default::default()
    };
    let mut client = EryxClient::new(
        start_server_with(2, |pool| EryxService::new(pool).with_limits(limits)).await,
    );

    for allow_plaintext_secrets in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Echo each request head, close-delimited (masking changes its length).
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let server = tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut head = Vec::new();
                let mut buf = [0; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                    .await;
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into_owned());
                let _ = sock.write_all(&head).await;
            }
        });
        let code = format!(
            "import urllib.request\n\
             def get(path):\n\
             \x20   req = urllib.request.Request(f'http://127.0.0.1:{port}{{path}}', headers={{'Authorization': 'Bearer guest', 'X-Grafana-Id': 'guest-identity'}})\n\
             \x20   try:\n\
             \x20       return urllib.request.urlopen(req, timeout=10).read().decode()\n\
             \x20   except Exception as e:\n\
             \x20       return f'error: {{e}}'\n\
             print('api:', get('/api/x'))\n\
             print('public:', get('/public'))\n\
             print('leak: {ACCESS} {ID}')\n"
        );
        let result = execute_half_closed(
            &mut client,
            ExecuteRequest {
                code,
                network_config: Some(NetworkConfig {
                    allowed_hosts: vec!["127.0.0.1".to_string()],
                    blocked_hosts: vec!["blocked.invalid".to_string()],
                    allowed_networks: vec!["127.0.0.0/8".to_string()],
                    allow_plaintext_secrets,
                    header_injections: vec![
                        HeaderInjection {
                            headers: vec![
                                HttpHeader {
                                    name: "Authorization".to_string(),
                                    value: format!("Bearer {ACCESS}"),
                                },
                                HttpHeader {
                                    name: "X-Id-Token".to_string(),
                                    value: ID.to_string(),
                                },
                            ],
                            strip_headers: vec!["X-Grafana-Id".to_string()],
                            http_rules: vec![HttpRule {
                                host: format!("127.0.0.1:{port}"),
                                methods: vec!["GET".to_string()],
                                paths: vec!["/api".to_string()],
                                ..Default::default()
                            }],
                        },
                        // A rule naming only a host covers every request to it.
                        HeaderInjection {
                            headers: vec![HttpHeader {
                                name: "X-Every".to_string(),
                                value: EVERY.to_string(),
                            }],
                            http_rules: vec![HttpRule {
                                host: format!("127.0.0.1:{port}"),
                                ..Default::default()
                            }],
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.success, "{}", result.error);
        let stdout = result.stdout.to_lossy();
        assert!(!stdout.contains(ACCESS) && !stdout.contains(ID), "{stdout}");
        assert!(stdout.contains("leak: [REDACTED] [REDACTED]"), "{stdout}");

        server.abort();
        let received = seen.lock().unwrap().clone();
        let api: Vec<_> = received
            .iter()
            .filter(|r| r.starts_with("GET /api/x "))
            .collect();
        let public: Vec<_> = received
            .iter()
            .filter(|r| r.starts_with("GET /public "))
            .collect();
        if allow_plaintext_secrets {
            // Outside the first injection's rules: the guest's own headers
            // stay, and only the host-wide injection is added.
            assert!(
                public.len() == 1
                    && public[0].contains("Bearer guest")
                    && public[0].contains("guest-identity")
                    && !public[0].contains(ID),
                "{received:?}"
            );
            assert!(
                public[0].contains(&format!("X-Every: {EVERY}\r\n")),
                "{}",
                public[0]
            );
            assert_eq!(api.len(), 1, "{received:?}");
            assert!(
                api[0].contains(&format!("X-Every: {EVERY}\r\n")),
                "{}",
                api[0]
            );
            assert!(
                api[0].contains(&format!("Authorization: Bearer {ACCESS}\r\n"))
                    && api[0].contains(&format!("X-Id-Token: {ID}\r\n"))
                    && !api[0].contains("guest")
                    && !api[0].to_ascii_lowercase().contains("x-grafana-id"),
                "{}",
                api[0]
            );
            assert!(
                stdout.contains("Authorization: [REDACTED]")
                    && stdout.contains("X-Id-Token: [REDACTED]"),
                "{stdout}"
            );
        } else {
            // The host-wide injection covers /public too, so over plain HTTP
            // both are refused: no request head reaches the server.
            assert!(api.is_empty() && public.is_empty(), "{received:?}");
            assert!(received.iter().all(String::is_empty), "{received:?}");
            assert!(
                stdout.contains("api: error:")
                    && stdout.contains("public: error:")
                    && stdout.contains("only sent over TLS"),
                "{stdout}"
            );
        }
    }
}

/// A header injection that can't be sent safely is INVALID_ARGUMENT, naming
/// it but never its values, as is one in a narrow child network.
#[tokio::test]
async fn invalid_header_injections_are_rejected() {
    use eryx_server::proto::eryx::v1::{
        ChildNetwork, HeaderInjection, HttpHeader, HttpRule, NetworkConfig, child_network::Mode,
    };

    let mut client = EryxClient::new(start_server().await);
    let header = |name: &str, value: &str| HttpHeader {
        name: name.to_string(),
        value: value.to_string(),
    };
    let injection = |headers, hosts: &[&str]| HeaderInjection {
        headers,
        http_rules: hosts
            .iter()
            .map(|h| HttpRule {
                host: (*h).to_string(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let ok = || {
        injection(
            vec![header("X-Key", "secret-value-1")],
            &["api.example.com"],
        )
    };
    let request = |network_config| ExecuteRequest {
        code: "pass".to_string(),
        network_config: Some(network_config),
        ..Default::default()
    };
    let with = |injections| NetworkConfig {
        allow_all_hosts: true,
        header_injections: injections,
        ..Default::default()
    };
    for (network_config, why) in [
        (
            with(vec![injection(
                vec![header("Host", "secret-value-1")],
                &["api.example.com"],
            )]),
            "Host",
        ),
        (
            with(vec![injection(
                vec![header("X-Key", "secret-value-1\r\nX-Evil: 1")],
                &["api.example.com"],
            )]),
            "control character",
        ),
        (
            with(vec![injection(
                vec![
                    header("X-Key", "secret-value-1"),
                    header("x-key", "secret-value-1"),
                ],
                &["api.example.com"],
            )]),
            "twice",
        ),
        (
            with(vec![injection(
                vec![header("X-Key", "secret-value-1")],
                &[],
            )]),
            "http_rules: at least one rule",
        ),
        (
            with(vec![injection(
                vec![header("X-Key", "secret-value-1")],
                &["*:443"],
            )]),
            "host pattern",
        ),
        (
            with(vec![injection(
                vec![header("X-Key", "secret-value-1")],
                &["api.example.com", "*.com"],
            )]),
            "host pattern",
        ),
        (
            with(vec![injection(
                vec![header("X-Key", "secret-value-1")],
                &["*"],
            )]),
            "host pattern",
        ),
        (
            with(vec![injection(
                vec![header("X-Key", "secret-value-1")],
                &["api.example.com:0"],
            )]),
            "host pattern",
        ),
        (
            with(vec![injection(vec![], &["api.example.com"])]),
            "no headers",
        ),
        (
            with(vec![
                ok(),
                injection(vec![header("X-Key", "")], &["api.example.com"]),
            ]),
            "header_injections[1]",
        ),
        (
            NetworkConfig {
                allow_all_hosts: true,
                child_network: Some(Box::new(ChildNetwork {
                    mode: Mode::Narrow as i32,
                    narrow: Some(Box::new(with(vec![ok()]))),
                })),
                ..Default::default()
            },
            "header_injections",
        ),
    ] {
        let status = execute_half_closed(&mut client, request(network_config))
            .await
            .unwrap_err();
        assert_eq!(
            status.code(),
            tonic::Code::InvalidArgument,
            "{why}: {status:?}"
        );
        assert!(status.message().contains(why), "{why}: {status:?}");
        assert!(!status.message().contains("secret-value-1"), "{status:?}");
    }
}

/// A shell request's header injections go to its commands too, each of
/// which then gets a proxy of its own, whose password is scrubbed from the
/// output like the values.
#[cfg(feature = "shell")]
#[tokio::test]
async fn header_injections_reach_shell_commands_with_scrubbed_proxies() {
    use eryx_server::proto::eryx::v1::{
        ChildNetwork, HeaderInjection, HttpHeader, HttpRule, Language, NetworkConfig,
        child_network::Mode,
    };

    let create: eryx_server::service::PoolFactory = Arc::new(|| {
        Box::pin(async {
            let config = PoolConfig::default().with_max_size(1);
            Ok(Arc::new(
                SandboxPool::new(Sandbox::embedded().shell(), config).await?,
            ))
        })
    });
    let channel = start_server_with(2, |pool| {
        EryxService::new(pool).with_lazy_pool(eryx::Language::Shell, create)
    })
    .await;
    let result = execute_request(
        channel,
        ExecuteRequest {
            code: "printenv HTTPS_PROXY; echo shell-secret-value-1".to_string(),
            language: Language::Shell as i32,
            network_config: Some(NetworkConfig {
                allowed_hosts: vec!["api.example.com".to_string()],
                child_network: Some(Box::new(ChildNetwork {
                    mode: Mode::Inherit as i32,
                    narrow: None,
                })),
                header_injections: vec![HeaderInjection {
                    headers: vec![HttpHeader {
                        name: "X-Key".to_string(),
                        value: "shell-secret-value-1".to_string(),
                    }],
                    http_rules: vec![HttpRule {
                        host: "api.example.com".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(result.success, "{result:?}");
    let stdout = result.stdout.to_lossy();
    assert!(
        stdout.starts_with("http://[REDACTED]@127.0.0.1:") && stdout.ends_with("\n[REDACTED]\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("eryx:"), "{stdout}");
}
