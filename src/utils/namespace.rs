//! Namespace helpers for tool names. Mirrors `latte-ts-agent-tools/src/utils/namespace.ts`.

use crate::types::{NamespaceConfig, NamespaceSeparator, DEFAULT_NAMESPACE_SEPARATOR};

/// Default separator (`.`).
pub const DEFAULT_SEPARATOR: NamespaceSeparator = DEFAULT_NAMESPACE_SEPARATOR;

/// Apply namespace configuration to a tool name.
///
/// - `None` → return the name unchanged.
/// - `Some(true)` → use the package name as a prefix (caller supplies it).
/// - `Some(NamespaceConfig { auto_prefix: false })` → return the name unchanged.
/// - `Some(NamespaceConfig { auto_prefix: true })` → prepend the prefix unless
///   the name already starts with the prefix.
pub fn resolve_full_tool_name(tool_name: &str, namespace: Option<&NamespaceConfig>) -> String {
    let Some(ns) = namespace else { return tool_name.to_string() };
    if !ns.auto_prefix {
        return tool_name.to_string();
    }
    let separator = ns.separator;
    let prefix_with_sep = format!("{}{}", ns.prefix, separator);
    if tool_name.starts_with(&prefix_with_sep) {
        return tool_name.to_string();
    }
    format!("{}{}{}", ns.prefix, separator, tool_name)
}

/// Parse a fully-qualified tool name into its namespace and original name.
pub fn parse_tool_name(
    full_tool_name: &str,
    separator: NamespaceSeparator,
) -> ParsedToolName {
    let parts: Vec<&str> = full_tool_name.split(separator).collect();
    if parts.len() == 1 {
        return ParsedToolName {
            namespace: None,
            original_name: full_tool_name.to_string(),
        };
    }
    ParsedToolName {
        namespace: Some(parts[0].to_string()),
        original_name: parts[1..].join(&separator.to_string()),
    }
}

/// Parsed view of a tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToolName {
    /// First component, if there is one.
    pub namespace: Option<String>,
    /// The remainder of the name (still possibly containing the separator).
    pub original_name: String,
}

/// Return `true` if `tool_name` lives in `namespace`.
pub fn matches_namespace(
    tool_name: &str,
    namespace: &str,
    separator: NamespaceSeparator,
) -> bool {
    tool_name.starts_with(&format!("{}{}", namespace, separator))
}

/// Filter a list of tool names down to those in `namespace`.
pub fn filter_tools_by_namespace(
    tool_names: &[String],
    namespace: &str,
    separator: NamespaceSeparator,
) -> Vec<String> {
    tool_names
        .iter()
        .filter(|n| matches_namespace(n, namespace, separator))
        .cloned()
        .collect()
}

/// Strip the namespace prefix from a fully-qualified tool name.
pub fn remove_namespace(tool_name: &str, separator: NamespaceSeparator) -> String {
    parse_tool_name(tool_name, separator).original_name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_with_namespace() {
        let ns = NamespaceConfig {
            prefix: "git".into(),
            separator: '.',
            auto_prefix: true,
        };
        assert_eq!(resolve_full_tool_name("status", Some(&ns)), "git.status");
        assert_eq!(resolve_full_tool_name("git.status", Some(&ns)), "git.status");
    }

    #[test]
    fn resolve_without_namespace() {
        assert_eq!(resolve_full_tool_name("status", None), "status");
    }

    #[test]
    fn resolve_with_disabled_prefix() {
        let ns = NamespaceConfig {
            prefix: "git".into(),
            separator: '.',
            auto_prefix: false,
        };
        assert_eq!(resolve_full_tool_name("status", Some(&ns)), "status");
    }

    #[test]
    fn parse_simple() {
        let p = parse_tool_name("git.status", '.');
        assert_eq!(p.namespace.as_deref(), Some("git"));
        assert_eq!(p.original_name, "status");
    }

    #[test]
    fn parse_no_namespace() {
        let p = parse_tool_name("status", '.');
        assert_eq!(p.namespace, None);
        assert_eq!(p.original_name, "status");
    }

    #[test]
    fn parse_multi_segment_namespace() {
        let p = parse_tool_name("git.git.status", '.');
        assert_eq!(p.namespace.as_deref(), Some("git"));
        assert_eq!(p.original_name, "git.status");
    }

    #[test]
    fn matches_and_remove() {
        assert!(matches_namespace("git.status", "git", '.'));
        assert!(!matches_namespace("file.read", "git", '.'));
        assert_eq!(remove_namespace("git.status", '.'), "status");
    }
}
