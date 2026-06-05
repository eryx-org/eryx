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
// These lints apply to prost/tonic-generated code we don't control:
// - `missing_docs`: generated types carry no doc comments.
// - `large_enum_variant`: the `ClientMessage` oneof mixes a large `ExecuteRequest`
//   with a small `CallbackResponse`; prost can't box it without changing the API,
//   and the size difference is inherent to the wire schema.
#[allow(missing_docs, clippy::large_enum_variant)]
pub mod proto {
    /// The `eryx.v1` package.
    pub mod eryx {
        /// Version 1 of the API.
        pub mod v1 {
            tonic::include_proto!("eryx.v1");
        }
    }
}
