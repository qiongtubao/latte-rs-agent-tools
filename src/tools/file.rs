//! File system tool package. Mirrors the TS `FileToolsPackage`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};
use crate::error::ToolError;

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

fn required(props: Vec<(&str, PropertyType, &str)>) -> ToolInputSchema {
    let mut p = BTreeMap::new();
    let mut req = Vec::new();
    for (name, ty, desc) in props {
        p.insert(name.to_string(), prop(ty, desc));
        req.push(name.to_string());
    }
    ToolInputSchema {
        schema_type: Default::default(),
        properties: p,
        required: Some(req),
        additional_properties: None,
    }
}

#[allow(dead_code)]
fn optional(props: Vec<(&str, PropertyType, &str)>) -> ToolInputSchema {
    let mut p = BTreeMap::new();
    for (name, ty, desc) in props {
        p.insert(name.to_string(), prop(ty, desc));
    }
    ToolInputSchema {
        schema_type: Default::default(),
        properties: p,
        required: None,
        additional_properties: None,
    }
}

fn resolve_tool_path(path: &str, ctx: &ToolExecutionContext) -> PathBuf {
    let path_buf = PathBuf::from(path);
    if path_buf.is_absolute() {
        return path_buf;
    }
    ctx.metadata
        .as_ref()
        .and_then(|m| m.get("cwd"))
        .and_then(|v| v.as_str())
        .map(|cwd| PathBuf::from(cwd).join(&path_buf))
        .unwrap_or(path_buf)
}

/// 解析 path 中的行范围选择器。
/// 格式：`:N-M`、`:raw`、`:N`、`:N+count`。
fn parse_path_selector(path: &str) -> (&str, Option<&str>) {
    if let Some(pos) = path.rfind(':') {
        let after_colon = &path[pos + 1..];
        if after_colon.starts_with('\\') {
            return (path, None);
        }
        let before = &path[..pos];
        if before.is_empty() {
            return (path, None);
        }
        if after_colon == "raw"
            || after_colon == "conflicts"
            || after_colon.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '+' || c == ',')
        {
            return (before, Some(after_colon));
        }
    }
    (path, None)
}

/// 解析行范围选择器，返回 (start, end) 1-indexed inclusive。
fn parse_line_range(sel: &str) -> Result<(usize, usize), ToolError> {
    if let Some(plus_pos) = sel.find('+') {
        let start: usize = sel[..plus_pos].parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
        let count: usize = sel[plus_pos + 1..].parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
        if start < 1 { return Err(ToolError::other("start line must be >= 1")); }
        if count < 1 { return Err(ToolError::other("count must be >= 1")); }
        return Ok((start, start + count - 1));
    }
    if let Some(dash_pos) = sel.find('-') {
        let start: usize = sel[..dash_pos].parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
        let end: usize = sel[dash_pos + 1..].parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
        if start < 1 || end < 1 { return Err(ToolError::other("line numbers must be >= 1")); }
        if end < start { return Err(ToolError::other("end line must be >= start line")); }
        return Ok((start, end));
    }
    let line: usize = sel.parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
    if line < 1 { return Err(ToolError::other("line number must be >= 1")); }
    Ok((line, line))
}

/// 列出目录内容。
async fn list_directory(dir: &std::path::Path, path_str: &str) -> Result<Value, ToolError> {
    let mut entries: Vec<String> = Vec::new();
    let mut rd = fs::read_dir(dir).await.map_err(|e| ToolError::execution_str("file.read", e.to_string()))?;
    while let Some(entry) = rd.next_entry().await.map_err(|e| ToolError::execution_str("file.read", e.to_string()))? {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().await.map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir { entries.push(format!("{}/", name)); } else { entries.push(name); }
    }
    entries.sort();
    Ok(json!({"path": path_str, "isDirectory": true, "entries": entries, "entryCount": entries.len()}))
}

/// 读取文件，支持选择器。
async fn read_file_sel(path_buf: &std::path::Path, selector: Option<&str>, max_size: u64) -> Result<Value, ToolError> {
    let meta = fs::metadata(path_buf).await.map_err(|e| ToolError::execution_str("file.read", format!("stat: {}", e)))?;
    if !meta.is_file() { return Err(ToolError::other(format!("Not a file: {}", path_buf.display()))); }
    if meta.len() > max_size { return Err(ToolError::other(format!("File too large: {} > {}", meta.len(), max_size))); }
    let bytes = fs::read(path_buf).await.map_err(|e| ToolError::execution_str("file.read", format!("read: {}", e)))?;
    let total_bytes = bytes.len() as u64;
    let content = String::from_utf8_lossy(&bytes).to_string();
    let total_lines = content.lines().count();
    let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0);

    if let Some(sel) = selector {
        if sel == "raw" {
            return Ok(json!({"content": content, "path": path_buf.to_string_lossy(), "size": total_bytes, "totalLines": total_lines, "encoding": "utf-8", "modifiedAt": modified, "selector": "raw"}));
        }
        let (start_line, end_line) = parse_line_range(sel)?;
        if start_line > total_lines { return Err(ToolError::other(format!("start_line {} exceeds file length {}", start_line, total_lines))); }
        let end = end_line.min(total_lines);
        let selected: Vec<&str> = content.lines().skip(start_line - 1).take(end - start_line + 1).collect();
        let selected_content = selected.join("\n");
        let numbered: Vec<String> = selected.iter().enumerate().map(|(i, l)| format!("{}:{}", start_line + i, l)).collect();
        return Ok(json!({"content": selected_content, "numberedContent": numbered.join("\n"), "path": path_buf.to_string_lossy(), "size": total_bytes, "totalLines": total_lines, "startLine": start_line, "endLine": end, "selectedLines": selected.len(), "encoding": "utf-8", "modifiedAt": modified, "selector": sel}));
    }
    Ok(json!({"content": content, "path": path_buf.to_string_lossy(), "size": total_bytes, "totalLines": total_lines, "encoding": "utf-8", "modifiedAt": modified}))
}

/// Standalone `file.read` tool constructor.
pub fn file_read_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            let path = input.get("path").and_then(|v| v.as_str()).ok_or_else(|| ToolError::other("path is required"))?;
            let max_size = input.get("maxSize").and_then(|v| v.as_u64()).unwrap_or(10 * 1024 * 1024);
            let (file_path, selector) = parse_path_selector(path);
            let path_buf = resolve_tool_path(file_path, &ctx);
            let meta = fs::metadata(&path_buf).await.map_err(|e| ToolError::execution_str("file.read", format!("stat: {}", e)))?;
            if meta.is_dir() { return list_directory(&path_buf, path).await; }
            let result = read_file_sel(&path_buf, selector, max_size).await?;
            Ok(result)
        }.boxed()
    };
    Tool::builder("read", "读取文件内容。支持行范围选择器：path:start-end、path:start+count、path:raw。也支持读取目录列表。", required(vec![("path", PropertyType::String, "文件路径，支持 :N-M :N+count :raw 选择器")]), std::sync::Arc::new(handler))
        .concurrency_safe(true)
        .timeout(std::time::Duration::from_secs(10))
        .build()
}

fn file_write_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("path is required"))?;
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("content is required"))?;
            let overwrite = input
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let create_dirs = input
                .get("createDirs")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            let path_buf = PathBuf::from(path);
            if !overwrite && path_buf.exists() {
                return Err(crate::error::ToolError::other(format!(
                    "File exists and overwrite=false: {}",
                    path
                )));
            }
            if create_dirs {
                if let Some(parent) = path_buf.parent() {
                    if !parent.as_os_str().is_empty() {
                        fs::create_dir_all(parent).await.map_err(|e| {
                            crate::error::ToolError::execution_str(
                                "file.write",
                                format!("create_dir_all: {}", e),
                            )
                        })?;
                    }
                }
            }
            let created = !path_buf.exists();
            let mut file = fs::File::create(&path_buf).await.map_err(|e| {
                crate::error::ToolError::execution_str("file.write", format!("create: {}", e))
            })?;
            file.write_all(content.as_bytes()).await.map_err(|e| {
                crate::error::ToolError::execution_str("file.write", format!("write: {}", e))
            })?;
            let _ = file.flush().await;
            Ok(json!({
                "success": true,
                "path": path,
                "bytesWritten": content.len(),
                "created": created,
            }))
        }
        .boxed()
    };
    Tool::builder(
        "write",
        "写入文件内容",
        required(vec![
            ("path", PropertyType::String, "文件路径"),
            ("content", PropertyType::String, "文件内容"),
        ]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(std::time::Duration::from_secs(10))
    .build()
}

fn file_list_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("path is required"))?;
            let recursive = input
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let include_hidden = input
                .get("includeHidden")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let root = resolve_tool_path(path, &ctx);
            if !root.is_dir() {
                return Err(crate::error::ToolError::other(format!(
                    "Not a directory: {}",
                    path
                )));
            }
            let mut entries = Vec::new();
            list_dir(&root, &root, recursive, include_hidden, &mut entries).await?;

            Ok(json!({
                "rootPath": path,
                "entries": entries,
                "total": entries.len(),
            }))
        }
        .boxed()
    };
    Tool::builder(
        "list",
        "列出目录内容",
        required(vec![("path", PropertyType::String, "目录路径")]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(true)
    .timeout(std::time::Duration::from_secs(30))
    .build()
}

async fn list_dir(
    dir: &Path,
    root: &Path,
    recursive: bool,
    include_hidden: bool,
    out: &mut Vec<Value>,
) -> Result<(), crate::error::ToolError> {
    let mut reader = match tokio::fs::read_dir(dir).await {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| crate::error::ToolError::execution_str("file.list", e.to_string()))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let entry_path = entry.path();
        let metadata = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let entry_type = if metadata.is_dir() {
            "directory"
        } else if metadata.is_symlink() {
            "symlink"
        } else {
            "file"
        };
        let extension = if entry_type == "file" {
            Path::new(&name)
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| format!(".{}", s))
        } else {
            None
        };
        let relative = entry_path
            .strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push(json!({
            "name": name,
            "path": entry_path.to_string_lossy().to_string(),
            "relativePath": relative,
            "type": entry_type,
            "size": metadata.len(),
            "extension": extension,
            "modifiedAt": modified,
        }));
        if recursive && entry_type == "directory" {
            let _ = Box::pin(list_dir(&entry_path, root, recursive, include_hidden, out)).await;
        }
    }
    Ok(())
}

fn file_delete_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("path is required"))?;
            let recursive = input
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let force = input
                .get("force")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let path_buf = PathBuf::from(path);
            let exists = path_buf.exists();
            if !exists {
                if force {
                    return Ok(json!({"success": true, "path": path, "wasDirectory": false}));
                }
                return Err(crate::error::ToolError::other(format!(
                    "Path does not exist: {}",
                    path
                )));
            }
            let was_dir = path_buf.is_dir();
            if was_dir && !recursive {
                return Err(crate::error::ToolError::other(format!(
                    "Path is a directory; pass recursive=true to delete: {}",
                    path
                )));
            }
            if was_dir {
                fs::remove_dir_all(&path_buf).await.map_err(|e| {
                    crate::error::ToolError::execution_str("file.delete", format!("rmdir: {}", e))
                })?;
            } else {
                fs::remove_file(&path_buf).await.map_err(|e| {
                    crate::error::ToolError::execution_str("file.delete", format!("rm: {}", e))
                })?;
            }
            Ok(json!({"success": true, "path": path, "wasDirectory": was_dir}))
        }
        .boxed()
    };
    Tool::builder(
        "delete",
        "删除文件或目录",
        required(vec![("path", PropertyType::String, "文件或目录路径")]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(std::time::Duration::from_secs(10))
    .build()
}

/// The `file` tool package. Mirrors `FileToolsPackage` in TS.
pub struct FileToolsPackage;

impl FileToolsPackage {
    /// Construct the package (5 tools: read, write, list, delete, search).
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "file".into(),
            version: Some("1.0.0".into()),
            namespace: Some(crate::types::NamespaceConfig {
                prefix: "file".into(),
                separator: '.',
                auto_prefix: true,
            }),
            description: Some("文件操作工具：读取、写入、列表、删除、搜索、查找".into()),
            dependencies: None,
            tools: vec![
                file_read_tool(),
                file_write_tool(),
                file_list_tool(),
                file_delete_tool(),
                crate::tools::search::file_search_tool(),
                crate::tools::find::file_find_tool(),
            ],
            on_init: None,
            on_destroy: None,
            before_execute: None,
            after_execute: None,
            metadata: Some(json!({"category": "filesystem", "tags": ["file", "io"]})),
        }
    }
}

impl Default for FileToolsPackage {
    fn default() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::create_tool_manager;
    use crate::types::ToolManager;
    use serde_json::json;
    use tempfile::TempDir;

    async fn setup_file(content: &str) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, content).await.unwrap();
        (dir, path.to_string_lossy().to_string())
    }

    async fn run_read(path: &str) -> Value {
        let m = create_tool_manager();
        m.register_package(FileToolsPackage::new()).await.unwrap();
        m.execute("file.read", json!({"path": path}), None).await.unwrap()
    }

    #[tokio::test]
    async fn test_read_selector_range() {
        let (_dir, path) = setup_file("line1\nline2\nline3\nline4\nline5\n").await;
        let r = run_read(&format!("{}:2-4", path)).await;
        assert_eq!(r["startLine"].as_u64().unwrap(), 2);
        assert_eq!(r["selectedLines"].as_u64().unwrap(), 3);
        assert!(r["content"].as_str().unwrap().contains("line2"));
        assert!(!r["content"].as_str().unwrap().contains("line1"));
    }

    #[tokio::test]
    async fn test_read_selector_single_line() {
        let (_dir, path) = setup_file("a\nb\nc\n").await;
        let r = run_read(&format!("{}:2", path)).await;
        assert_eq!(r["content"].as_str().unwrap(), "b");
    }

    #[tokio::test]
    async fn test_read_selector_raw() {
        let (_dir, path) = setup_file("hello\nworld\n").await;
        let r = run_read(&format!("{}:raw", path)).await;
        assert_eq!(r["selector"].as_str().unwrap(), "raw");
        assert_eq!(r["content"].as_str().unwrap(), "hello\nworld\n");
    }

    #[tokio::test]
    async fn test_read_directory() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        fs::create_dir(dir.path().join("subdir")).await.unwrap();
        fs::write(dir.path().join("f.txt"), "hi").await.unwrap();
        let r = run_read(&p).await;
        assert!(r["isDirectory"].as_bool().unwrap());
        let entries = r["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e.as_str().unwrap() == "f.txt"));
        assert!(entries.iter().any(|e| e.as_str().unwrap() == "subdir/"));
    }
}
