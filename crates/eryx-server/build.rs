//! Build script for eryx-server: compiles protobuf definitions.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true) // For integration tests
        // Box the large `ExecuteRequest` oneof variant so the generated
        // `ClientMessage` message enum doesn't trip clippy::large_enum_variant.
        .boxed(".eryx.v1.ClientMessage.message.execute_request")
        .compile_protos(&["proto/eryx/v1/eryx.proto"], &["proto"])?;
    Ok(())
}
