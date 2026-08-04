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
    if !args.is_object() {
        return;
    }
    let Some(obj) = args.as_object_mut() else {
        return;
    };

    // 1. 可选字段的 null / "" → 删除（如果该字段不在 required 中）。
    //    LLM 常用 null 或空串表示"不传"，但校验层会拒绝类型不符。
    if let Some(required) = &schema.required {
        for (key, value) in obj.clone().iter() {
            if !required.contains(key) {
                if value.is_null() || value.as_str() == Some("") {
                    obj.remove(key);
                }
            }
        }
    }

    // 2. 字符串值的尾随空白/换行去除（路径、标识符类字段）。
    //    LLM 偶尔在字符串末尾附加换行符。
    let mut keys_to_trim: Vec<String> = Vec::new();
    for (key, value) in &*obj {
        if let Some(s) = value.as_str() {
            let trimmed = s.trim();
            if trimmed.len() != s.len() {
                keys_to_trim.push(key.clone());
            }
        }
    }
    for key in keys_to_trim {
        if let Some(v) = obj.get_mut(&key) {
            if let Some(s) = v.as_str() {
                *v = serde_json::Value::String(s.trim().to_string());
            }
        }
    }

    // 3. JSON 字符串编码的数组 → 真数组（如 `paths: '["a","b"]'` → `["a","b"]`）。
    //    当 schema 属性声明为 Array 但传了字符串时，尝试解析 JSON。
    for (key, prop_schema) in &schema.properties {
        if prop_schema.property_type != PropertyType::Array {
            continue;
        }
        if let Some(v) = obj.get(key) {
            if let Some(s) = v.as_str() {
                // 尝试解析为 JSON 数组
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                    if parsed.is_array() {
                        obj.insert(key.clone(), parsed);
                    }
                }
            }
        }
    }

    // 4. 单字符串 → 单元素数组转换（如 `paths: "src"` → `["src"]`）。
    //    当 schema 声明 Array 但传了 string 时，包成数组。
    for (key, prop_schema) in &schema.properties {
        if prop_schema.property_type != PropertyType::Array {
            continue;
        }
        if let Some(v) = obj.get(key) {
            if let Some(s) = v.as_str() {
                // 已经是 JSON 字符串编码的数组（如 '["src"]'）→ 上面第 3 步已经处理了
                // 这里处理纯字符串（如 "src"）
                let already_parsed = serde_json::from_str::<serde_json::Value>(s)
                    .map(|parsed| parsed.is_array())
                    .unwrap_or(false);
                if !already_parsed {
                    obj.insert(key.clone(), serde_json::json!([s]));
                }
            }
        }
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