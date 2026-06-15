//! Script handler resolver. Mirrors the TS `ScriptHandlerResolver`.

use futures::FutureExt;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::ToolError;
use crate::types::{
    HandlerRef, HandlerResolver, ScriptHandlerRef, SharedToolHandler, ToolExecutionContext,
};

/// Executes tools via shell commands.
pub struct ScriptHandlerResolver {
    default_timeout: Duration,
}

impl ScriptHandlerResolver {
    /// Construct a new resolver.
    pub fn new(default_timeout: Duration) -> Self {
        Self { default_timeout }
    }

    /// Build the concrete handler for a script `HandlerRef`.
    pub async fn resolve(
        &self,
        reference: &HandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, ToolError> {
        let script_ref = match reference {
            HandlerRef::Script(r) => r.clone(),
            other => {
                return Err(ToolError::handler(
                    tool_name,
                    format!("Not a script handler reference: {:?}", other),
                ));
            }
        };
        let handler = build_script_handler(script_ref, tool_name.to_string());
        Ok(Arc::new(handler))
    }
}

impl std::fmt::Debug for ScriptHandlerResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptHandlerResolver")
            .field("default_timeout", &self.default_timeout)
            .finish()
    }
}

fn build_script_handler(
    ref_: ScriptHandlerRef,
    tool_name: String,
) -> impl Fn(serde_json::Value, ToolExecutionContext) -> futures::future::BoxFuture<'static, Result<serde_json::Value, ToolError>>
       + Send
       + Sync
       + 'static {
    move |input: serde_json::Value, _ctx: ToolExecutionContext| {
        let ref_ = ref_.clone();
        let tool_name = tool_name.clone();
        (async move { execute_script(&ref_, &input, &tool_name).await }).boxed()
    }
}

async fn execute_script(
    ref_: &ScriptHandlerRef,
    input: &serde_json::Value,
    tool_name: &str,
) -> Result<serde_json::Value, ToolError> {
    let mut command = Command::new(&ref_.command);
    if let Some(args) = &ref_.args {
        command.args(args);
    }
    if let Some(cwd) = &ref_.cwd {
        command.current_dir(cwd);
    }
    if let Some(env) = &ref_.env {
        for (k, v) in env {
            command.env(k, v);
        }
    }

    if ref_.stdin_json {
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|e| {
            ToolError::execution_str(tool_name, format!("Failed to spawn: {}", e))
        })?;

        if let Some(stdin) = child.stdin.as_mut() {
            let payload = serde_json::to_vec(input).map_err(|e| {
                ToolError::execution_str(tool_name, format!("Failed to serialize input: {}", e))
            })?;
            stdin.write_all(&payload).await.map_err(|e| {
                ToolError::execution_str(tool_name, format!("Failed to write stdin: {}", e))
            })?;
        }

        let output = child.wait_with_output().await.map_err(|e| {
            ToolError::execution_str(tool_name, format!("Failed to wait: {}", e))
        })?;

        if !output.status.success() {
            return Err(ToolError::execution_str(
                tool_name,
                format!(
                    "Exit code {:?}: {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                ),
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if ref_.parse_json {
            serde_json::from_str(&stdout).map_err(|e| {
                ToolError::execution_str(
                    tool_name,
                    format!("Failed to parse JSON output: {}: {}", e, stdout),
                )
            })
        } else {
            Ok(serde_json::Value::String(stdout))
        }
    } else {
        // Use simple exec with arguments; append input as trailing args if any.
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let output = command.output().await.map_err(|e| {
            ToolError::execution_str(tool_name, format!("Failed to execute: {}", e))
        })?;

        if !output.status.success() {
            return Err(ToolError::execution_str(
                tool_name,
                format!(
                    "Exit code {:?}: {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                ),
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if ref_.parse_json {
            serde_json::from_str(&stdout).map_err(|e| {
                ToolError::execution_str(
                    tool_name,
                    format!("Failed to parse JSON output: {}: {}", e, stdout),
                )
            })
        } else {
            Ok(serde_json::Value::String(stdout))
        }
    }
}

#[async_trait]
impl HandlerResolver for ScriptHandlerResolver {
    async fn resolve(
        &self,
        reference: &HandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, ToolError> {
        ScriptHandlerResolver::resolve(self, reference, tool_name).await
    }
}
