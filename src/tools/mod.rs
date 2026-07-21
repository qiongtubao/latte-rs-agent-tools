//! Built-in tool packages mirroring `latte-ts-agent-tools/src/tools`.

pub mod fetch;
pub mod file;
pub mod find;
pub mod git;
pub mod mcp;
pub mod playwright;
pub mod search;
pub mod shell;
pub mod todo;

pub use fetch::{http_fetch_tool, HttpToolsPackage};
pub use file::{FileToolsPackage, file_read_tool};
pub use find::file_find_tool;
pub use git::GitToolsPackage;
pub use mcp::McpToolsPackage;
pub use playwright::PlaywrightToolsPackage;
pub use search::file_search_tool;
pub use shell::ShellToolsPackage;
pub use todo::{todo_tool, TodoToolsPackage};

use crate::types::ToolPackage;

/// Aggregate of every built-in package.
pub fn builtin_tool_packages() -> Vec<ToolPackage> {
    vec![
        GitToolsPackage::new(),
        FileToolsPackage::new(),
        HttpToolsPackage::new(),
        McpToolsPackage::new(),
        PlaywrightToolsPackage::new(),
        ShellToolsPackage::new(),
        TodoToolsPackage::new(),
    ]
}