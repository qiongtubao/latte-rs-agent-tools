//! JSON-Schema validation for tool inputs.
//!
//! Mirrors `latte-ts-agent-tools/src/utils/schema-validator.ts`. Implements a
//! minimal, dependency-free validator that supports the JSON-Schema types
//! emitted by `ToolInputSchema`.


use crate::types::{PropertyType, ToolInputProperty, ToolInputSchema};

/// Returned by [`validate_input`] / [`validate_schema`].
#[derive(Debug, Clone, Default)]
pub struct ValidationErrors {
    /// Whether the input is valid against the schema.
    pub valid: bool,
    /// Per-field error messages.
    pub errors: Vec<String>,
}

impl ValidationErrors {
    fn new(errors: Vec<String>) -> Self {
        let valid = errors.is_empty();
        Self { valid, errors }
    }
}

/// Validate an input object against a `ToolInputSchema`.
///
/// Returns a populated `ValidationErrors` regardless of outcome. The caller can
/// branch on `valid` and inspect `errors` for diagnostics.
pub fn validate_input(input: &serde_json::Value, schema: &ToolInputSchema) -> ValidationErrors {
    let mut errors = Vec::new();

    if !input.is_object() {
        errors.push(format!("Input must be a JSON object, got {}", json_kind(input)));
        return ValidationErrors::new(errors);
    }
    let obj = input.as_object().expect("checked above");

    // Required properties
    if let Some(required) = schema.required.as_ref() {
        for prop in required {
            if !obj.contains_key(prop) {
                errors.push(format!("Missing required property: {}", prop));
            }
        }
    }

    // Property type checks
    for (key, value) in obj {
        if let Some(prop_schema) = schema.properties.get(key) {
            if let Some(msg) = validate_type(value, key, prop_schema) {
                errors.push(msg);
            }
        } else if matches!(schema.additional_properties, Some(false)) {
            errors.push(format!("Unknown property: {}", key));
        }
    }

    ValidationErrors::new(errors)
}

/// Validate the structural shape of a tool input schema.
pub fn validate_schema(schema: &serde_json::Value) -> ValidationErrors {
    let mut errors = Vec::new();

    let obj = match schema.as_object() {
        Some(o) => o,
        None => {
            errors.push("Schema must be an object".to_string());
            return ValidationErrors::new(errors);
        }
    };

    if obj.get("type").and_then(|v| v.as_str()) != Some("object") {
        errors.push("Schema type must be 'object'".to_string());
    }

    if let Some(props) = obj.get("properties") {
        if !props.is_object() {
            errors.push("Schema properties must be an object".to_string());
        }
    }

    if let Some(required) = obj.get("required") {
        if !required.is_array() {
            errors.push("Schema required must be an array".to_string());
        }
    }

    ValidationErrors::new(errors)
}

fn validate_type(
    value: &serde_json::Value,
    key: &str,
    schema: &ToolInputProperty,
) -> Option<String> {
    let actual = json_kind(value);

    let type_ok = match schema.property_type {
        PropertyType::Integer => actual == "integer",
        PropertyType::Number => actual == "number" || actual == "integer",
        PropertyType::String => actual == "string",
        PropertyType::Boolean => actual == "boolean",
        PropertyType::Array => actual == "array",
        PropertyType::Object => actual == "object",
        PropertyType::Null => actual == "null",
    };

    if !type_ok {
        return Some(format!(
            "Property '{}' must be {:?}, got {}",
            key, schema.property_type, actual
        ));
    }

    // Enum check
    if let Some(values) = schema.enum_values.as_ref() {
        if !values.iter().any(|v| v == value) {
            let labels: Vec<String> = values.iter().map(|v| v.to_string()).collect();
            return Some(format!(
                "Property '{}' must be one of: {}",
                key,
                labels.join(", ")
            ));
        }
    }

    // Numeric constraints
    if let Some(num) = value.as_f64() {
        if let Some(min) = schema.minimum {
            if num < min {
                return Some(format!("Property '{}' must be >= {}", key, min));
            }
        }
        if let Some(max) = schema.maximum {
            if num > max {
                return Some(format!("Property '{}' must be <= {}", key, max));
            }
        }
    }

    // String / array length constraints
    let length = value
        .as_str()
        .map(|s| s.chars().count())
        .or_else(|| value.as_array().map(|a| a.len()));
    if let Some(len) = length {
        if let Some(min) = schema.min_length {
            if len < min {
                return Some(format!(
                    "Property '{}' must be at least {} characters",
                    key, min
                ));
            }
        }
        if let Some(max) = schema.max_length {
            if len > max {
                return Some(format!(
                    "Property '{}' must be at most {} characters",
                    key, max
                ));
            }
        }
    }

    None
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use crate::types::{PropertyType, SchemaType, ToolInputProperty, ToolInputSchema};

    fn schema_with(props: BTreeMap<String, ToolInputProperty>, required: Vec<String>) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: SchemaType,
            properties: props,
            required: Some(required),
            additional_properties: Some(false),
        }
    }

    fn prop(ty: PropertyType) -> ToolInputProperty {
        ToolInputProperty {
            property_type: ty,
            description: None,
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
            items: None, properties: None, required: None, additional_properties: None,
        }
    }

    #[test]
    fn rejects_non_object_input() {
        let s = ToolInputSchema::default();
        let result = validate_input(&serde_json::json!("not an object"), &s);
        assert!(!result.valid);
    }

    #[test]
    fn enforces_required() {
        let mut props = BTreeMap::new();
        props.insert("name".into(), prop(PropertyType::String));
        let schema = schema_with(props, vec!["name".into()]);
        let result = validate_input(&serde_json::json!({}), &schema);
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("name")));
    }

    #[test]
    fn enforces_type() {
        let mut props = BTreeMap::new();
        props.insert("n".into(), prop(PropertyType::Integer));
        let schema = schema_with(props, vec![]);
        let result = validate_input(&serde_json::json!({ "n": "not a number" }), &schema);
        assert!(!result.valid);
    }

    #[test]
    fn accepts_correct_input() {
        let mut props = BTreeMap::new();
        props.insert("n".into(), prop(PropertyType::Integer));
        let schema = schema_with(props, vec!["n".into()]);
        let result = validate_input(&serde_json::json!({ "n": 42 }), &schema);
        assert!(result.valid, "{:?}", result.errors);
    }

    #[test]
    fn validates_schema_shape() {
        let result = validate_schema(&serde_json::json!({ "type": "not_object" }));
        assert!(!result.valid);
    }
}
