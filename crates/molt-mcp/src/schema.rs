//! Holding calls to a tool's input schema.
//!
//! The bridge checks every call's input against the schema the model was
//! given before it reaches the server, and refuses one that does not fit,
//! so the server only ever gets inputs of the shape it declared. The check
//! covers the keywords that decide a value's shape (`type`, `properties`,
//! `required`, `additionalProperties`, `items`, `enum`, `const`, `anyOf`,
//! `oneOf`, `allOf`); the others, such as `format` or `minimum`, are left to
//! the server.

use serde_json::Value;

/// Why `value` does not fit `schema`, if it does not.
pub fn check(schema: &Value, value: &Value) -> Result<(), String> {
    at(schema, value, "input")
}

fn type_fits(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => {
            value.as_i64().is_some() || value.as_u64().is_some() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => true,
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn at(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        // `true` takes anything; `false` nothing.
        return if schema == &Value::Bool(false) { Err(format!("{path} is not allowed")) } else { Ok(()) };
    };
    match schema.get("type") {
        Some(Value::String(t)) if !type_fits(t, value) => {
            return Err(format!("{path} must be {t}, not {}", kind(value)));
        }
        Some(Value::Array(types)) if !types.iter().filter_map(Value::as_str).any(|t| type_fits(t, value)) => {
            let names: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
            return Err(format!("{path} must be {}, not {}", names.join(" or "), kind(value)));
        }
        _ => {}
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            return Err(format!("{path} must be one of {}", Value::Array(allowed.clone())));
        }
    }
    if let Some(constant) = schema.get("const") {
        if constant != value {
            return Err(format!("{path} must be {constant}"));
        }
    }
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        for sub in all {
            at(sub, value, path)?;
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(any) = schema.get(key).and_then(Value::as_array) {
            if !any.is_empty() && !any.iter().any(|sub| at(sub, value, path).is_ok()) {
                return Err(format!("{path} fits none of the shapes the schema allows"));
            }
        }
    }
    if let Value::Object(fields) = value {
        let properties = schema.get("properties").and_then(Value::as_object);
        for name in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !fields.contains_key(name) {
                return Err(format!("{path} needs the field {name:?}"));
            }
        }
        for (name, field) in fields {
            let inner = format!("{path}.{name}");
            match properties.and_then(|p| p.get(name)) {
                Some(sub) => at(sub, field, &inner)?,
                None => match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        let known: Vec<&str> = properties.into_iter().flatten().map(|(k, _)| k.as_str()).collect();
                        return Err(format!(
                            "{path} has the field {name:?}, which the schema does not have; it takes {}",
                            if known.is_empty() { "no fields".to_owned() } else { known.join(", ") }
                        ));
                    }
                    Some(sub @ Value::Object(_)) => at(sub, field, &inner)?,
                    _ => {}
                },
            }
        }
    }
    if let (Value::Array(items), Some(sub)) = (value, schema.get("items")) {
        for (i, item) in items.iter().enumerate() {
            at(sub, item, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

/// The keywords a schema may use and still be held to strictly by the
/// model API. Anything else, and the declaration is not marked strict: a
/// schema the API's strict mode refuses would fail every model call of the run.
const STRICT_KEYWORDS: &[&str] =
    &["type", "description", "title", "properties", "required", "additionalProperties", "items", "enum", "const"];

/// Whether the model API can hold calls to `schema` exactly: only the
/// keywords above, one type per schema, and every object closed with
/// `"additionalProperties": false`.
pub fn strict_ok(schema: &Value) -> bool {
    let Some(map) = schema.as_object() else { return false };
    if map.keys().any(|k| !STRICT_KEYWORDS.contains(&k.as_str())) {
        return false;
    }
    match map.get("type").and_then(Value::as_str) {
        Some("object") => {
            map.get("additionalProperties") == Some(&Value::Bool(false))
                && map.get("properties").and_then(Value::as_object).is_none_or(|props| props.values().all(strict_ok))
        }
        Some("array") => map.get("items").is_some_and(strict_ok),
        Some("string" | "integer" | "number" | "boolean" | "null") => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_input_that_does_not_fit_the_schema_is_refused_with_where() {
        let schema = json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "labels": { "type": "array", "items": { "type": "string", "enum": ["bug", "docs"] } },
            },
            "required": ["id"],
            "additionalProperties": false,
        });
        assert!(check(&schema, &json!({ "id": 3, "labels": ["bug"] })).is_ok());
        let says = |value: Value| check(&schema, &value).unwrap_err();
        assert!(says(json!({})).contains("needs the field \"id\""));
        assert!(says(json!({ "id": "3" })).contains("input.id must be integer, not a string"));
        assert!(says(json!({ "id": 3, "labels": ["x"] })).contains("input.labels[0] must be one of"));
        assert!(says(json!({ "id": 3, "rm": true })).contains("does not have; it takes id, labels"));
        assert!(says(json!([])).contains("must be object"));
        // A schema that leaves fields open takes them.
        assert!(check(&json!({ "type": "object" }), &json!({ "any": 1 })).is_ok());
    }

    #[test]
    fn only_a_closed_schema_of_plain_keywords_is_marked_strict() {
        let closed = json!({
            "type": "object",
            "properties": { "q": { "type": "string", "description": "What to find." } },
            "required": ["q"],
            "additionalProperties": false,
        });
        assert!(strict_ok(&closed));
        assert!(!strict_ok(&json!({ "type": "object", "properties": { "q": { "type": "string" } } })));
        let formatted = json!({ "type": "object", "properties": { "at": { "type": "string", "format": "date" } },
            "additionalProperties": false });
        assert!(!strict_ok(&formatted));
    }
}
