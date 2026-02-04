#![allow(dead_code, unused_variables, unused_imports)]

# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async pub fn run() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let result = sandbox.execute("print('Hello from Eryx!')").await?;
    println!("{}", result.stdout);
    Ok(())
}