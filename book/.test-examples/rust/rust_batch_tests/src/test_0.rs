#![allow(dead_code, unused_variables, unused_imports)]

# extern crate eryx;
# extern crate tokio;
use eryx::Sandbox;

#[tokio::main]
async pub fn run() -> Result<(), eryx::Error> {
    // Create a sandbox with embedded Python runtime
    let sandbox = Sandbox::embedded().build()?;

    // Execute Python code
    let result = sandbox.execute(r#"
print("Hello from Python!")
x = 2 + 2
print(f"2 + 2 = {x}")
    "#).await?;

    println!("{}", result.stdout);
    // Output:
    // Hello from Python!
    // 2 + 2 = 4

    Ok(())
}