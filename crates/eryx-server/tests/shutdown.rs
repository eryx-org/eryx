//! Exercise signal handling in the production server binary with an active RPC.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{SocketAddr, TcpListener};
use std::process::Stdio;
use std::time::Duration;

use eryx_server::proto::eryx::v1::eryx_client::EryxClient;
use eryx_server::proto::eryx::v1::{
    CallbackDeclaration, CallbackOutcome, CallbackResponse, ClientMessage, ExecuteRequest,
    ResourceLimits, callback_response, client_message, server_message,
};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;

async fn connect(child: &mut Child, addr: SocketAddr) -> Channel {
    timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "server exited at startup"
            );
            if let Ok(channel) = Channel::from_shared(format!("http://{addr}"))
                .unwrap()
                .connect()
                .await
            {
                return channel;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("server did not start listening")
}

async fn drain_active_call(signal: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut child = Command::new(env!("CARGO_BIN_EXE_eryx-server"))
        .args([
            "--listen-addr",
            &addr.to_string(),
            "--metrics-addr",
            "127.0.0.1:0",
            "--pool-max-size",
            "1",
            "--pool-min-idle",
            "1",
        ])
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let channel = connect(&mut child, addr).await;
    let mut health = HealthClient::new(channel.clone());
    for service in ["", "eryx.v1.Eryx"] {
        let status = health
            .check(HealthCheckRequest {
                service: service.into(),
            })
            .await
            .unwrap()
            .into_inner()
            .status;
        assert_eq!(status, ServingStatus::Serving as i32, "{service:?}");
    }
    let mut client = EryxClient::new(channel);
    let (tx, rx) = mpsc::channel(4);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "print(await wait_for_release())".into(),
                callbacks: vec![CallbackDeclaration {
                    name: "wait_for_release".into(),
                    description: "Wait for the test to release execution".into(),
                    ..Default::default()
                }],
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
    let callback = timeout(Duration::from_secs(10), async {
        loop {
            let message = stream
                .message()
                .await
                .unwrap()
                .expect("stream ended before callback");
            if let Some(server_message::Message::CallbackRequest(callback)) = message.message {
                return callback;
            }
        }
    })
    .await
    .expect("execution did not reach the callback");

    assert!(
        Command::new("kill")
            .args([signal, &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    timeout(Duration::from_secs(5), async {
        while TcpStream::connect(addr).await.is_ok() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("draining server still accepts new connections");
    assert!(
        child.try_wait().unwrap().is_none(),
        "{signal} terminated the server before its active call finished"
    );

    tx.send(ClientMessage {
        message: Some(client_message::Message::CallbackResponse(
            CallbackResponse {
                request_id: callback.request_id,
                result: Some(callback_response::Result::JsonResult("\"released\"".into())),
                outcome: CallbackOutcome::Ok.into(),
            },
        )),
    })
    .await
    .unwrap();
    let result = timeout(Duration::from_secs(10), async {
        loop {
            let message = stream
                .message()
                .await
                .unwrap()
                .expect("stream ended before result");
            if let Some(server_message::Message::ExecuteResult(result)) = message.message {
                return result;
            }
        }
    })
    .await
    .expect("active call did not finish during shutdown");
    assert!(result.success, "execution failed: {}", result.error);
    assert_eq!(result.stdout, b"released\n");

    drop(stream);
    drop(tx);
    drop(client);
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("server did not exit after draining")
        .unwrap();
    assert!(status.success(), "server exited unsuccessfully: {status}");
}

#[tokio::test]
async fn sigterm_drains_active_call() {
    drain_active_call("-TERM").await;
}

#[tokio::test]
async fn sigint_drains_active_call() {
    drain_active_call("-INT").await;
}
