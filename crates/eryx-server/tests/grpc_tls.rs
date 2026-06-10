//! TLS / mTLS end-to-end tests for the eryx gRPC server.
//!
//! These start an in-process tonic server wrapped with `ServerTlsConfig` and
//! connect a client over a TLS channel, verifying:
//!   - a plain server-TLS handshake + execute round-trip,
//!   - a mutual-TLS handshake where the client presents a CA-signed cert,
//!   - that the server rejects a client with no certificate once mTLS is on.
//!
//! Certificates are generated in-process with `rcgen` (a CA that signs both a
//! server cert for `localhost`/`127.0.0.1` and a client cert), so nothing is
//! committed to the repo and the certs never expire mid-test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use eryx::{PoolConfig, Sandbox, SandboxPool};
use eryx_server::proto::eryx::v1::eryx_client::EryxClient;
use eryx_server::proto::eryx::v1::eryx_server::EryxServer;
use eryx_server::proto::eryx::v1::{
    ClientMessage, ExecuteRequest, ResourceLimits, client_message, server_message,
};
use eryx_server::service::EryxService;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::server::ServerTlsConfig;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server};

/// PEM material for a CA plus a server and client certificate it signed.
struct TestCerts {
    ca_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
}

/// Generate a throwaway CA and CA-signed server + client certificates.
fn generate_certs() -> TestCerts {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();

    let server_key = KeyPair::generate().unwrap();
    let server_cert =
        CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .unwrap()
            .signed_by(&server_key, &ca_cert, &ca_key)
            .unwrap();

    let client_key = KeyPair::generate().unwrap();
    let client_cert = CertificateParams::new(vec!["eryx-test-client".to_string()])
        .unwrap()
        .signed_by(&client_key, &ca_cert, &ca_key)
        .unwrap();

    TestCerts {
        ca_pem: ca_cert.pem(),
        server_cert_pem: server_cert.pem(),
        server_key_pem: server_key.serialize_pem(),
        client_cert_pem: client_cert.pem(),
        client_key_pem: client_key.serialize_pem(),
    }
}

/// Start an in-process gRPC server on a random port with the given TLS config
/// and return its address.
async fn start_tls_server(tls: ServerTlsConfig) -> std::net::SocketAddr {
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
            .tls_config(tls)
            .expect("invalid server TLS config")
            .add_service(EryxServer::new(EryxService::new(pool)))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    // Give the server a moment to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// Connect a TLS channel to `addr` using the supplied client TLS config.
async fn connect(
    addr: std::net::SocketAddr,
    tls: ClientTlsConfig,
) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    Endpoint::from_shared(format!("https://{addr}"))?
        .tls_config(tls)?
        .connect()
        .await
}

/// Run `print('hello over tls')` over the channel and assert it round-trips.
async fn assert_hello_round_trip(channel: tonic::transport::Channel) {
    let mut client = EryxClient::new(channel);

    let (tx, rx) = mpsc::channel(16);
    tx.send(ClientMessage {
        message: Some(client_message::Message::ExecuteRequest(Box::new(
            ExecuteRequest {
                code: "print('hello over tls')".to_string(),
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
            assert!(
                result.stdout.contains("hello over tls"),
                "stdout missing expected output: {:?}",
                result.stdout
            );
            got_result = true;
        }
    }
    assert!(got_result, "never received ExecuteResult");
}

/// Server-side TLS only: the client trusts the CA and completes a round-trip.
#[tokio::test]
async fn tls_execute_round_trip() {
    let certs = generate_certs();

    let server_tls = ServerTlsConfig::new().identity(Identity::from_pem(
        certs.server_cert_pem.clone(),
        certs.server_key_pem.clone(),
    ));
    let addr = start_tls_server(server_tls).await;

    let client_tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(certs.ca_pem.clone()));
    let channel = connect(addr, client_tls).await.expect("TLS connect failed");

    assert_hello_round_trip(channel).await;
}

/// Mutual TLS: the server requires a client cert signed by its CA, and the
/// client presents one, so the round-trip succeeds.
#[tokio::test]
async fn mtls_execute_round_trip() {
    let certs = generate_certs();

    let server_tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(
            certs.server_cert_pem.clone(),
            certs.server_key_pem.clone(),
        ))
        .client_ca_root(Certificate::from_pem(certs.ca_pem.clone()));
    let addr = start_tls_server(server_tls).await;

    let client_tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(certs.ca_pem.clone()))
        .identity(Identity::from_pem(
            certs.client_cert_pem.clone(),
            certs.client_key_pem.clone(),
        ));
    let channel = connect(addr, client_tls)
        .await
        .expect("mTLS connect failed");

    assert_hello_round_trip(channel).await;
}

/// Mutual TLS rejects a client that presents no certificate: the handshake
/// must fail rather than the request being served.
#[tokio::test]
async fn mtls_rejects_client_without_cert() {
    let certs = generate_certs();

    let server_tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(
            certs.server_cert_pem.clone(),
            certs.server_key_pem.clone(),
        ))
        .client_ca_root(Certificate::from_pem(certs.ca_pem.clone()));
    let addr = start_tls_server(server_tls).await;

    // Trust the server CA but present no client identity.
    let client_tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(certs.ca_pem.clone()));

    // The handshake may fail at connect time, or the connection may be torn
    // down on the first request — either way the round-trip must not succeed.
    let outcome: Result<(), Box<dyn std::error::Error>> = async {
        let channel = connect(addr, client_tls).await?;
        let mut client = EryxClient::new(channel);
        let (tx, rx) = mpsc::channel(16);
        tx.send(ClientMessage {
            message: Some(client_message::Message::ExecuteRequest(Box::new(
                ExecuteRequest {
                    code: "print('should not run')".to_string(),
                    resource_limits: Some(ResourceLimits {
                        execution_timeout_ms: 30_000,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ))),
        })
        .await?;
        let mut stream = client.execute(ReceiverStream::new(rx)).await?.into_inner();
        while stream.message().await?.is_some() {}
        Ok(())
    }
    .await;

    assert!(
        outcome.is_err(),
        "client without a certificate should be rejected under mTLS, but the round-trip succeeded"
    );
}
