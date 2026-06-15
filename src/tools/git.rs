//! Git version control tools. Mirrors the TS `GitToolsPackage`.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};

/// Default timeout for git commands.
const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(30);

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

async fn exec_git(args: &[String]) -> Result<String, crate::error::ToolError> {
    let output = Command::new("git")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| crate::error::ToolError::execution_str("git", format!("spawn failed: {}", e)))?;
    if !output.status.success() {
        return Err(crate::error::ToolError::execution_str(
            "git",
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn git_status_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let mut args = vec!["status".to_string()];
            if input.get("short").and_then(|v| v.as_bool()).unwrap_or(false) {
                args.push("--short".to_string());
            }
            if input.get("branch").and_then(|v| v.as_bool()).unwrap_or(false) {
                args.push("--branch".to_string());
            }
            if input.get("porcelain").and_then(|v| v.as_bool()).unwrap_or(false) {
                args.push("--porcelain".to_string());
            }
            Ok(Value::String(exec_git(&args).await?))
        }
        .boxed()
    };
    Tool::builder(
        "status",
        "获取 git 工作区状态",
        optional(vec![
            ("short", PropertyType::Boolean, "使用简短格式"),
            ("branch", PropertyType::Boolean, "显示分支信息"),
            ("porcelain", PropertyType::Boolean, "使用机器可读格式"),
        ]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(true)
    .timeout(DEFAULT_GIT_TIMEOUT)
    .build()
}

fn git_diff_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let mut args = vec!["diff".to_string()];
            if input.get("staged").and_then(|v| v.as_bool()).unwrap_or(false) {
                args.push("--staged".to_string());
            }
            if let Some(branch) = input.get("branch").and_then(|v| v.as_str()) {
                args.push(branch.to_string());
            }
            if let Some(file) = input.get("file").and_then(|v| v.as_str()) {
                args.push("--".to_string());
                args.push(file.to_string());
            }
            Ok(Value::String(exec_git(&args).await?))
        }
        .boxed()
    };
    Tool::builder(
        "diff",
        "查看 git 差异",
        optional(vec![
            ("staged", PropertyType::Boolean, "显示暂存区的差异"),
            ("file", PropertyType::String, "指定文件路径"),
            ("branch", PropertyType::String, "与指定分支比较"),
        ]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(true)
    .timeout(DEFAULT_GIT_TIMEOUT)
    .build()
}

fn git_log_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let mut args: Vec<String> = vec!["log".to_string()];
            if input.get("oneline").and_then(|v| v.as_bool()).unwrap_or(false) {
                args.push("--oneline".to_string());
            }
            if let Some(n) = input.get("n").and_then(|v| v.as_i64()) {
                args.push(n.to_string());
            }
            if input.get("follow").and_then(|v| v.as_bool()).unwrap_or(false) {
                args.push("--follow".to_string());
            }
            if let Some(file) = input.get("file").and_then(|v| v.as_str()) {
                args.push("--".to_string());
                args.push(file.to_string());
            }
            Ok(Value::String(exec_git(&args).await?))
        }
        .boxed()
    };
    Tool::builder(
        "log",
        "查看 git 提交历史",
        optional(vec![
            ("oneline", PropertyType::Boolean, "使用单行格式"),
            ("n", PropertyType::Integer, "限制显示的提交数量"),
            ("file", PropertyType::String, "指定文件路径"),
            ("follow", PropertyType::Boolean, "跟踪文件重命名"),
        ]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(true)
    .timeout(DEFAULT_GIT_TIMEOUT)
    .build()
}

fn git_branch_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let mut args = vec!["branch".to_string()];
            if input.get("list").and_then(|v| v.as_bool()).unwrap_or(false) {
                args.push("-a".to_string());
                return Ok(Value::String(exec_git(&args).await?));
            }
            if let Some(name) = input.get("create").and_then(|v| v.as_str()) {
                args.push(name.to_string());
                return Ok(Value::String(exec_git(&args).await?));
            }
            if let Some(name) = input.get("delete").and_then(|v| v.as_str()) {
                args.push("-d".to_string());
                if input.get("force").and_then(|v| v.as_bool()).unwrap_or(false) {
                    args.push("-D".to_string());
                }
                args.push(name.to_string());
                return Ok(Value::String(exec_git(&args).await?));
            }
            args.push("-a".to_string());
            Ok(Value::String(exec_git(&args).await?))
        }
        .boxed()
    };
    Tool::builder(
        "branch",
        "管理 git 分支",
        optional(vec![
            ("list", PropertyType::Boolean, "列出所有分支"),
            ("create", PropertyType::String, "创建新分支"),
            ("delete", PropertyType::String, "删除分支"),
            ("force", PropertyType::Boolean, "强制操作"),
        ]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(true)
    .timeout(DEFAULT_GIT_TIMEOUT)
    .build()
}

fn git_commit_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let message = input
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("message is required"))?;
            let mut args = vec!["commit".to_string(), "-m".to_string(), message.to_string()];
            if input.get("amend").and_then(|v| v.as_bool()).unwrap_or(false) {
                args.push("--amend".to_string());
            }
            Ok(Value::String(exec_git(&args).await?))
        }
        .boxed()
    };
    Tool::builder(
        "commit",
        "创建 git 提交",
        required(vec![("message", PropertyType::String, "提交信息")]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(DEFAULT_GIT_TIMEOUT)
    .build()
}

fn git_add_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let mut args = vec!["add".to_string()];
            let all = input.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
            if all {
                args.push("-A".to_string());
            } else if let Some(files) = input.get("files").and_then(|v| v.as_array()) {
                if !files.is_empty() {
                    args.push("--".to_string());
                    for f in files {
                        if let Some(s) = f.as_str() {
                            args.push(s.to_string());
                        }
                    }
                } else {
                    args.push(".".to_string());
                }
            } else {
                args.push(".".to_string());
            }
            Ok(Value::String(exec_git(&args).await?))
        }
        .boxed()
    };
    Tool::builder(
        "add",
        "添加文件到暂存区",
        optional(vec![
            ("files", PropertyType::Array, "要添加的文件列表"),
            ("all", PropertyType::Boolean, "添加所有更改"),
        ]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(DEFAULT_GIT_TIMEOUT)
    .build()
}

/// The `git` tool package. Mirrors `GitToolsPackage` in TS.
pub struct GitToolsPackage;

impl GitToolsPackage {
    /// Construct the package, including all 6 git tools.
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "git".into(),
            version: Some("1.0.0".into()),
            namespace: Some(crate::types::NamespaceConfig {
                prefix: "git".into(),
                separator: '.',
                auto_prefix: true,
            }),
            description: Some("Git 版本控制工具".into()),
            dependencies: None,
            tools: vec![
                git_status_tool(),
                git_diff_tool(),
                git_log_tool(),
                git_branch_tool(),
                git_commit_tool(),
                git_add_tool(),
            ],
            on_init: Some(std::sync::Arc::new(|_registry| {
                async move {
                    match Command::new("git").arg("--version").output().await {
                        Ok(out) if out.status.success() => {
                            log::info!(
                                "git available: {}",
                                String::from_utf8_lossy(&out.stdout).trim()
                            );
                        }
                        _ => log::warn!("git not available; some tools may fail"),
                    }
                }
                .boxed()
            })),
            on_destroy: None,
            before_execute: None,
            after_execute: None,
            metadata: Some(json!({ "category": "vcs", "tags": ["git", "vcs"] })),
        }
    }
}

impl Default for GitToolsPackage {
    fn default() -> Self {
        Self
    }
}
