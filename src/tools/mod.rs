//! Built-in tool packages mirroring `latte-ts-agent-tools/src/tools`.

pub mod file;
pub mod git;
pub mod shell;

pub use file::{FileToolsPackage, file_read_tool};
pub use git::GitToolsPackage;
pub use shell::ShellToolsPackage;

use crate::types::ToolPackage;

/// Aggregate of every built-in package.
pub fn builtin_tool_packages() -> Vec<ToolPackage> {
    vec![
        GitToolsPackage::new(),
        FileToolsPackage::new(),
        ShellToolsPackage::new(),
    ]
}
