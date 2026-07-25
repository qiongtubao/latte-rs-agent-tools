//! File edit tool — apply precise text edits to files.
//!
//! 提供 `file.edit` 工具：对文件做精确的基于行号的文本替换、删除、插入。

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::fs;

use crate::error::ToolError;
use crate::types::{
    NamespaceConfig, PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema,
    ToolPackage,
};

fn prop(ty: PropertyType, description: &str) -> ToolInputProperty {
    ToolInputProperty {
        property_type: ty,
        description: Some(description.into()),
        enum_values: None,
        minimum: None,
        maximum: None,
        min_length: None,
        max_length: None,
    }
}

fn edit_schema() -> ToolInputSchema {
    let mut p = BTreeMap::new();
    p.insert("path".into(), prop(PropertyType::String, "File path to edit (required)."));
    p.insert(
        "ops".into(),
        ToolInputProperty {
            property_type: PropertyType::Array,
            description: Some("Array of edit operations.".into()),
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
        },
    );
    ToolInputSchema {
        schema_type: Default::default(),
        properties: p,
        required: Some(vec!["path".into(), "ops".into()]),
        additional_properties: None,
    }
}

fn resolve_path(path: &str, ctx: &ToolExecutionContext) -> PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Some(meta) = &ctx.metadata {
        if let Some(cwd) = meta.get("cwd").and_then(|v| v.as_str()) {
            return std::path::Path::new(cwd).join(path);
        }
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| p.to_path_buf())
}

fn split_content(content: &str) -> (Vec<&str>, bool) {
    let ends_with_newline = content.ends_with('\n');
    let lines: Vec<&str> = content.split('\n').collect();
    let effective = if ends_with_newline {
        &lines[..lines.len() - 1]
    } else {
        &lines[..]
    };
    (effective.to_vec(), ends_with_newline)
}

fn join_lines(lines: &[&str], ends_with_newline: bool) -> String {
    let mut result = lines.join("\n");
    if ends_with_newline {
        result.push('\n');
    }
    result
}

fn apply_line_op(content: &str, op: &Value) -> Result<String, ToolError> {
    let start_line = op
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .ok_or_else(|| ToolError::other("start_line is required"))?;
    if start_line < 1 {
        return Err(ToolError::other("start_line must be >= 1"));
    }

    let delete = op.get("delete").and_then(|v| v.as_bool()).unwrap_or(false);
    let insert_after = op.get("insert_after").and_then(|v| v.as_bool()).unwrap_or(false);
    let insert_before = op.get("insert_before").and_then(|v| v.as_bool()).unwrap_or(false);

    let (lines, ends_with_newline) = split_content(content);
    let total_lines = lines.len();

    if start_line > total_lines {
        if insert_after {
            let new_content = op
                .get("new_content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::other("new_content is required"))?;
            let result = if content.ends_with('\n') {
                format!("{}{}\n", content, new_content)
            } else {
                format!("{}\n{}\n", content, new_content)
            };
            return Ok(result);
        }
        return Err(ToolError::other(format!(
            "start_line {} exceeds file length {}",
            start_line, total_lines
        )));
    }

    if delete {
        let end_line = op
            .get("end_line")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(start_line);
        if end_line < start_line {
            return Err(ToolError::other("end_line must be >= start_line"));
        }
        if end_line > total_lines {
            return Err(ToolError::other(format!(
                "end_line {} exceeds file length {}",
                end_line, total_lines
            )));
        }
        let result: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(i, _)| { let n = i + 1; n < start_line || n > end_line })
            .map(|(_, l)| *l)
            .collect();
        return Ok(join_lines(&result, ends_with_newline));
    }

    if insert_after || insert_before {
        let new_content = op
            .get("new_content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::other("new_content is required"))?;
        let insert_lines: Vec<&str> = new_content.split('\n').filter(|l| !l.is_empty()).collect();
        let mut result: Vec<&str> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let line_num = i + 1;
            if insert_before && line_num == start_line {
                result.extend_from_slice(&insert_lines);
                result.push(line);
            } else {
                result.push(line);
            }
            if insert_after && line_num == start_line {
                result.extend_from_slice(&insert_lines);
            }
        }
        return Ok(join_lines(&result, ends_with_newline));
    }

    let end_line = op
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(start_line);
    if end_line < start_line {
        return Err(ToolError::other("end_line must be >= start_line"));
    }
    if end_line > total_lines {
        return Err(ToolError::other(format!(
            "end_line {} exceeds file length {}",
            end_line, total_lines
        )));
    }
    let new_content = op
        .get("new_content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::other("new_content is required"))?;
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let mut result: Vec<&str> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let line_num = i + 1;
        if line_num < start_line || line_num > end_line {
            result.push(line);
        } else if line_num == start_line {
            result.extend_from_slice(&new_lines);
        }
    }
    Ok(join_lines(&result, ends_with_newline))
}

fn apply_text_op(content: &str, op: &Value) -> Result<String, ToolError> {
    let old_text = op
        .get("old_text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::other("old_text is required"))?;
    if !content.contains(old_text) {
        return Err(ToolError::other(format!("old_text not found: {:?}", old_text)));
    }
    let delete = op.get("delete").and_then(|v| v.as_bool()).unwrap_or(false);
    if delete {
        let result = content.replace(old_text, "");
        let cleaned: Vec<&str> = result.lines().filter(|l| !l.trim().is_empty()).collect();
        return Ok(cleaned.join("\n"));
    }
    let new_text = op
        .get("new_text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::other("new_text is required"))?;
    Ok(content.replace(old_text, new_text))
}

fn file_edit_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            let path = input.get("path").and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::other("path is required"))?;
            let ops = input.get("ops").and_then(|v| v.as_array())
                .ok_or_else(|| ToolError::other("ops must be an array"))?;
            if ops.is_empty() {
                return Err(ToolError::other("ops must not be empty"));
            }
            let resolved = resolve_path(path, &ctx);
            let resolved_str = resolved.to_string_lossy().to_string();
            let content = fs::read_to_string(&resolved).await
                .map_err(|e| ToolError::other(format!("failed to read: {}", e)))?;
            let mut current = content;
            let mut changes: Vec<Value> = Vec::new();
            for (idx, op) in ops.iter().enumerate() {
                let before = current.clone();
                current = if op.get("old_text").is_some() && op.get("start_line").is_none() {
                    apply_text_op(&current, op)?
                } else if op.get("start_line").is_some() {
                    apply_line_op(&current, op)?
                } else {
                    return Err(ToolError::other(format!("op[{}]: specify start_line or old_text", idx)));
                };
                if current != before {
                    changes.push(json!({"op_index": idx}));
                }
            }
            fs::write(&resolved, &current).await
                .map_err(|e| ToolError::other(format!("failed to write: {}", e)))?;
            Ok(json!({"path": resolved_str, "success": true, "changes": changes}))
        }.boxed()
    };
    Tool::builder("edit", "Apply precise text edits to a file.", edit_schema(), std::sync::Arc::new(handler))
        .concurrency_safe(false)
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

pub struct EditToolsPackage;

impl EditToolsPackage {
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "edit".into(),
            version: Some("1.0.0".into()),
            namespace: Some(NamespaceConfig { prefix: "file".into(), separator: '.', auto_prefix: true }),
            description: Some("文件编辑工具".into()),
            dependencies: None,
            tools: vec![file_edit_tool()],
            on_init: None, on_destroy: None, before_execute: None, after_execute: None,
            metadata: Some(json!({"category": "file", "tags": ["edit", "file", "patch"]})),
        }
    }
}

impl Default for EditToolsPackage { fn default() -> Self { Self } }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::create_tool_manager;
    use crate::types::ToolManager;
    use serde_json::json;
    use tempfile::TempDir;

    async fn setup_test(content: &str) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, content).await.unwrap();
        (dir, path.to_string_lossy().to_string())
    }

    async fn run_edit(path: &str, ops: Value) -> Value {
        let m = create_tool_manager();
        m.register_package(EditToolsPackage::new()).await.unwrap();
        m.execute("file.edit", json!({"path": path, "ops": ops}), None).await.unwrap()
    }

    #[tokio::test]
    async fn test_edit_replace_single_line() {
        let (_dir, path) = setup_test("line1\nline2\nline3\n").await;
        let r = run_edit(&path, json!([{"start_line":2,"end_line":2,"new_content":"replaced"}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "line1\nreplaced\nline3\n");
    }

    #[tokio::test]
    async fn test_edit_delete_lines() {
        let (_dir, path) = setup_test("keep1\nremove\nremove\nkeep2\n").await;
        let r = run_edit(&path, json!([{"start_line":2,"end_line":3,"delete":true}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "keep1\nkeep2\n");
    }

    #[tokio::test]
    async fn test_edit_insert_after() {
        let (_dir, path) = setup_test("hello\nworld\n").await;
        let r = run_edit(&path, json!([{"start_line":1,"new_content":"inserted","insert_after":true}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "hello\ninserted\nworld\n");
    }

    #[tokio::test]
    async fn test_edit_text_replace() {
        let (_dir, path) = setup_test("fn old_name() {}\n").await;
        let r = run_edit(&path, json!([{"old_text":"old_name","new_text":"new_name"}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "fn new_name() {}\n");
    }
}