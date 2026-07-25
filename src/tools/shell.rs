//! Shell tool package. Mirrors the TS `ShellToolsPackage`.

use std::collections::BTreeMap;
use std::process::Stdio;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::process::Command;
use std::sync::LazyLock;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

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

/// Build a schema with required + optional fields. Required fields are
/// listed in `properties` AND in `required`; optional fields are only
/// in `properties`. The LLM may omit optional fields — handlers fall
/// back to defaults or the `ToolExecutionContext` (e.g. `ctx.metadata.cwd`).
fn schema_with_optional(
    required: Vec<(&str, PropertyType, &str)>,
    optional: Vec<(&str, PropertyType, &str)>,
) -> ToolInputSchema {
    let mut p = BTreeMap::new();
    let mut req = Vec::new();
    for (name, ty, desc) in required {
        p.insert(name.to_string(), prop(ty, desc));
        req.push(name.to_string());
    }
    for (name, ty, desc) in optional {
        // Skip if a required field already claimed the name.
        if !p.contains_key(name) {
            p.insert(name.to_string(), prop(ty, desc));
        }
    }
    ToolInputSchema {
        schema_type: Default::default(),
        properties: p,
        required: Some(req),
        additional_properties: None,
    }
}

/// Backwards-compatible schema builder for tools that only need required
/// fields. New tools should prefer `schema_with_optional`.
fn required(props: Vec<(&str, PropertyType, &str)>) -> ToolInputSchema {
    schema_with_optional(props, Vec::new())
}

/// Default wall-clock timeout for `shell.exec`, in milliseconds. Mirrors the
/// bash tool's old 30s default — overridden by the schema's `timeout` field if
/// the model asks for a longer cap, or by the env var
/// `LATTE_AGENT_BASH_TIMEOUT_SECS` for operator-level caps.
/// L1 — single-tool-call default wall-clock timeout (e.g. one `bash`
/// invocation). 300s matches oh-my-pi's `bash.ts:106` default of 5min,
/// which gives deepseek-v4-flash enough budget for long-running git
/// grep / find across large trees without losing the result to a
/// premature timeout. Overridable per-call via the schema's `timeout`
/// field (model can ask for less); operator-level cap via the env var
/// `LATTE_AGENT_BASH_TIMEOUT_SECS`.
fn default_exec_timeout_ms() -> u64 {
    let from_env = std::env::var("LATTE_AGENT_BASH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    from_env.unwrap_or(300_000) // L1 = 300s (5 minutes), aligned with oh-my-pi bash.ts
}

/// Strip a leading `cd <path> && ` from `command` when no explicit `cwd`
/// argument was passed. Mirrors oh-my-pi's bash.ts:690-702. We extract the
/// path into `cwd` so the model can write inline `cd X && cmd` without the
/// shell-resolved cwd differing from the model-intended cwd (which can happen
/// when the path needs `~` expansion or contains shell metacharacters).
///
/// We refuse extraction when the path uses `$VAR`, `$(...)`, or backticks —
/// the shell handles those at runtime and `current_dir` wouldn't.
fn extract_cd_prefix(command: &str) -> Option<(&str, &str)> {
    // Look for `cd <path> && <rest>` at the very start. The path runs up to
    // the first `&&` and may not contain `&`, `\\`, `\n`, `\r` (single-line
    // constraint, matches oh-my-pi).
    let leading = command
        .strip_prefix("cd ")
        .or_else(|| command.strip_prefix("cd\t"))
        ?;
    let (path, rest) = leading.split_once("&&")?;
    let path = path.trim();
    if path.is_empty() || path.contains(['&', '\\', '\n', '\r']) {
        return None;
    }
    if path.contains(['$', '`', '(']) {
        return None;
    }
    // Strip surrounding quotes.
    let path = path
        .strip_prefix('"')
        .and_then(|p| p.strip_suffix('"'))
        .or_else(|| path.strip_prefix('\'').and_then(|p| p.strip_suffix('\'')))
        .unwrap_or(path);
    let rest = rest.trim_start();
    Some((path, rest))
}


static BG_JOBS: LazyLock<Mutex<BTreeMap<String, tokio::process::Child>>> =
    std::sync::LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// 后台 job 计数器。
static BG_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn shell_exec_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            let original = input
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("command is required"))?
                .to_string();
            // Resolve cwd in priority order:
            //   1. `input.cwd` (explicit) — wins outright
            //   2. leading `cd X &&` extract from command — sets cwd AND strips
            //      the prefix so we don't double-run cd inside `sh -c`
            //   3. `ctx.metadata.cwd` (runner-provided fallback)
            let (cwd_from_param, command) = if let Some(p) = input.get("cwd").and_then(|v| v.as_str()) {
                (Some(p.to_string()), original)
            } else if let Some((p, rest)) = extract_cd_prefix(&original) {
                (Some(p.to_string()), rest.to_string())
            } else {
                (None, original)
            };
            let cwd = cwd_from_param.or_else(|| {
                ctx.metadata
                    .as_ref()
                    .and_then(|m| m.get("cwd"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });
            let timeout_ms = input
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or_else(default_exec_timeout_ms);
            let env_obj = input.get("env").and_then(|v| v.as_object()).cloned();
            let stdin_payload = input.get("stdin").and_then(|v| v.as_str()).map(String::from);
            let is_async = input.get("async").and_then(|v| v.as_bool()).unwrap_or(false);

            // --- 后台执行模式 --------------------------------------------------
            if is_async {
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
                cmd.stdin(Stdio::piped());
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());

                let mut child = cmd
                    .spawn()
                    .map_err(|e| crate::error::ToolError::execution("shell.exec", e))?;

                // 写 stdin
                if let Some(payload) = stdin_payload {
                    if let Some(stdin) = child.stdin.as_mut() {
                        let _ = stdin.write_all(payload.as_bytes()).await;
                        let _ = stdin.shutdown().await;
                    }
                }

                let job_id = BG_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let job_key = format!("bg-{}", job_id);
                let mut jobs = BG_JOBS.lock().await;
                jobs.insert(job_key.clone(), child);

                return Ok(json!({
                    "jobId": job_key,
                    "status": "running",
                    "message": format!("Background job {} started. Use shell.exec with `jobId: \"{}\"` and `wait: true` to get the result.", job_key, job_key),
                }));
            }

            // --- 等待后台 job --------------------------------------------------
            let is_wait = input.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);
            if is_wait {
                let job_id = input
                    .get("jobId")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| crate::error::ToolError::other("jobId is required when wait=true"))?;

                let mut jobs = BG_JOBS.lock().await;
                let mut child = jobs
                    .remove(job_id)
                    .ok_or_else(|| crate::error::ToolError::other(format!("Job not found: {}", job_id)))?;
                drop(jobs);

                let output = tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    child.wait_with_output(),
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
                return Ok(json!({
                    "stdout": stdout,
                    "stderr": stderr,
                    "exitCode": output.status.code(),
                    "success": success,
                    "jobId": job_id,
                }));
            }

            // --- 前台执行模式（默认）-------------------------------------------
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
            cmd.stdin(Stdio::piped());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            let mut child = cmd
                .spawn()
                .map_err(|e| crate::error::ToolError::execution("shell.exec", e))?;

            // 写 stdin
            if let Some(payload) = stdin_payload {
                if let Some(stdin) = child.stdin.as_mut() {
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
        "执行 shell 命令并返回输出。支持前台执行（默认）和后台执行（`async: true` 返回 jobId，之后用 `wait: true` + `jobId` 取结果）。支持 `stdin` 输入。支持 `cwd`、`timeout`、`env` 参数。",
        schema_with_optional(
            vec![("command", PropertyType::String, "要执行的命令")],
            vec![
                ("cwd", PropertyType::String, "工作目录。可选。如果 command 以 `cd X &&` 开头，会被自动提取。"),
                ("timeout", PropertyType::Number, "超时（秒），默认 300s (L1)。命令超过这个时间会被 kill 掉。"),
                ("env", PropertyType::Object, "额外的环境变量（key=value 字符串映射）"),
                ("stdin", PropertyType::String, "标准输入内容（可选）"),
                ("async", PropertyType::Boolean, "后台执行模式。设为 true 立即返回 jobId，不等待命令完成。"),
                ("wait", PropertyType::Boolean, "等待后台 job 完成。需要配合 `jobId` 使用。"),
                ("jobId", PropertyType::String, "后台 job ID。wait=true 时必填，指定要等待的 job。"),
            ],
        ),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(std::time::Duration::from_secs(300))
    .build()
}

fn shell_spawn_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
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
            let cwd = input
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| {
                    ctx.metadata
                        .as_ref()
                        .and_then(|m| m.get("cwd"))
                        .and_then(|v| v.as_str())
                        .map(String::from)
                });
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
