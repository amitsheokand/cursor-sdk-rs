//! Conversions between `google.protobuf.Struct` and [`serde_json::Value`].
//!
//! Every free-form payload in `sdk.v1` (stream message bodies, tool arguments,
//! store records) is a `Struct`. Callers of this crate only ever see
//! `serde_json::Value`, so these helpers sit on the boundary.

use prost_types::{value::Kind, ListValue, Struct, Value as PbValue};
use serde_json::{Map, Number, Value as JsonValue};

/// Convert a protobuf `Struct` into a JSON object value.
///
/// A missing struct becomes [`JsonValue::Null`], matching the proto3 reading
/// of an unset message field.
pub fn struct_to_json(source: Option<&Struct>) -> JsonValue {
    match source {
        Some(value) => JsonValue::Object(fields_to_map(value)),
        None => JsonValue::Null,
    }
}

fn fields_to_map(source: &Struct) -> Map<String, JsonValue> {
    source
        .fields
        .iter()
        .map(|(key, value)| (key.clone(), pb_value_to_json(value)))
        .collect()
}

fn pb_value_to_json(source: &PbValue) -> JsonValue {
    match &source.kind {
        None | Some(Kind::NullValue(_)) => JsonValue::Null,
        Some(Kind::BoolValue(value)) => JsonValue::Bool(*value),
        Some(Kind::NumberValue(value)) => number_to_json(*value),
        Some(Kind::StringValue(value)) => JsonValue::String(value.clone()),
        Some(Kind::StructValue(value)) => JsonValue::Object(fields_to_map(value)),
        Some(Kind::ListValue(value)) => {
            JsonValue::Array(value.values.iter().map(pb_value_to_json).collect())
        }
    }
}

/// Convert a JSON value into a protobuf `Struct`.
///
/// Returns `None` for anything that is not a JSON object, because `Struct` can
/// only encode objects. Use [`json_to_object_struct`] when you want scalars
/// wrapped instead of dropped.
pub fn json_to_struct(source: &JsonValue) -> Option<Struct> {
    match source {
        JsonValue::Object(map) => Some(map_to_struct(map)),
        _ => None,
    }
}

/// Convert a JSON value into a `Struct`, wrapping non-objects as `{"value": …}`.
///
/// Custom-tool results are `Struct`s on the wire, so a scalar return value has
/// to be given an object shell before it can be encoded — see the custom-tool
/// notes in the bridge's `docs/services.md`.
pub fn json_to_object_struct(source: JsonValue) -> Struct {
    match source {
        JsonValue::Object(map) => map_to_struct(&map),
        other => {
            let mut fields = std::collections::BTreeMap::new();
            fields.insert("value".to_string(), json_to_pb_value(&other));
            Struct { fields }
        }
    }
}

fn map_to_struct(source: &Map<String, JsonValue>) -> Struct {
    Struct {
        fields: source
            .iter()
            .map(|(key, value)| (key.clone(), json_to_pb_value(value)))
            .collect(),
    }
}

/// `Struct` stores every number as a double. Whole values are re-emitted as
/// JSON integers, the way protobuf's own JSON mapping canonicalizes them, so a
/// count or a byte size does not surface as `3.0`.
fn number_to_json(value: f64) -> JsonValue {
    if value.fract() == 0.0 && value.abs() <= (i64::MAX as f64) {
        return JsonValue::Number(Number::from(value as i64));
    }
    // NaN and the infinities have no JSON representation.
    Number::from_f64(value)
        .map(JsonValue::Number)
        .unwrap_or(JsonValue::Null)
}

fn json_to_pb_value(source: &JsonValue) -> PbValue {
    let kind = match source {
        JsonValue::Null => Kind::NullValue(0),
        JsonValue::Bool(value) => Kind::BoolValue(*value),
        // Integers beyond f64's exact range lose precision here; that is the
        // same lossy mapping the protobuf JSON spec applies to Struct.
        JsonValue::Number(value) => Kind::NumberValue(value.as_f64().unwrap_or(0.0)),
        JsonValue::String(value) => Kind::StringValue(value.clone()),
        JsonValue::Array(values) => Kind::ListValue(ListValue {
            values: values.iter().map(json_to_pb_value).collect(),
        }),
        JsonValue::Object(map) => Kind::StructValue(map_to_struct(map)),
    };
    PbValue { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_nested_values() {
        let original = serde_json::json!({
            "text": "hi",
            "count": 3,
            "ok": true,
            "nested": {"list": [1, "two", null, {"deep": false}]},
        });
        let encoded = json_to_struct(&original).expect("object encodes");
        assert_eq!(struct_to_json(Some(&encoded)), original);
    }

    #[test]
    fn wraps_scalars_for_tool_results() {
        let wrapped = json_to_object_struct(JsonValue::String("done".into()));
        assert_eq!(
            struct_to_json(Some(&wrapped)),
            serde_json::json!({"value": "done"})
        );
    }

    #[test]
    fn whole_numbers_survive_as_integers() {
        // `Struct` has only doubles; a byte count must not come back as `12.0`.
        let encoded =
            json_to_struct(&serde_json::json!({"size": 12, "ratio": 0.5})).expect("object encodes");
        assert_eq!(
            struct_to_json(Some(&encoded)),
            serde_json::json!({"size": 12, "ratio": 0.5})
        );
    }

    #[test]
    fn missing_struct_reads_as_null() {
        assert_eq!(struct_to_json(None), JsonValue::Null);
    }
}
