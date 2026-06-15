//! # latte-rs-agent-tools
//!
//! Tool management implementation for AI agents — port of
//! [`latte-ts-agent-tools`](../latte-ts-agent-tools/README.md) to Rust.
//!
//! Provides a complete system for registering, executing, and managing tools
//! with lifecycle hooks, validation, and plugin packages.
//!
//! ## Architecture
//!
//! ```text
//! ToolManager  (orchestrator)
//!     │
//!     ├── ToolRegistry    (storage / namespace / conflict resolution)
//!     ├── HookManager     (lifecycle events: before/after/error/...)
//!     ├── HandlerResolver (builtin / http / script / composite)
//!     └── ConfigValidator (JSON config validation)
//! ```
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use latte_rs_agent_tools::prelude::*;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let manager = create_tool_manager();
//!
//! // Register a package
//! manager.register_package(GitToolsPackage::new()).await?;
//!
//! // Execute a tool
//! let result = manager
//!     .execute("git.status", serde_json::json!({ "short": true }), None)
//!     .await?;
//! println!("{}", result);
//! # Ok(())
//! # }
//! ```

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod types;
pub mod error;
pub mod utils;
pub mod resolvers;
pub mod core;
pub mod tools;

#[cfg(test)]
mod tests;

/// Convenience re-exports for the most common types and constructors.
pub mod prelude {
    pub use crate::core::{
        create_tool_manager, ToolManagerFactory, tool_manager_factory_create, ConfigValidatorImpl, HookManagerImpl,
        ToolManagerImpl, ToolRegistryImpl,
    };
    pub use crate::error::{ToolError, ToolResult};
    pub use crate::resolvers::{
        BuiltinHandlerResolver, CompositeHandlerResolver, HttpHandlerResolver,
        ScriptHandlerResolver,
    };
    pub use crate::tools::{
        builtin_tool_packages, file_read_tool, FileToolsPackage, GitToolsPackage,
        ShellToolsPackage,
    };
    pub use crate::types::*;
    pub use crate::utils::{
        create_logger, parse_tool_name, resolve_full_tool_name, validate_input, DefaultLogger,
        SilentLogger, DEFAULT_SEPARATOR,
    };
}
