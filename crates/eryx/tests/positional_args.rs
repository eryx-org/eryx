//! Positional callback arguments bind in the order a callback's schema
//! declares its parameters, not alphabetically (#860), in every guest.
#![cfg(feature = "embedded")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;

use eryx::{CallbackError, DynamicCallback, JsonSchema, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

/// `zip` is declared before `city`, the reverse of alphabetical order.
fn place() -> DynamicCallback {
    DynamicCallback::builder("place", "Echo a place", |args| {
        Box::pin(async move { Ok(args) })
    })
    .param("zip", "string", "postal code", true)
    .param("city", "string", "city name", true)
    .build()
}

#[derive(Deserialize, JsonSchema)]
struct PlaceArgs {
    zip: String,
    city: String,
}

/// The same callback, with its schema derived from a struct.
struct TypedPlace;

impl TypedCallback for TypedPlace {
    type Args = PlaceArgs;

    fn name(&self) -> &str {
        "typed_place"
    }

    fn description(&self) -> &str {
        "Echo a place"
    }

    fn invoke_typed(
        &self,
        args: PlaceArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({ "zip": args.zip, "city": args.city })) })
    }
}

#[tokio::test]
async fn python_binds_positionals_in_declaration_order() {
    let sandbox = Sandbox::embedded()
        .with_callback(place())
        .with_callback(TypedPlace)
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            r#"
import json
print(json.dumps(await place("10001", "NYC"), sort_keys=True))
print(json.dumps(await typed_place("10001", "NYC"), sort_keys=True))
# Bound by the host rather than the guest's wrapper.
print(json.dumps(await invoke("place", "10001", "NYC"), sort_keys=True))
# list_callbacks() shows the schema in declaration order too.
print(list(next(cb for cb in list_callbacks() if cb["name"] == "place")["parameters_schema"]["properties"]))
"#,
        )
        .await
        .unwrap();
    let expected = r#"{"city": "NYC", "zip": "10001"}"#;
    let stdout = out.stdout_text();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[..3], [expected, expected, expected], "{stdout}");
    assert_eq!(lines[3], "['zip', 'city']", "{stdout}");
}

#[cfg(feature = "javascript")]
#[tokio::test]
async fn javascript_binds_positionals_in_declaration_order() {
    let sandbox = Sandbox::embedded()
        .javascript()
        .with_callback(place())
        .with_callback(TypedPlace)
        .build()
        .unwrap();
    let out = sandbox
        .execute(
            r#"
const show = ({zip, city}) => `${zip} ${city}`;
console.log(show(await place("10001", "NYC")));
console.log(show(await typed_place("10001", "NYC")));
console.log(show(await invoke("place", "10001", "NYC")));
"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout_utf8().unwrap(),
        "10001 NYC\n10001 NYC\n10001 NYC\n"
    );
}
