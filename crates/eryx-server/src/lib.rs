//! gRPC server for sandboxed Python execution via eryx.
//!
//! This crate provides a [tonic] gRPC server that wraps the [`eryx`] sandbox,
//! enabling remote Python execution with bidirectional callback streaming.
//! It is designed as the backend for the Grafana Assistant's `execute_python` tool.

#![deny(unsafe_code)]

pub mod callbacks;
pub mod output;
pub mod service;
pub mod trace;

/// Generated protobuf types for the eryx gRPC API.
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
