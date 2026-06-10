//! gRPC server for sandboxed Python execution via eryx.
//!
//! This crate provides a [tonic] gRPC server that wraps the [`eryx`] sandbox,
//! enabling remote Python execution with bidirectional callback streaming.
//! It is designed as the backend for the Grafana Assistant's `execute_python` tool.

#![deny(unsafe_code)]

pub mod callbacks;
pub mod output;
pub mod replay;
pub mod service;
pub mod telemetry;
pub mod trace;

/// Generated protobuf types for the eryx gRPC API.
// `missing_docs`: prost/tonic-generated types carry no doc comments. The large
// oneof variants (`ClientMessage.execute_request`, `ServerMessage.execute_result`)
// are boxed in `build.rs` via `.boxed(...)`, so no `large_enum_variant` allow is
// needed (boxing is in-memory only; the wire format is unchanged).
#[allow(missing_docs)]
pub mod proto {
    /// The `eryx.v1` package.
    pub mod eryx {
        /// Version 1 of the API.
        pub mod v1 {
            tonic::include_proto!("eryx.v1");
        }
    }
}
