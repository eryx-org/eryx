#![allow(dead_code, unused_variables, unused_imports)]

# extern crate eryx;
# extern crate tokio;
use eryx::{Sandbox, session::{InProcessSession, Session}};

#[tokio::main]
async pub fn run() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;
    let mut session = InProcessSession::new(&sandbox).await?;

    // State persists across executions
    session.execute("x = 42").await?;
    let result = session.execute("print(x * 2)").await?;
    println!("{}", result.stdout); // "84"

    Ok(())
}