//! gRPC-backed trace handler for streaming execution trace events.

use async_trait::async_trait;
use eryx::{TraceEvent, TraceEventKind, TraceHandler};
use tokio::sync::mpsc;

use crate::proto::eryx::v1::{self, ServerMessage, server_message};

/// A [`TraceHandler`] that streams trace events over a gRPC response channel.
#[derive(Debug)]
pub struct GrpcTraceHandler {
    tx: mpsc::Sender<ServerMessage>,
}

impl GrpcTraceHandler {
    /// Create a new handler that sends trace events over the given channel.
    pub fn new(tx: mpsc::Sender<ServerMessage>) -> Self {
        Self { tx }
    }
}

/// Convert an eryx `TraceEvent` into a proto `TraceEvent`.
fn to_proto_trace_event(event: &TraceEvent) -> v1::TraceEvent {
    let (event_type, function, message, name, duration_ms) = match &event.event {
        TraceEventKind::Line => ("line", String::new(), String::new(), String::new(), 0),
        TraceEventKind::Call { function } => {
            ("call", function.clone(), String::new(), String::new(), 0)
        }
        TraceEventKind::Return { function } => {
            ("return", function.clone(), String::new(), String::new(), 0)
        }
        TraceEventKind::Exception { message } => (
            "exception",
            String::new(),
            message.clone(),
            String::new(),
            0,
        ),
        TraceEventKind::CallbackStart { name } => (
            "callback_start",
            String::new(),
            String::new(),
            name.clone(),
            0,
        ),
        TraceEventKind::CallbackEnd { name, duration_ms } => (
            "callback_end",
            String::new(),
            String::new(),
            name.clone(),
            *duration_ms,
        ),
    };

    v1::TraceEvent {
        lineno: event.lineno,
        event_type: event_type.to_string(),
        function,
        message,
        name,
        duration_ms,
        context_json: event
            .context
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default(),
    }
}

#[async_trait]
impl TraceHandler for GrpcTraceHandler {
    async fn on_trace(&self, event: TraceEvent) {
        let proto_event = to_proto_trace_event(&event);
        let msg = ServerMessage {
            message: Some(server_message::Message::TraceEvent(proto_event)),
        };
        // Best-effort: if the channel is closed, the event is lost.
        let _ = self.tx.send(msg).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sends_line_trace_event() {
        let (tx, mut rx) = mpsc::channel(16);
        let handler = GrpcTraceHandler::new(tx);

        handler
            .on_trace(TraceEvent {
                lineno: 42,
                event: TraceEventKind::Line,
                context: None,
            })
            .await;

        let msg = rx.recv().await.unwrap();
        if let Some(server_message::Message::TraceEvent(event)) = msg.message {
            assert_eq!(event.lineno, 42);
            assert_eq!(event.event_type, "line");
        } else {
            panic!("expected TraceEvent");
        }
    }

    #[tokio::test]
    async fn sends_call_trace_event() {
        let (tx, mut rx) = mpsc::channel(16);
        let handler = GrpcTraceHandler::new(tx);

        handler
            .on_trace(TraceEvent {
                lineno: 10,
                event: TraceEventKind::Call {
                    function: "my_func".to_string(),
                },
                context: None,
            })
            .await;

        let msg = rx.recv().await.unwrap();
        if let Some(server_message::Message::TraceEvent(event)) = msg.message {
            assert_eq!(event.event_type, "call");
            assert_eq!(event.function, "my_func");
        } else {
            panic!("expected TraceEvent");
        }
    }

    #[tokio::test]
    async fn sends_callback_end_trace_event() {
        let (tx, mut rx) = mpsc::channel(16);
        let handler = GrpcTraceHandler::new(tx);

        handler
            .on_trace(TraceEvent {
                lineno: 0,
                event: TraceEventKind::CallbackEnd {
                    name: "query_loki".to_string(),
                    duration_ms: 150,
                },
                context: None,
            })
            .await;

        let msg = rx.recv().await.unwrap();
        if let Some(server_message::Message::TraceEvent(event)) = msg.message {
            assert_eq!(event.event_type, "callback_end");
            assert_eq!(event.name, "query_loki");
            assert_eq!(event.duration_ms, 150);
        } else {
            panic!("expected TraceEvent");
        }
    }

    #[tokio::test]
    async fn includes_context_json() {
        let (tx, mut rx) = mpsc::channel(16);
        let handler = GrpcTraceHandler::new(tx);

        handler
            .on_trace(TraceEvent {
                lineno: 5,
                event: TraceEventKind::Line,
                context: Some(serde_json::json!({"x": 42})),
            })
            .await;

        let msg = rx.recv().await.unwrap();
        if let Some(server_message::Message::TraceEvent(event)) = msg.message {
            assert!(event.context_json.contains("42"));
        } else {
            panic!("expected TraceEvent");
        }
    }
}
