//! File system tool package. Mirrors the TS `FileToolsPackage`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};

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

/// Standalone `file.read` tool constructor.
pub fn file_read_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("path is required"))?;
            let max_size = input
                .get("maxSize")
                .and_then(|v| v.as_u64())
                .unwrap_or(10 * 1024 * 1024);
            let metadata = fs::metadata(path)
                .await
                .map_err(|e| crate::error::ToolError::execution_str("file.read", format!("stat: {}", e)))?;
            if !metadata.is_file() {
                return Err(crate::error::ToolError::other(format!("Not a file: {}", path)));
            }
            if metadata.len() > max_size {
                return Err(crate::error::ToolError::other(format!(
                    "File too large: {} > {}",
                    metadata.len(),
                    max_size
                )));
            }
            let bytes = fs::read(path).await.map_err(|e| {
                crate::error::ToolError::execution_str("file.read", format!("read: {}", e))
            })?;
            let content = String::from_utf8_lossy(&bytes).to_string();
            let size = bytes.len() as u64;
            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Ok(json!({
                "content": content,
                "path": path,
                "size": size,
                "encoding": "utf-8",
                "modifiedAt": modified,
            }))
        }
        .boxed()
    };
    Tool::builder(
        "read",
        "读取文件内容",
        required(vec![("path", PropertyType::String, "文件路径")]),
        std::sync::Arc::new(handler),
    )
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
            let include_hidden = input
                .get("includeHidden")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let root = PathBuf::from(path);
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

fn file_search_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("path is required"))?;
            let pattern = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("pattern is required"))?;
            let file_pattern = input.get("filePattern").and_then(|v| v.as_str());
            let ignore_case = input
                .get("ignoreCase")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let max_depth = input
                .get("maxDepth")
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as usize;

            let re_pattern = if ignore_case {
                format!("(?i){}", pattern)
            } else {
                pattern.to_string()
            };
            let re = regex::Regex::new(&re_pattern)
                .map_err(|e| crate::error::ToolError::other(format!("invalid regex: {}", e)))?;
            let glob_re = file_pattern.map(|p| {
                let escaped = p
                    .replace('.', "\\.")
                    .replace('*', ".*")
                    .replace('?', ".");
                format!("^{}$", escaped)
            });
            let glob = glob_re
                .as_ref()
                .and_then(|s| regex::Regex::new(s).ok());

            let root = PathBuf::from(path);
            let mut files: Vec<PathBuf> = Vec::new();
            collect_files(&root, max_depth, 0, &mut files);
            if let Some(glob) = glob.as_ref() {
                files.retain(|f| {
                    f.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| glob.is_match(n))
                        .unwrap_or(false)
                });
            }

            let mut matches: Vec<Value> = Vec::new();
            for f in &files {
                let content = match tokio::fs::read_to_string(f).await {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        matches.push(json!({
                            "file": f.to_string_lossy().to_string(),
                            "line": i + 1,
                            "content": line,
                        }));
                    }
                }
            }
            Ok(json!({
                "pattern": pattern,
                "searchPath": path,
                "matches": matches,
                "totalMatches": matches.len(),
                "filesSearched": files.len(),
            }))
        }
        .boxed()
    };
    Tool::builder(
        "search",
        "在文件中搜索内容",
        required(vec![
            ("pattern", PropertyType::String, "搜索模式"),
            ("path", PropertyType::String, "搜索路径"),
        ]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(true)
    .timeout(std::time::Duration::from_secs(60))
    .build()
}

fn collect_files(dir: &Path, max_depth: usize, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            collect_files(&path, max_depth, depth + 1, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
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
            description: Some("文件操作工具：读取、写入、列表、删除、搜索".into()),
            dependencies: None,
            tools: vec![
                file_read_tool(),
                file_write_tool(),
                file_list_tool(),
                file_delete_tool(),
                file_search_tool(),
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
