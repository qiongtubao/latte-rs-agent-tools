//! Eval tool — execute Python code in persistent sessions.
//!
//! 提供 `eval.exec` 工具：执行 Python 代码，支持持久化 REPL 会话。
//!
//! ## 用法
//!
//! 单次执行（不保留状态）：
//! ```json
//! { "code": "print(1 + 1)" }
//! ```
//!
//! 持久化会话（跨调用保持变量）：
//! ```json
//! { "code": "x = 42", "session_id": "my-session" }
//! { "code": "print(x)", "session_id": "my-session" }
//! ```
//!
//! 重置会话：
//! ```json
//! { "code": "print('fresh')", "session_id": "my-session", "reset": true }
//! ```

use std::collections::BTreeMap;
use std::sync::LazyLock;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

use crate::error::ToolError;
use crate::types::{
    NamespaceConfig, PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema,
    ToolPackage,
};

/// 全局持久化 Python 会话表。
static PY_SESSIONS: LazyLock<Mutex<BTreeMap<String, PySession>>> =
    std::sync::LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// 一个持久化 Python 会话。
struct PySession {
    /// 写入 stdin 发送代码
    stdin: ChildStdin,
    /// 子进程句柄，用于检测存活
    child: Child,
    /// 创建时间
    _created_at: std::time::Instant,
}

impl Drop for PySession {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

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

/// 构造 schema：code 必填，其他可选。
fn eval_schema() -> ToolInputSchema {
    let mut p = BTreeMap::new();
    p.insert(
        "code".into(),
        prop(PropertyType::String, "Python code to execute"),
    );
    p.insert(
        "session_id".into(),
        prop(
            PropertyType::String,
            "Session ID for persistent REPL (optional). Same ID reuses the same Python process.",
        ),
    );
    p.insert(
        "cwd".into(),
        prop(
            PropertyType::String,
            "Working directory (optional). Defaults to session cwd.",
        ),
    );
    p.insert(
        "timeout".into(),
        prop(
            PropertyType::Number,
            "Timeout in seconds (default 30, max 300).",
        ),
    );
    p.insert(
        "reset".into(),
        prop(
            PropertyType::Boolean,
            "Reset the session (kill existing process) before running.",
        ),
    );
    ToolInputSchema {
        schema_type: Default::default(),
        properties: p,
        required: Some(vec!["code".into()]),
        additional_properties: None,
    }
}

/// 默认超时（秒）。
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// 最大超时（秒）。
const MAX_TIMEOUT_SECS: u64 = 300;

/// 构造 `eval.exec` 工具。
fn eval_exec_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            // --- 1. 解析参数 ------------------------------------------------
            let code = input
                .get("code")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::other("code is required"))?
                .to_string();

            let session_id = input
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from);

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

            let timeout_secs = input
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .min(MAX_TIMEOUT_SECS);

            let reset = input
                .get("reset")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let timeout = std::time::Duration::from_secs(timeout_secs);

            if code.trim().is_empty() {
                return Ok(json!({
                    "stdout": "",
                    "stderr": "code is empty",
                    "exit_code": 1,
                    "success": false,
                }));
            }

            // --- 2. 持久化会话：管理进程 ------------------------------------
            if let Some(sid) = &session_id {
                // 如果 reset，先清理旧会话
                if reset {
                    let mut sessions = PY_SESSIONS.lock().await;
                    if let Some(mut old) = sessions.remove(sid) {
                        let _ = old.child.start_kill();
                    }
                }

                // 取或创建会话
                let mut sessions = PY_SESSIONS.lock().await;
                if let Some(session) = sessions.get_mut(sid) {
                    // 检查进程是否还活着
                    match session.child.try_wait() {
                        Ok(Some(_)) => {
                            // 进程已退出，移除并创建新会话
                            let mut removed = sessions.remove(sid).unwrap();
                            let _ = removed.child.start_kill();
                            drop(removed);
                        }
                        Ok(None) => {
                            // 进程存活，继续使用
                        }
                        Err(_) => {
                            // 出错，移除
                            sessions.remove(sid);
                        }
                    }
                }

                // 如果会话不存在，创建新会话
                if !sessions.contains_key(sid) {
                    let new_session = create_python_session(&cwd).await?;
                    sessions.insert(sid.clone(), new_session);
                }

                // 执行代码
                let result = execute_in_session(sid, &code, timeout).await?;
                return Ok(result);
            }

            // --- 3. 无会话模式：一次执行 ------------------------------------
            let result = execute_once(&code, &cwd, timeout).await?;
            Ok(result)
        }
        .boxed()
    };

    Tool::builder(
        "exec",
        "Execute Python code and return the output. Supports persistent REPL sessions (use `session_id`) that keep variables across calls. Use `reset: true` to restart a session.",
        eval_schema(),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(std::time::Duration::from_secs(MAX_TIMEOUT_SECS + 5))
    .build()
}

/// 创建新的持久化 Python 会话。
async fn create_python_session(cwd: &Option<String>) -> Result<PySession, ToolError> {
    let mut cmd = Command::new("python3");
    cmd.arg("-u") // 无缓冲输出
        .arg("-i") // 交互模式，保持进程存活
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(c) = cwd.as_deref() {
        cmd.current_dir(c);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::execution("eval.exec", e))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ToolError::other("failed to get stdin of python process"))?;

    let _created_at = std::time::Instant::now();

    Ok(PySession {
        stdin,
        child,
        _created_at,
    })
}

/// 在持久化会话中执行代码。
async fn execute_in_session(
    session_id: &str,
    code: &str,
    timeout: std::time::Duration,
) -> Result<Value, ToolError> {
    let mut sessions = PY_SESSIONS.lock().await;
    let session = sessions
        .get_mut(session_id)
        .ok_or_else(|| ToolError::other("session not found"))?;

    // 写一个标记，方便从输出中分离代码的 stdout 和 Python 的交互提示
    let marker = format!("__EVAL_RESULT_MARKER_{}__", std::process::id());
    let full_input = format!(
        "{}\nimport sys; sys.stdout.flush(); sys.stderr.flush()\ntry:\n    exec({:?})\nexcept Exception as e:\n    print(f'__EVAL_ERROR__{{e}}', file=sys.stderr)\nfinally:\n    sys.stdout.flush()\n    sys.stderr.flush()\n    print('{}')\n",
        marker, code, marker
    );

    // 写 stdin
    session
        .stdin
        .write_all(full_input.as_bytes())
        .await
        .map_err(|e| ToolError::execution("eval.exec", e))?;
    session
        .stdin
        .flush()
        .await
        .map_err(|e| ToolError::execution("eval.exec", e))?;

    // 读取输出直到遇到 marker
    let stdout = session
        .child
        .stdout
        .as_mut()
        .ok_or_else(|| ToolError::other("no stdout"))?;
    let mut reader = BufReader::new(stdout);
    let mut out_lines: Vec<String> = Vec::new();
    let mut err_lines: Vec<String> = Vec::new();
    let mut found_marker = false;

    // 读 stdout
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        let read_result = tokio::time::timeout(timeout, reader.read_line(&mut line_buf)).await;

        match read_result {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                let trimmed = line_buf.trim_end().to_string();
                if trimmed.contains(&marker) {
                    found_marker = true;
                    break;
                }
                if trimmed != ">>>" && trimmed != "..." && !trimmed.is_empty() {
                    out_lines.push(trimmed);
                }
            }
            Ok(Err(e)) => return Err(ToolError::execution("eval.exec", e)),
            Err(_) => {
                err_lines.push(format!("Timeout after {} seconds", timeout.as_secs()));
                break;
            }
        }
    }

    // 读 stderr（非阻塞，尽量读）
    let stderr = session
        .child
        .stderr
        .as_mut()
        .ok_or_else(|| ToolError::other("no stderr"))?;
    let mut err_reader = BufReader::new(stderr);
    let mut err_line = String::new();
    loop {
        err_line.clear();
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            err_reader.read_line(&mut err_line),
        )
        .await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                let trimmed = err_line.trim_end().to_string();
                if !trimmed.is_empty() {
                    err_lines.push(trimmed);
                }
            }
            _ => break,
        }
    }

    if !found_marker {
        err_lines.push(
            "Session output marker not found — process may have terminated.".into(),
        );
    }

    let stdout = out_lines.join("\n");
    let stderr = err_lines.join("\n");
    let has_error = out_lines.is_empty() && !stderr.is_empty();

    Ok(json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": if has_error { 1 } else { 0 },
        "success": !has_error,
        "session_id": session_id,
    }))
}

/// 单次执行（无持久化会话）。
async fn execute_once(
    code: &str,
    cwd: &Option<String>,
    timeout: std::time::Duration,
) -> Result<Value, ToolError> {
    let mut cmd = Command::new("python3");
    cmd.arg("-u").arg("-c").arg(code);

    if let Some(c) = cwd.as_deref() {
        cmd.current_dir(c);
    }

    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| ToolError::timeout("eval.exec", timeout))?
        .map_err(|e| ToolError::execution("eval.exec", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let success = output.status.success();

    Ok(json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": output.status.code(),
        "success": success,
        "session_id": null,
    }))
}

/// `eval` 工具包。
pub struct EvalToolsPackage;

impl EvalToolsPackage {
    /// 构造 eval 工具包。
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "eval".into(),
            version: Some("1.0.0".into()),
            namespace: Some(NamespaceConfig {
                prefix: "eval".into(),
                separator: '.',
                auto_prefix: true,
            }),
            description: Some("Python 代码执行工具：一次性执行或持久化 REPL 会话".into()),
            dependencies: None,
            tools: vec![eval_exec_tool()],
            on_init: None,
            on_destroy: None,
            before_execute: None,
            after_execute: None,
            metadata: Some(json!({"category": "code", "tags": ["python", "eval", "repl"]})),
        }
    }
}

impl Default for EvalToolsPackage {
    fn default() -> Self {
        Self
    }
}


// 单测
// =============================================================================
//
// 测试策略：调用 eval.exec 执行简单 Python 代码，验证输出正确。
// 注意：需要系统安装 python3。

#[cfg(test)]
mod tests {
    use crate::tools::EvalToolsPackage;
    use crate::types::ToolManager;
    use crate::core::create_tool_manager;
    use serde_json::json;
    use serde_json::Value;
    async fn run_eval(input: Value) -> Value {
        let manager = crate::core::create_tool_manager();
        manager
            .register_package(EvalToolsPackage::new())
            .await
            .unwrap();
        manager
            .execute("eval.exec", input, None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_eval_simple_expression() {
        // 测试：执行简单的 Python 表达式，验证 stdout 输出正确
        let result = run_eval(json!({"code": "print(1 + 2)"})).await;
        assert_eq!(result["stdout"].as_str().unwrap().trim(), "3");
        assert!(result["success"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_eval_with_error() {
        // 测试：Python 除零错误，验证返回失败但不崩溃
        let result = run_eval(json!({"code": "1/0"})).await;
        assert!(!result["success"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_eval_multi_line() {
        // 测试：多行 Python 代码（for 循环），验证每行输出正确
        let result = run_eval(json!({"code": "for i in range(3):\n    print(i)"})).await;
        let stdout = result["stdout"].as_str().unwrap();
        assert!(stdout.contains("0"), "stdout should contain 0");
        assert!(stdout.contains("1"), "stdout should contain 1");
        assert!(stdout.contains("2"), "stdout should contain 2");
    }

    #[tokio::test]
    async fn test_eval_empty_code() {
        // 测试：空代码，返回失败
        let result = run_eval(json!({"code": ""})).await;
        assert!(!result["success"].as_bool().unwrap());
        assert!(result["stderr"].as_str().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn test_eval_stdout_only() {
        // 测试：只输出到 stdout，不产生 stderr
        let result = run_eval(json!({"code": "print('hello world')"})).await;
        assert_eq!(result["stdout"].as_str().unwrap().trim(), "hello world");
        assert!(result["success"].as_bool().unwrap());
    }
}