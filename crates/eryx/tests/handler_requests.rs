//! Callback and output requests can be built outside eryx, so custom
//! handlers can be tested without running a sandbox.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::Arc;

use eryx::callback_handler::run_callback_handler;
use eryx::{CallbackHostResult, CallbackRequest, OutputRequest, ResourceLimits};
use tokio::sync::{mpsc, oneshot};

#[tokio::test]
async fn callback_handler_answers_a_hand_built_request() {
    let (tx, rx) = mpsc::channel(1);
    let (response_tx, response_rx) = oneshot::channel();
    tx.send(CallbackRequest::new("missing", "{}", response_tx))
        .await
        .unwrap();
    drop(tx);

    run_callback_handler(
        rx,
        Arc::new(HashMap::new()),
        ResourceLimits::default(),
        Arc::new(HashMap::new()),
    )
    .await;

    match response_rx.await.unwrap() {
        CallbackHostResult::Err(message) => assert!(message.contains("missing")),
        other => panic!("expected an error for an unknown callback, got {other:?}"),
    }
}

#[test]
fn output_request_keeps_its_stream_and_bytes() {
    let request = OutputRequest::new(1, b"oops".to_vec());
    assert_eq!(request.stream, 1);
    assert_eq!(request.data, b"oops");
}
