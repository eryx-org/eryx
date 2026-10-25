//! Signatures `#[callback]` must accept beyond the basic ones covered by
//! `crates/eryx/tests/callback_macro.rs`.

use eryx::Callback;

/// Public callbacks, whose args struct must be at least as visible.
pub mod callbacks {
    use eryx::{CallbackError, callback};
    use serde_json::{Value, json};

    /// Adds one
    #[callback]
    pub async fn increment(mut n: u32) -> Result<Value, CallbackError> {
        n += 1;
        Ok(json!(n))
    }

    /// Crate-visible callback
    #[callback]
    pub(crate) async fn shout(message: String) -> Result<Value, CallbackError> {
        Ok(json!(message.to_uppercase()))
    }
}

#[test]
fn public_callback_with_mut_param() {
    let schema = serde_json::to_value(callbacks::increment.parameters_schema());
    assert!(
        schema.is_ok_and(|schema| schema["properties"].get("n").is_some()),
        "schema should describe `n`"
    );
    assert_eq!(callbacks::shout.name(), "shout");
}
