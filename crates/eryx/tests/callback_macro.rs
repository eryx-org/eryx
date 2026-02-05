//! Integration tests for the #[callback] proc macro.

#![cfg(feature = "macros")]

use eryx::{Callback, CallbackError, callback};
use serde_json::{Value, json};

#[cfg(feature = "embedded")]
use eryx::Sandbox;

/// Returns the current Unix timestamp
#[callback]
async fn get_time() -> Result<Value, CallbackError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs();
    Ok(json!({ "timestamp": now }))
}

/// Echoes the message back
#[callback]
async fn echo(message: String) -> Result<Value, CallbackError> {
    Ok(json!({ "echoed": message }))
}

/// Echoes the message with optional repeat count
#[callback]
async fn echo_repeat(message: String, repeat: Option<u32>) -> Result<Value, CallbackError> {
    let repeat = repeat.unwrap_or(1);
    Ok(json!({ "echoed": message.repeat(repeat as usize) }))
}

/// Adds two numbers
#[callback]
async fn add(a: i64, b: i64) -> Result<Value, CallbackError> {
    Ok(json!({ "sum": a + b }))
}

#[test]
fn callback_macro_creates_unit_struct() {
    // The macro should create a unit struct with the function name
    let _: get_time = get_time;
    let _: echo = echo;
    let _: echo_repeat = echo_repeat;
    let _: add = add;
}

#[test]
fn callback_macro_implements_callback_trait() {
    // Should implement Callback trait (via TypedCallback blanket impl)
    fn assert_callback<T: Callback>() {}
    assert_callback::<get_time>();
    assert_callback::<echo>();
    assert_callback::<echo_repeat>();
    assert_callback::<add>();
}

#[test]
fn callback_macro_returns_correct_name() {
    assert_eq!(Callback::name(&get_time), "get_time");
    assert_eq!(Callback::name(&echo), "echo");
    assert_eq!(Callback::name(&echo_repeat), "echo_repeat");
    assert_eq!(Callback::name(&add), "add");
}

#[test]
fn callback_macro_returns_doc_comment_as_description() {
    assert_eq!(
        Callback::description(&get_time),
        "Returns the current Unix timestamp"
    );
    assert_eq!(Callback::description(&echo), "Echoes the message back");
    assert_eq!(
        Callback::description(&echo_repeat),
        "Echoes the message with optional repeat count"
    );
    assert_eq!(Callback::description(&add), "Adds two numbers");
}

#[test]
fn callback_macro_generates_schema() {
    // No args callback should have empty/null schema
    let schema = Callback::parameters_schema(&get_time);
    let value = schema.to_value();
    // Unit type schema - should be valid
    assert!(value.is_object() || value.is_boolean());

    // Single arg callback should have properties
    let schema = Callback::parameters_schema(&echo);
    let value = schema.to_value();
    let properties = value.get("properties").expect("should have properties");
    assert!(properties.get("message").is_some());

    // Multi arg callback should have all properties
    let schema = Callback::parameters_schema(&add);
    let value = schema.to_value();
    let properties = value.get("properties").expect("should have properties");
    assert!(properties.get("a").is_some());
    assert!(properties.get("b").is_some());
}

#[tokio::test]
async fn callback_macro_invoke_no_args() {
    let result = Callback::invoke(&get_time, json!({})).await;
    assert!(result.is_ok());
    let value = result.expect("should succeed");
    assert!(value.get("timestamp").is_some());
}

#[tokio::test]
async fn callback_macro_invoke_with_args() {
    let result = Callback::invoke(&echo, json!({ "message": "hello" })).await;
    assert!(result.is_ok());
    let value = result.expect("should succeed");
    assert_eq!(value["echoed"], "hello");
}

#[tokio::test]
async fn callback_macro_invoke_with_optional_args() {
    // Without optional arg
    let result = Callback::invoke(&echo_repeat, json!({ "message": "hi" })).await;
    assert!(result.is_ok());
    assert_eq!(result.expect("should succeed")["echoed"], "hi");

    // With optional arg
    let result = Callback::invoke(&echo_repeat, json!({ "message": "hi", "repeat": 3 })).await;
    assert!(result.is_ok());
    assert_eq!(result.expect("should succeed")["echoed"], "hihihi");
}

#[tokio::test]
async fn callback_macro_invoke_with_multiple_args() {
    let result = Callback::invoke(&add, json!({ "a": 17, "b": 25 })).await;
    assert!(result.is_ok());
    assert_eq!(result.expect("should succeed")["sum"], 42);
}

#[cfg(feature = "embedded")]
#[tokio::test]
async fn callback_macro_with_sandbox() {
    let sandbox = Sandbox::embedded()
        .with_callback(get_time)
        .with_callback(echo)
        .with_callback(add)
        .build()
        .expect("should build sandbox");

    let result = sandbox
        .execute(
            r#"
t = await get_time()
assert 'timestamp' in t

e = await echo(message="hello")
assert e['echoed'] == 'hello'

s = await add(a=10, b=32)
assert s['sum'] == 42

print("all tests passed")
"#,
        )
        .await
        .expect("should execute");

    assert!(result.stdout.contains("all tests passed"));
}

#[test]
fn callback_macro_can_be_boxed_as_trait_object() {
    let callbacks: Vec<Box<dyn Callback>> = vec![Box::new(get_time), Box::new(echo), Box::new(add)];

    assert_eq!(callbacks.len(), 3);
    assert_eq!(callbacks[0].name(), "get_time");
    assert_eq!(callbacks[1].name(), "echo");
    assert_eq!(callbacks[2].name(), "add");
}
