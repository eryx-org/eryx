//! Execution tracing types and handlers.
//!
//! This module provides types for tracking Python code execution and streaming output.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// An execution trace event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    /// Line number in the source code.
    pub lineno: u32,
    /// The kind of trace event.
    pub event: TraceEventKind,
    /// Optional context data (e.g., locals snapshot).
    pub context: Option<serde_json::Value>,
}

/// The kind of trace event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceEventKind {
    /// About to execute a line (from `sys.settrace` 'line' event).
    Line,
    /// Function call (from `sys.settrace` 'call' event).
    Call {
        /// Name of the function being called.
        function: String,
    },
    /// Function return (from `sys.settrace` 'return' event).
    Return {
        /// Name of the function returning.
        function: String,
    },
    /// Exception raised (from `sys.settrace` 'exception' event).
    Exception {
        /// Exception message.
        message: String,
    },
    /// Callback invocation started (emitted when a callback is called).
    CallbackStart {
        /// Name of the callback being invoked.
        name: String,
    },
    /// Callback invocation completed (emitted when a callback returns).
    CallbackEnd {
        /// Name of the callback that completed.
        name: String,
        /// Duration of the callback in milliseconds.
        duration_ms: u64,
    },
}

/// Handler for trace events during execution.
///
/// Receives events from Python's `sys.settrace` plus callback start/end events.
/// Useful for UI visualization of script execution progress.
#[async_trait]
pub trait TraceHandler: Send + Sync {
    /// Called when a trace event occurs.
    async fn on_trace(&self, event: TraceEvent);
}

/// Handler for streaming stdout during execution.
#[async_trait]
pub trait OutputHandler: Send + Sync {
    /// Called when output is produced.
    async fn on_output(&self, chunk: &str);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_event_serializes_correctly() {
        let event = TraceEvent {
            lineno: 42,
            event: TraceEventKind::Line,
            context: None,
        };

        let json = serde_json::to_string(&event).unwrap_or_default();
        assert!(json.contains("42"));
        assert!(json.contains("line"));
    }

    #[test]
    fn trace_event_kind_variants_serialize() {
        let call = TraceEventKind::Call {
            function: "test_fn".to_string(),
        };
        let json = serde_json::to_string(&call).unwrap_or_default();
        assert!(json.contains("call"));
        assert!(json.contains("test_fn"));

        let callback_end = TraceEventKind::CallbackEnd {
            name: "http.get".to_string(),
            duration_ms: 150,
        };
        let json = serde_json::to_string(&callback_end).unwrap_or_default();
        assert!(json.contains("callback_end"));
        assert!(json.contains("150"));
    }
}
