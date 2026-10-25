//! Records the host target triple, so `compile` can tell whether an artifact it
//! built for `--target` can be loaded on this machine for verification.

fn main() -> Result<(), std::env::VarError> {
    let target = std::env::var("TARGET")?;
    println!("cargo:rustc-env=ERYX_PRECOMPILE_HOST_TARGET={target}");
    Ok(())
}
