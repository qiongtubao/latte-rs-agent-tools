//! Error types for the tool runtime.

use std::fmt;
use thiserror::Error;

/// Result alias used throughout the crate.
pub type ToolResult<T> = std::result::Result<T, ToolError>;

/// `ToolNotFound` 的可用工具后缀。空列表 → 空串（保持旧报错文本不变，
/// 老快照测试不受影响）。列表过长会截断：报错要能被模型读完，不是
/// 倒一份注册表。
fn fmt_available(available: &[String]) -> String {
    if available.is_empty() {
        return String::new();
    }
    const MAX_LISTED: usize = 24;
    let mut names: Vec<&str> = available.iter().map(|s| s.as_str()).collect();
    names.sort_unstable();
    let shown = names.len().min(MAX_LISTED);
    let mut s = format!(" — 可用工具：{}", names[..shown].join(", "));
    if names.len() > shown {
        s.push_str(&format!(" …（另有 {} 个）", names.len() - shown));
    }
    s.push_str("。请从这个列表里挑一个重试，不要再猜别的名字");
    s
}

/// Base error type. Mirrors the TS `ToolError` hierarchy.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Tool was not found in the registry.
    ///
    /// `available` 是当次注册表里真实可用的工具名。带上它是为了让调用
    /// 方（模型）能自纠：只说 "Tool not found: edit" 时，模型无从知道
    /// 正确名字是 `write`，只会换个名字继续猜。空 vec = 调用点拿不到
    /// 注册表快照，退回旧行为。
    #[error("Tool not found: {name}{}", fmt_available(available))]
    ToolNotFound {
        /// 模型请求的工具名。
        name: String,
        /// 注册表中实际可用的工具名。
        available: Vec<String>,
    },

    /// A tool with this name is already registered.
    #[error("Tool already exists: {0}")]
    ToolAlreadyExists(String),

    /// A package with this name is already registered.
    #[error("Package already registered: {0}")]
    PackageAlreadyExists(String),

    /// Package was not found in the registry.
    #[error("Package not found: {0}")]
    PackageNotFound(String),

    /// Tool execution failed (wraps the original error).
    #[error("Tool execution failed: {tool_name} (attempt {attempt}): {source_string}")]
    ToolExecution {
        /// Tool name.
        tool_name: String,
        /// Attempt number (1-based).
        attempt: u32,
        /// Display-friendly message.
        source_string: String,
        /// The original error, if any.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// Tool execution exceeded its timeout.
    #[error("Tool execution timeout: {tool_name} ({timeout_ms}ms)")]
    ToolTimeout {
        /// Tool name.
        tool_name: String,
        /// Timeout in milliseconds.
        timeout_ms: u64,
    },

    /// Missing package dependency.
    #[error("Missing dependencies for package {package_name}: {missing:?}")]
    Dependency {
        /// Package that needs them.
        package_name: String,
        /// Missing dependency names.
        missing: Vec<String>,
    },

    /// Validation failed (with structured details).
    #[error("Validation failed: {summary}")]
    Validation {
        /// Top-level message.
        summary: String,
        /// Per-field issues.
        errors: Vec<ValidationIssue>,
    },

    /// A handler reference could not be resolved.
    #[error("Failed to resolve handler for {tool_name}: {reason}")]
    HandlerResolution {
        /// Tool name.
        tool_name: String,
        /// Why resolution failed.
        reason: String,
    },

    /// Operation attempted on a destroyed manager.
    #[error("ToolManager has been destroyed")]
    ManagerDestroyed,

    /// Generic configuration error.
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// Catch-all I/O error (e.g., from a tool handler).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Catch-all serialization error.
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Catch-all unknown / user-supplied error.
    #[error("{0}")]
    Other(String),
}

/// Flattened validation issue carried inside `ToolError::Validation`.
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    /// JSON-style path.
    pub path: String,
    /// Human-readable message.
    pub message: String,
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Internal wrapper used to box various error sources uniformly.
pub struct ToolErrorSource(pub Box<dyn std::error::Error + Send + Sync>);


impl<E> From<E> for ToolErrorSource
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(e: E) -> Self {
        Self(Box::new(e))
    }
}

impl From<String> for ToolError {
    fn from(value: String) -> Self {
        Self::Other(value)
    }
}

impl From<&str> for ToolError {
    fn from(value: &str) -> Self {
        Self::Other(value.to_string())
    }
}

impl ToolError {
    /// Construct a `ToolExecution` error from an `Error + Send + Sync`.
    pub fn execution<E>(tool_name: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        let display = source.to_string();
        Self::ToolExecution {
            tool_name: tool_name.into(),
            attempt: 1,
            source_string: display,
            source: Some(Box::new(source)),
        }
    }

    /// Construct a `ToolExecution` error from a plain string message.
    pub fn execution_str(tool_name: impl Into<String>, message: impl Into<String>) -> Self {
        let msg = message.into();
        Self::ToolExecution {
            tool_name: tool_name.into(),
            attempt: 1,
            source_string: msg.clone(),
            source: Some(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                msg,
            ))),
        }
    }

    /// Construct a `ToolTimeout` error.
    pub fn timeout(tool_name: impl Into<String>, timeout: std::time::Duration) -> Self {
        Self::ToolTimeout {
            tool_name: tool_name.into(),
            timeout_ms: timeout.as_millis() as u64,
        }
    }

    /// Construct a `HandlerResolution` error.
    pub fn handler(tool_name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::HandlerResolution {
            tool_name: tool_name.into(),
            reason: reason.into(),
        }
    }

    /// Construct a `Validation` error.
    pub fn validation(summary: impl Into<String>, errors: Vec<ValidationIssue>) -> Self {
        Self::Validation {
            summary: summary.into(),
            errors,
        }
    }

    /// Construct a `Dependency` error.
    pub fn dependency(package_name: impl Into<String>, missing: Vec<String>) -> Self {
        Self::Dependency {
            package_name: package_name.into(),
            missing,
        }
    }

    /// Construct a generic `Other` error.
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    /// Tool not found.
    pub fn tool_not_found(name: impl Into<String>) -> Self {
        Self::ToolNotFound {
            name: name.into(),
            available: Vec::new(),
        }
    }

    /// Tool not found，附带注册表里真实可用的工具名供调用方自纠。
    pub fn tool_not_found_with_available(
        name: impl Into<String>,
        available: Vec<String>,
    ) -> Self {
        Self::ToolNotFound {
            name: name.into(),
            available,
        }
    }

    /// Tool already exists.
    pub fn tool_already_exists(name: impl Into<String>) -> Self {
        Self::ToolAlreadyExists(name.into())
    }

    /// Package not found.
    pub fn package_not_found(name: impl Into<String>) -> Self {
        Self::PackageNotFound(name.into())
    }

    /// Package already exists.
    pub fn package_already_exists(name: impl Into<String>) -> Self {
        Self::PackageAlreadyExists(name.into())
    }

    /// Configuration error.
    pub fn config(message: impl Into<String>) -> Self {
        Self::Configuration(message.into())
    }
}

#[cfg(test)]
mod available_tools_tests {
    use super::*;

    /// 不带 available 时报错文本保持原样（老调用点与既有断言不受影响）。
    #[test]
    fn no_available_keeps_legacy_message() {
        let err = ToolError::tool_not_found("edit");
        assert_eq!(err.to_string(), "Tool not found: edit");
    }

    /// 带 available 时列出可用工具，并明确要求从列表里挑——只报
    /// "Tool not found" 时模型只会继续猜别的名字。
    #[test]
    fn available_tools_are_listed_and_sorted() {
        let err = ToolError::tool_not_found_with_available(
            "edit",
            vec!["write".into(), "bash".into(), "read".into()],
        );
        let msg = err.to_string();
        assert!(msg.starts_with("Tool not found: edit"), "{msg}");
        assert!(msg.contains("bash, read, write"), "应排序后列出: {msg}");
        assert!(msg.contains("不要再猜别的名字"), "{msg}");
    }

    /// 列表过长要截断：报错是给模型读的，不是倒一份注册表。
    #[test]
    fn long_available_list_is_capped() {
        let many: Vec<String> = (0..80).map(|i| format!("tool_{i:02}")).collect();
        let msg = ToolError::tool_not_found_with_available("edit", many).to_string();
        assert!(msg.contains("另有 56 个"), "应标注省略数量: {msg}");
        assert!(!msg.contains("tool_79"), "尾部应被截断: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_round_trip() {
        let err = ToolError::tool_not_found("git_status");
        assert_eq!(err.to_string(), "Tool not found: git_status");
    }

    #[test]
    fn timeout_helper() {
        let err = ToolError::timeout("foo", std::time::Duration::from_millis(250));
        match err {
            ToolError::ToolTimeout {
                tool_name,
                timeout_ms,
            } => {
                assert_eq!(tool_name, "foo");
                assert_eq!(timeout_ms, 250);
            }
            _ => panic!("expected ToolTimeout"),
        }
    }
}
