//! Shell tool package. Mirrors the TS `ShellToolsPackage`.

use std::collections::BTreeMap;
use std::process::Stdio;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::process::Command;

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

fn shell_exec_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let command = input
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("command is required"))?
                .to_string();
            let cwd = input.get("cwd").and_then(|v| v.as_str()).map(String::from);
            let timeout_ms = input
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(30_000);
            let env_obj = input.get("env").and_then(|v| v.as_object()).cloned();
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&command);
            if let Some(c) = cwd.as_deref() {
                cmd.current_dir(c);
            }
            if let Some(env) = env_obj {
                for (k, v) in env {
                    if let Some(s) = v.as_str() {
                        cmd.env(k, s);
                    }
                }
            }
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            let output = tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                cmd.output(),
            )
            .await
            .map_err(|_| {
                crate::error::ToolError::timeout(
                    "shell.exec",
                    std::time::Duration::from_millis(timeout_ms),
                )
            })?
            .map_err(|e| crate::error::ToolError::execution("shell.exec", e))?;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let success = output.status.success();
            Ok(json!({
                "stdout": stdout,
                "stderr": stderr,
                "exitCode": output.status.code(),
                "success": success,
            }))
        }
        .boxed()
    };
    Tool::builder(
        "exec",
        "执行 shell 命令并返回输出",
        required(vec![("command", PropertyType::String, "要执行的命令")]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(std::time::Duration::from_secs(30))
    .build()
}

fn shell_spawn_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let command = input
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("command is required"))?
                .to_string();
            let args: Vec<String> = input
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let cwd = input.get("cwd").and_then(|v| v.as_str()).map(String::from);
            let timeout_ms = input
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(60_000);
            let stdin_payload = input
                .get("stdin")
                .and_then(|v| v.as_str())
                .map(String::from);

            let mut cmd = Command::new(&command);
            cmd.args(&args);
            if let Some(c) = cwd.as_deref() {
                cmd.current_dir(c);
            }
            cmd.stdin(Stdio::piped());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            let mut child = cmd
                .spawn()
                .map_err(|e| crate::error::ToolError::execution("shell.spawn", e))?;

            if let Some(payload) = stdin_payload.as_deref() {
                if let Some(stdin) = child.stdin.as_mut() {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(payload.as_bytes()).await;
                    let _ = stdin.shutdown().await;
                }
            }

            let output = tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                child.wait_with_output(),
            )
            .await
            .map_err(|_| {
                crate::error::ToolError::timeout(
                    "shell.spawn",
                    std::time::Duration::from_millis(timeout_ms),
                )
            })?
            .map_err(|e| crate::error::ToolError::execution("shell.spawn", e))?;

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Ok(json!({
                "stdout": stdout,
                "stderr": stderr,
                "exitCode": output.status.code(),
                "success": output.status.success(),
            }))
        }
        .boxed()
    };
    Tool::builder(
        "spawn",
        "启动进程并流式处理输出",
        required(vec![("command", PropertyType::String, "命令")]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(std::time::Duration::from_secs(60))
    .build()
}

/// The `shell` tool package. Mirrors `ShellToolsPackage` in TS.
pub struct ShellToolsPackage;

impl ShellToolsPackage {
    /// Construct the package (exec + spawn).
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "shell".into(),
            version: Some("1.0.0".into()),
            namespace: Some(crate::types::NamespaceConfig {
                prefix: "shell".into(),
                separator: '.',
                auto_prefix: true,
            }),
            description: Some("Shell 命令执行工具".into()),
            dependencies: None,
            tools: vec![shell_exec_tool(), shell_spawn_tool()],
            on_init: None,
            on_destroy: None,
            before_execute: None,
            after_execute: None,
            metadata: Some(json!({"category": "system", "tags": ["shell", "process"]})),
        }
    }
}

impl Default for ShellToolsPackage {
    fn default() -> Self {
        Self
    }
}
