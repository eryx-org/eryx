//! Build script for eryx-server: compiles protobuf definitions.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true) // For integration tests
        .compile_protos(&["proto/eryx/v1/eryx.proto"], &["proto"])?;
    Ok(())
}
