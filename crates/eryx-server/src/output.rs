//! gRPC-backed output handler for streaming sandbox output.

use async_trait::async_trait;
use eryx::OutputHandler;
use tokio::sync::mpsc;

use crate::proto::eryx::v1::{OutputEvent, OutputStream, ServerMessage, server_message};

/// An [`OutputHandler`] that streams stdout/stderr over a gRPC response channel.
#[derive(Debug)]
pub struct GrpcOutputHandler {
    tx: mpsc::Sender<ServerMessage>,
}

impl GrpcOutputHandler {
    /// Create a new handler that sends output events over the given channel.
    pub fn new(tx: mpsc::Sender<ServerMessage>) -> Self {
        Self { tx }
    }

    async fn send_output(&self, stream: OutputStream, data: &str) {
        tracing::trace!(stream = ?stream, data_len = data.len(), "sending output");
        let msg = ServerMessage {
            message: Some(server_message::Message::OutputEvent(OutputEvent {
                stream: stream.into(),
                data: data.to_string(),
            })),
        };
        // Best-effort: if the channel is closed, the output is lost.
        let _ = self.tx.send(msg).await;
    }
}

#[async_trait]
impl OutputHandler for GrpcOutputHandler {
    async fn on_output(&self, chunk: &str) {
        self.send_output(OutputStream::Stdout, chunk).await;
    }

    async fn on_stderr(&self, chunk: &str) {
        self.send_output(OutputStream::Stderr, chunk).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sends_stdout_event() {
        let (tx, mut rx) = mpsc::channel(16);
        let handler = GrpcOutputHandler::new(tx);

        handler.on_output("hello").await;

        let msg = rx.recv().await.unwrap();
        if let Some(server_message::Message::OutputEvent(event)) = msg.message {
            assert_eq!(event.stream, i32::from(OutputStream::Stdout));
            assert_eq!(event.data, "hello");
        } else {
            panic!("expected OutputEvent");
        }
    }

    #[tokio::test]
    async fn sends_stderr_event() {
        let (tx, mut rx) = mpsc::channel(16);
        let handler = GrpcOutputHandler::new(tx);

        handler.on_stderr("error").await;

        let msg = rx.recv().await.unwrap();
        if let Some(server_message::Message::OutputEvent(event)) = msg.message {
            assert_eq!(event.stream, i32::from(OutputStream::Stderr));
            assert_eq!(event.data, "error");
        } else {
            panic!("expected OutputEvent");
        }
    }
}
