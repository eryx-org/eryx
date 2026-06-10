//! Build script for eryx-server: compiles protobuf definitions.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        // Box the large oneof variants so the small variants (e.g.
        // `CallbackResponse`) don't pay the size of the large ones. This is an
        // in-memory representation change only — the protobuf wire format is
        // unchanged — and avoids a `clippy::large_enum_variant` allow.
        // Path is `<fq message>.<oneof name>.<field name>`. Both oneofs are named
        // `message`, so e.g. `ClientMessage.message.execute_request`.
        .boxed(".eryx.v1.ClientMessage.message.execute_request")
        .boxed(".eryx.v1.ServerMessage.message.execute_result")
        .build_server(true)
        .build_client(true) // For integration tests
        // Box the large `ExecuteRequest` oneof variant so the generated
        // `ClientMessage` message enum doesn't trip clippy::large_enum_variant.
        .boxed(".eryx.v1.ClientMessage.message.execute_request")
        .compile_protos(&["proto/eryx/v1/eryx.proto"], &["proto"])?;
    Ok(())
}
