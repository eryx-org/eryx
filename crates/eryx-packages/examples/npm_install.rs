//! Install npm packages into a cache directory and print the lockfile.
//!
//! ```sh
//! cargo run -p eryx-packages --example npm_install -- /tmp/eryx-packages zod date-fns@^4
//! ```

use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let cache = args
        .next()
        .ok_or("usage: npm_install <cache dir> <package>...")?;
    let specs: Vec<String> = args.collect();
    let start = Instant::now();
    let installed = eryx_packages::npm::Install::new(specs, &cache)
        .run()
        .await?;
    for (path, package) in &installed.lockfile.packages {
        println!("{path} {}", package.version);
    }
    for warning in &installed.warnings {
        eprintln!("warning: {warning}");
    }
    eprintln!(
        "{} packages in {:?} at {}",
        installed.lockfile.packages.len(),
        start.elapsed(),
        installed.node_modules.display()
    );
    Ok(())
}
