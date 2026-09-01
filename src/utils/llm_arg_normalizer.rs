//! LLM 参数归一化。参考 oh-my-pi `validateToolArguments` 中的 LLM 怪癖修复。
//!
//! LLM 在生成工具参数时通常会产出一些格式错误（null 占位、尾随空白、JSON 字符串
//! 表示的数组等），这些错误在 schema 校验之前应该被自动修复，而不是直接报错回
//! 给模型。本模块在 `tool_manager.execute` 的 `validate_input` 之前被调用。

use crate::types::{PropertyType, ToolInputSchema};

/// 对 LLM 生成的工具参数做归一化，修复常见的格式问题。
///
/// 在 `validate_input` 之前调用。修改 `args` 原地。
pub fn normalize_llm_args(args: &mut serde_json::Value, schema: &ToolInputSchema) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    normalize_object(obj, &schema.properties, schema.required.as_deref(), true);
}

fn normalize_object(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    properties: &std::collections::BTreeMap<String, crate::types::ToolInputProperty>,
    required: Option<&[String]>,
    trim_strings: bool,
) {
    // 可选字段的 null / "" → 删除。LLM 常用它们表示“不传”。
    if let Some(required) = required {
        obj.retain(|key, value| {
            required.contains(key) || (!value.is_null() && value.as_str() != Some(""))
        });
    }

    // 保持历史行为：仅顶层参数清理外围空白。递归对象里的字符串可能是
    // edit 正文、命令片段或 env 值，前后空白/换行属于有效载荷，不能 trim。
    if trim_strings {
        for value in obj.values_mut() {
            if let Some(text) = value.as_str() {
                let trimmed = text.trim();
                if trimmed.len() != text.len() {
                    *value = serde_json::Value::String(trimmed.to_string());
                }
            }
        }
    }
    for (key, property) in properties {
        if let Some(value) = obj.get_mut(key) {
            normalize_property(value, property, trim_strings);
        }
    }
}

fn normalize_property(
    value: &mut serde_json::Value,
    schema: &crate::types::ToolInputProperty,
    trim_strings: bool,
) {
    match schema.property_type {
        PropertyType::Array => {
            if let Some(text) = value.as_str() {
                let parsed = serde_json::from_str::<serde_json::Value>(text)
                    .ok()
                    .filter(serde_json::Value::is_array)
                    .unwrap_or_else(|| serde_json::json!([text]));
                *value = parsed;
            }
            let (Some(items), Some(values)) = (schema.items.as_deref(), value.as_array_mut())
            else {
                return;
            };
            for item in values {
                // 模型常把 string item 包成 {"text":"..."}；严格校验前取其首个字符串值。
                if items.property_type == PropertyType::String {
                    if let Some(text) = item
                        .as_object()
                        .and_then(|object| object.values().find_map(serde_json::Value::as_str))
                    {
                        *item = serde_json::Value::String(text.to_string());
                    }
                }
                // 数组元素是嵌套载荷；递归修正形状，但保留字符串字节。
                normalize_property(item, items, false);
            }
        }
        PropertyType::Object => {
            let Some(object) = value.as_object_mut() else {
                return;
            };
            if let Some(properties) = &schema.properties {
                normalize_object(object, properties, schema.required.as_deref(), false);
            }
            if let Some(crate::types::ToolAdditionalProperties::Schema(additional)) =
                &schema.additional_properties
            {
                for (key, child) in object {
                    if schema
                        .properties
                        .as_ref()
                        .is_none_or(|properties| !properties.contains_key(key))
                    {
                        normalize_property(child, additional, false);
                    }
                }
            }
        }
        PropertyType::String if trim_strings => {
            if let Some(text) = value.as_str() {
                let trimmed = text.trim();
                if trimmed.len() != text.len() {
                    *value = serde_json::Value::String(trimmed.to_string());
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PropertyType, ToolInputProperty, ToolInputSchema};
    use std::collections::BTreeMap;
    use serde_json::json;

    fn make_schema(props: Vec<(&str, PropertyType)>, required: &[&str]) -> ToolInputSchema {
        let mut p = BTreeMap::new();
        for (name, ty) in props {
            p.insert(name.to_string(), ToolInputProperty {
                property_type: ty,
                description: None,
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
                items: None, properties: None, required: None, additional_properties: None,
            });
        }
        ToolInputSchema {
            schema_type: Default::default(),
            properties: p,
            required: Some(required.iter().map(|s| s.to_string()).collect()),
            additional_properties: None,
        }
    }

    #[test]
    fn test_optional_null_is_removed() {
        let schema = make_schema(
            vec![("pattern", PropertyType::String), ("paths", PropertyType::Array)],
            &["pattern"],
        );
        let mut args = json!({"pattern": "foo", "paths": null});
        normalize_llm_args(&mut args, &schema);
        assert_eq!(args, json!({"pattern": "foo"}), "null optional should be removed");
    }

    #[test]
    fn test_optional_empty_string_is_removed() {
        let schema = make_schema(
            vec![("pattern", PropertyType::String), ("paths", PropertyType::Array)],
            &["pattern"],
        );
        let mut args = json!({"pattern": "foo", "paths": ""});
        normalize_llm_args(&mut args, &schema);
        assert_eq!(args, json!({"pattern": "foo"}), "empty string optional should be removed");
    }

    #[test]
    fn test_required_null_is_kept() {
        let schema = make_schema(
            vec![("pattern", PropertyType::String)],
            &["pattern"],
        );
        let mut args = json!({"pattern": null});
        normalize_llm_args(&mut args, &schema);
        assert_eq!(args, json!({"pattern": null}), "null required should be kept");
    }

    #[test]
    fn test_string_whitespace_trimmed() {
        let schema = make_schema(
            vec![("path", PropertyType::String)],
            &["path"],
        );
        let mut args = json!({"path": " src/main.rs \n"});
        normalize_llm_args(&mut args, &schema);
        assert_eq!(args["path"], "src/main.rs", "whitespace stripped");
    }

    #[test]
    fn test_json_string_encoded_array() {
        let schema = make_schema(
            vec![("paths", PropertyType::Array)],
            &[],
        );
        let mut args = json!({"paths": "[\"src\", \"tests\"]"});
        normalize_llm_args(&mut args, &schema);
        assert_eq!(args["paths"], json!(["src", "tests"]), "JSON string array decoded");
    }

    #[test]
    fn test_single_string_to_array() {
        let schema = make_schema(
            vec![("paths", PropertyType::Array)],
            &[],
        );
        let mut args = json!({"paths": "src"});
        normalize_llm_args(&mut args, &schema);
        assert_eq!(args["paths"], json!(["src"]), "plain string wrapped to array");
    }

    #[test]
    fn test_recursive_array_item_normalization() {
        let string_item = ToolInputProperty {
            property_type: PropertyType::String,
            description: None,
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
            items: None,
            properties: None,
            required: None,
            additional_properties: None,
        };
        let string_list = ToolInputProperty {
            property_type: PropertyType::Array,
            description: None,
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
            items: Some(Box::new(string_item)),
            properties: None,
            required: None,
            additional_properties: None,
        };
        let option = ToolInputProperty {
            property_type: PropertyType::Object,
            description: None,
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
            items: None,
            properties: Some(BTreeMap::from([
                ("pros".into(), string_list.clone()),
                ("cons".into(), string_list),
            ])),
            required: None,
            additional_properties: Some(true.into()),
        };
        let schema = ToolInputSchema {
            schema_type: Default::default(),
            properties: BTreeMap::from([(
                "options".into(),
                ToolInputProperty {
                    property_type: PropertyType::Array,
                    description: None,
                    enum_values: None,
                    minimum: None,
                    maximum: None,
                    min_length: None,
                    max_length: None,
                    items: Some(Box::new(option)),
                    properties: None,
                    required: None,
                    additional_properties: None,
                },
            )]),
            required: Some(vec!["options".into()]),
            additional_properties: None,
        };
        let mut args = json!({
            "options": [{
                "pros": "fast\nsimple",
                "cons": [{"text": "stateful"}]
            }]
        });

        normalize_llm_args(&mut args, &schema);

        assert_eq!(args["options"][0]["pros"], json!(["fast\nsimple"]));
        assert_eq!(args["options"][0]["cons"], json!(["stateful"]));
        assert!(crate::utils::schema_validator::validate_input(&args, &schema).valid);
    }

    #[test]
    fn test_recursive_literal_strings_preserve_whitespace() {
        let literal = ToolInputProperty {
            property_type: PropertyType::String,
            description: None,
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
            items: None,
            properties: None,
            required: None,
            additional_properties: None,
        };
        let operation = ToolInputProperty {
            property_type: PropertyType::Object,
            description: None,
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
            items: None,
            properties: Some(BTreeMap::from([
                ("old_text".into(), literal.clone()),
                ("new_text".into(), literal.clone()),
            ])),
            required: None,
            additional_properties: Some(false.into()),
        };
        let env = ToolInputProperty {
            property_type: PropertyType::Object,
            description: None,
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
            items: None,
            properties: None,
            required: None,
            additional_properties: Some(literal.into()),
        };
        let schema = ToolInputSchema {
            schema_type: Default::default(),
            properties: BTreeMap::from([
                (
                    "ops".into(),
                    ToolInputProperty {
                        property_type: PropertyType::Array,
                        description: None,
                        enum_values: None,
                        minimum: None,
                        maximum: None,
                        min_length: None,
                        max_length: None,
                        items: Some(Box::new(operation)),
                        properties: None,
                        required: None,
                        additional_properties: None,
                    },
                ),
                ("env".into(), env),
            ]),
            required: Some(vec!["ops".into()]),
            additional_properties: None,
        };
        let mut args = json!({
            "ops": [{"old_text": "DROP_ME\n", "new_text": "    replacement"}],
            "env": {"INDENT": "  preserved  "}
        });
        let original = args.clone();

        normalize_llm_args(&mut args, &schema);

        assert_eq!(args, original, "nested literal strings must stay byte-exact");
    }

    #[test]
    fn test_noop_on_normal_format() {
        let schema = make_schema(
            vec![("pattern", PropertyType::String), ("paths", PropertyType::Array)],
            &["pattern"],
        );
        let mut args = json!({"pattern": "foo", "paths": ["src", "tests"]});
        let original = args.clone();
        normalize_llm_args(&mut args, &schema);
        assert_eq!(args, original, "normal args unchanged");
    }
}