//! Logger implementations: a default logger that prints to stderr/stdout, and
//! a no-op silent logger. Mirrors the TS `DefaultLogger` and `SilentLogger`.

use std::sync::Arc;

use crate::types::ToolLogger;

/// Default logger that writes timestamped, prefixed messages to the console.
#[derive(Debug, Clone)]
pub struct DefaultLogger {
    /// Prefix prepended to every log line (e.g. `"ToolManager"`).
    pub prefix: String,
}

impl DefaultLogger {
    /// Construct a new default logger with the given prefix.
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }
}

impl Default for DefaultLogger {
    fn default() -> Self {
        Self::new("ToolManager")
    }
}

impl ToolLogger for DefaultLogger {
    fn debug(&self, message: &str, args: &[serde_json::Value]) {
        eprintln!("[{}:debug] {}", self.prefix, format_msg(message, args));
    }
    fn info(&self, message: &str, args: &[serde_json::Value]) {
        eprintln!("[{}:info] {}", self.prefix, format_msg(message, args));
    }
    fn warn(&self, message: &str, args: &[serde_json::Value]) {
        eprintln!("[{}:warn] {}", self.prefix, format_msg(message, args));
    }
    fn error(&self, message: &str, args: &[serde_json::Value]) {
        eprintln!("[{}:error] {}", self.prefix, format_msg(message, args));
    }
}

/// No-op logger; discards every message.
#[derive(Debug, Clone, Default)]
pub struct SilentLogger;

impl ToolLogger for SilentLogger {}

fn format_msg(message: &str, args: &[serde_json::Value]) -> String {
    if args.is_empty() {
        return message.to_string();
    }
    let parts: Vec<String> = args.iter().map(json_compact).collect();
    format!("{} {}", message, parts.join(" "))
}

fn json_compact(v: &serde_json::Value) -> String {
    v.to_string()
}

/// Create a logger from the standard options block.
pub fn create_logger(options: Option<LoggerOptions>) -> Arc<dyn ToolLogger> {
    match options {
        Some(opts) if opts.silent => Arc::new(SilentLogger),
        Some(opts) => Arc::new(DefaultLogger::new(opts.prefix.unwrap_or_else(|| "ToolManager".into()))),
        None => Arc::new(DefaultLogger::default()),
    }
}

/// Options for [`create_logger`].
#[derive(Debug, Clone, Default)]
pub struct LoggerOptions {
    /// Prefix for log lines. Ignored when `silent` is `true`.
    pub prefix: Option<String>,
    /// When `true`, returns a `SilentLogger`.
    pub silent: bool,
}
