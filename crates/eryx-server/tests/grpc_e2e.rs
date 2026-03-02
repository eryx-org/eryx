//! End-to-end gRPC tests for the eryx server.
//!
//! These tests start a tonic server in-process, connect a client, and verify
//! the full bidirectional streaming flow: execute request → callback round-trip
//! → output streaming → trace events → final result.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use eryx::{PoolConfig, Sandbox, SandboxPool};
use eryx_server::proto::eryx::v1::eryx_client::EryxClient;
use eryx_server::proto::eryx::v1::eryx_server::EryxServer;
use eryx_server::proto::eryx::v1::{
    CallbackDeclaration, ClientMessage, ExecuteRequest, ParameterDeclaration, ResourceLimits,
    callback_response, client_message, server_message,
};
use eryx_server::service::EryxService;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

/// Start an in-process gRPC server on a random port and return the channel.
async fn start_server() -> Channel {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let pool_config = PoolConfig {
        max_size: 4,
        min_idle: 1,
        ..Default::default()
    };

    let pool = SandboxPool::new(Sandbox::embedded(), pool_config)
        .await
        .expect("failed to create sandbox pool");
    let pool = Arc::new(pool);

    tokio::spawn(async move {
        Server::builder()
            .add_service(EryxServer::new(EryxService::new(pool)))
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
        message: Some(client_message::Message::ExecuteRequest(ExecuteRequest {
            code: "print('hello from eryx')".to_string(),
            callbacks: vec![],
            resource_limits: Some(ResourceLimits {
                execution_timeout_ms: 30_000,
                ..Default::default()
            }),
            enable_tracing: false,
        })),
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
                result.stdout.contains("hello from eryx"),
                "stdout missing expected output: {:?}",
                result.stdout
            );
            got_result = true;
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

#[tokio::test]
async fn execute_with_echo_callback() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    // Send the execute request with an echo callback declaration.
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(ExecuteRequest {
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
        })),
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
                    result.stdout.contains("got:"),
                    "stdout missing callback result: {:?}",
                    result.stdout
                );
                assert!(
                    result.stdout.contains("echoed"),
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

#[tokio::test]
async fn execute_with_callback_error() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(ExecuteRequest {
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
        })),
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
                    result.stdout.contains("caught:"),
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
        message: Some(client_message::Message::ExecuteRequest(ExecuteRequest {
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
        })),
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
                assert!(result.stdout.contains("line 0"));
                assert!(result.stdout.contains("line 2"));
                got_result = true;
            }
            _ => {}
        }
    }

    assert!(got_result, "never received ExecuteResult");
    // We should have received at least one output event.
    assert!(!output_events.is_empty(), "expected output events");
    let all_output: String = output_events.concat();
    assert!(
        all_output.contains("line 0"),
        "output events missing expected content: {:?}",
        all_output
    );
}

#[tokio::test]
async fn execute_reports_stats() {
    let channel = start_server().await;
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);

    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(ExecuteRequest {
            code: "x = 1 + 1".to_string(),
            callbacks: vec![],
            resource_limits: None,
            enable_tracing: false,
        })),
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
        message: Some(client_message::Message::ExecuteRequest(ExecuteRequest {
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
        })),
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
                assert!(
                    result.stdout.contains("3"),
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
        message: Some(client_message::Message::ExecuteRequest(ExecuteRequest {
            code: "x = 1 + 1".to_string(),
            callbacks: vec![],
            resource_limits: Some(ResourceLimits {
                execution_timeout_ms: 30_000,
                ..Default::default()
            }),
            enable_tracing: false,
        })),
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
        match msg.message {
            Some(server_message::Message::TraceEvent(event)) => {
                trace_events.push(event);
            }
            _ => {}
        }
    }

    assert!(
        trace_events.is_empty(),
        "should not receive trace events when enable_tracing is false, got {}",
        trace_events.len()
    );
}
