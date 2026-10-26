//! The TLS server name must be the host the socket connected to.
#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used)]

use eryx::{NetConfig, Sandbox};
use tokio::net::TcpListener;

#[tokio::test]
async fn tls_hostname_must_match_connect_host() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let sandbox = Sandbox::embedded()
        .with_network(NetConfig::permissive())
        .build()
        .unwrap();
    let out = sandbox
        .execute(&format!(
            "import socket, ssl\n\
             s = socket.create_connection(('127.0.0.1', {port}))\n\
             try:\n    ssl.create_default_context().wrap_socket(s, server_hostname='localhost')\n\
             except PermissionError as e:\n    print('does not match' in str(e))"
        ))
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "True\n");
}
