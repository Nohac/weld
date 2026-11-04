//! Compact public JSON payloads, separate from the native binary RPC contract.

use schemars::JsonSchema;
use serde::{Serialize, Serializer, ser::Error};
use serde_json::Value;
use std::{fmt, ops::Deref};

/// Response-only adapter used by all generated CLI, JSON-RPC and MCP methods.
/// Protocol envelopes are serialized outside this wrapper, preserving null IDs.
#[derive(Debug, JsonSchema)]
#[schemars(transparent)]
pub struct Json<T>(pub T);

impl<T> Deref for Json<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
impl<T: fmt::Display> fmt::Display for Json<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl<T: Serialize> Serialize for Json<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        value(&self.0)
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }
}

pub fn value(value: &impl Serialize) -> serde_json::Result<Value> {
    let mut value = serde_json::to_value(value)?;
    omit_null_fields(&mut value);
    Ok(value)
}

fn omit_null_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => fields.retain(|_, value| {
            omit_null_fields(value);
            !value.is_null()
        }),
        // Null array entries are positional data, not optional object fields.
        Value::Array(values) => values.iter_mut().for_each(omit_null_fields),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn public_payloads_omit_null_fields_without_changing_arrays_or_false_values() {
        let payload = json!({"missing":null,"false":false,"zero":0,"empty":"",
            "rows":[null,{"missing":null,"present":1},["p",57,null,20]],
            "nested":{"missing":null}});
        assert_eq!(
            serde_json::to_value(Json(&payload)).expect("public JSON"),
            json!({"false":false,"zero":0,"empty":"","rows":[null,{"present":1},["p",57,null,20]],"nested":{}})
        );
        assert!(
            payload.get("missing").is_some(),
            "source serialization remains unchanged"
        );
        let envelope = json!({"jsonrpc":"2.0","id":null,"result":Json(payload)});
        assert!(
            envelope.get("id").is_some(),
            "required protocol null remains"
        );
        assert!(envelope["result"].get("missing").is_none());
    }
}
