//! Error types for the tool runtime.

use std::fmt;
use thiserror::Error;

/// Result alias used throughout the crate.
pub type ToolResult<T> = std::result::Result<T, ToolError>;

/// Base error type. Mirrors the TS `ToolError` hierarchy.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Tool was not found in the registry.
    #[error("Tool not found: {0}")]
    ToolNotFound(String),

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
        Self::ToolNotFound(name.into())
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
mod tests {
    use super::*;

    #[test]
    fn display_round_trip() {
        let err = ToolError::tool_not_found("git.status");
        assert_eq!(err.to_string(), "Tool not found: git.status");
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
