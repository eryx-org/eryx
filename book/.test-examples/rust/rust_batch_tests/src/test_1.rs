#![allow(dead_code, unused_variables, unused_imports)]

# extern crate eryx;
# extern crate tokio;
# extern crate serde;
# extern crate serde_json;
# extern crate schemars;
use std::{future::Future, pin::Pin};
use eryx::{TypedCallback, CallbackError, Sandbox, JsonSchema};
use serde::Deserialize;
use serde_json::{json, Value};

// Define a callback with typed arguments
#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    /// The message to echo back
    message: String,
}

struct Echo;

impl TypedCallback for Echo {
    type Args = EchoArgs;

    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echoes back the message" }

    fn invoke_typed(
        &self,
        args: EchoArgs
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            Ok(json!({ "echoed": args.message }))
        })
    }
}

#[tokio::main]
async pub fn run() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded()
        .with_callback(Echo)
        .build()?;

    let result = sandbox.execute(r#"
# Callbacks are available as async functions
response = await echo(message="Hello!")
print(f"Echo: {response}")
    "#).await?;

    println!("{}", result.stdout);
    Ok(())
}