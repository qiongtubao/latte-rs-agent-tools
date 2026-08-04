//! 按字段截断 args 用于错误回显。参考 oh-my-pi `truncateArgsForError`。
//!
//! 当 schema 校验失败的错误消息中包含 args 时，按字段把每个 string 字段截到 256 字符，
//! 防止大 payload（write/edit 类大内容）作为 tool_result echo 回模型时占用过多 token。
//!
//! 当前 `validate_input` 不返回 args，因此本模块当前未在错误路径上调用；
//! 预留该 helper 给未来 error path 集成（任何把 args 序列化进 tool_result 错误的位置）。

use serde_json::Value;

/// Cap per-field string length when embedding received args in an error message.
pub const MAX_ERROR_ARG_STRING_LENGTH: usize = 256;

/// 递归按字段截断所有 string 值。number/boolean/null 不动；对象递归；数组按元素处理。
pub fn truncate_args_for_error(value: &Value) -> Value {
    match value {
        Value::String(s) => {
            if s.len() <= MAX_ERROR_ARG_STRING_LENGTH {
                value.clone()
            } else {
                Value::String(format!(
                    "{}… [truncated {} chars]",
                    &s[..MAX_ERROR_ARG_STRING_LENGTH],
                    s.len() - MAX_ERROR_ARG_STRING_LENGTH,
                ))
            }
        }
        Value::Array(arr) => Value::Array(arr.iter().map(truncate_args_for_error).collect()),
        Value::Object(obj) => {
            let mut out = serde_json::Map::new();
            for (k, v) in obj {
                out.insert(k.clone(), truncate_args_for_error(v));
            }
            Value::Object(out)
        }
        // Number / bool / null: 原样返回
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn short_string_unchanged() {
        let v = json!({"path": "src/main.rs"});
        assert_eq!(truncate_args_for_error(&v), v);
    }

    #[test]
    fn long_string_truncated_with_marker() {
        let big = "x".repeat(2000);
        let v = json!({"content": big.clone()});
        let out = truncate_args_for_error(&v);
        let s = out["content"].as_str().unwrap();
        assert!(s.starts_with(&"x".repeat(256)), "前 256 字符保留");
        assert!(s.contains("[truncated 1744 chars]"), "标记截断长度");
    }

    #[test]
    fn nested_object_recursed() {
        let v = json!({
            "outer": {
                "inner": "x".repeat(1000),
                "list": ["x".repeat(1000), "short"]
            }
        });
        let out = truncate_args_for_error(&v);
        let s = out["outer"]["inner"].as_str().unwrap();
        assert!(s.contains("[truncated"));
        let arr = out["outer"]["list"].as_array().unwrap();
        assert!(arr[0].as_str().unwrap().contains("[truncated"));
        assert_eq!(arr[1].as_str().unwrap(), "short");
    }

    #[test]
    fn non_string_fields_passed_through() {
        let v = json!({"n": 42, "b": true, "missing": null, "arr": [1, 2, 3]});
        assert_eq!(truncate_args_for_error(&v), v);
    }
}
