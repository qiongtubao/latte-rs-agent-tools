//! Utility modules: logger, namespace helpers, and JSON-schema validation.

pub mod logger;
pub mod namespace;
pub mod schema_validator;

pub use logger::{create_logger, DefaultLogger, SilentLogger};
pub use namespace::{
    filter_tools_by_namespace, matches_namespace, parse_tool_name, remove_namespace,
    resolve_full_tool_name, DEFAULT_SEPARATOR,
};
pub use schema_validator::{validate_input, validate_schema, ValidationErrors};
