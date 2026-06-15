//! Config validator. Mirrors `latte-ts-agent-tools/src/core/config-validator.ts`.

use std::collections::HashSet;

use crate::types::{
    ConflictStrategy, HandlerRef, NamespaceConfigOrBool, SerializedHandler,
    ToolManagerSerializedConfig, ToolPackageConfig, ValidationError, ValidationResult,
    ValidationSeverity,
};

/// Validates `ToolManagerSerializedConfig` instances.
pub struct ConfigValidatorImpl;

impl ConfigValidatorImpl {
    /// Construct a new validator.
    pub fn new() -> Self {
        Self
    }

    /// Validate a full config. Returns a structured `ValidationResult`.
    pub fn validate(&self, config: &ToolManagerSerializedConfig) -> ValidationResult {
        let mut errors: Vec<ValidationError> = Vec::new();
        let mut tool_count = 0usize;
        let package_count = config.packages.len();

        if config.version.is_empty() {
            errors.push(ValidationError {
                path: "version".to_string(),
                message: "Missing version field".to_string(),
                severity: ValidationSeverity::Error,
                code: Some("MISSING_VERSION".to_string()),
                suggestion: None,
            });
        }

        // Manager config validation
        if let Some(mc) = config.config.as_ref() {
            if let Some(t) = mc.default_timeout {
                if t == 0 {
                    errors.push(ValidationError {
                        path: "config.defaultTimeout".to_string(),
                        message: "defaultTimeout must be a positive number".to_string(),
                        severity: ValidationSeverity::Error,
                        code: Some("INVALID_TIMEOUT".to_string()),
                        suggestion: None,
                    });
                }
            }
            if let Some(s) = mc.conflict_strategy {
                let _ = ConflictStrategy::Error == s; // accepted; nothing to check beyond enum constraint
            }
        }

        // Package-level validation
        let mut seen_pkg_names: HashSet<String> = HashSet::new();
        for (i, pkg) in config.packages.iter().enumerate() {
            self.validate_package(pkg, &format!("packages[{}]", i), &mut errors);
            if !seen_pkg_names.insert(pkg.name.clone()) {
                errors.push(ValidationError {
                    path: "packages".to_string(),
                    message: format!("Duplicate package name: {}", pkg.name),
                    severity: ValidationSeverity::Error,
                    code: Some("DUPLICATE_PACKAGE".to_string()),
                    suggestion: None,
                });
            }
            tool_count += pkg.tools.len();
        }

        // Standalone tools
        if let Some(tools) = config.standalone_tools.as_ref() {
            for (i, t) in tools.iter().enumerate() {
                self.validate_tool(t, &format!("standaloneTools[{}]", i), &mut errors);
                tool_count += 1;
            }
        }

        ValidationResult::summarize(errors, tool_count, package_count)
    }

    /// Validate a single package. Pushes errors into `out`.
    pub fn validate_package(
        &self,
        pkg: &ToolPackageConfig,
        base_path: &str,
        out: &mut Vec<ValidationError>,
    ) {
        if pkg.name.is_empty() {
            out.push(ValidationError {
                path: format!("{}.name", base_path),
                message: "Package name is required".to_string(),
                severity: ValidationSeverity::Error,
                code: Some("MISSING_PACKAGE_NAME".to_string()),
                suggestion: None,
            });
        }
        if pkg.tools.is_empty() {
            out.push(ValidationError {
                path: format!("{}.tools", base_path),
                message: "Package must have at least one tool".to_string(),
                severity: ValidationSeverity::Error,
                code: Some("EMPTY_PACKAGE_TOOLS".to_string()),
                suggestion: None,
            });
        } else {
            let mut seen: HashSet<String> = HashSet::new();
            for (i, t) in pkg.tools.iter().enumerate() {
                self.validate_tool(t, &format!("{}.tools[{}]", base_path, i), out);
                if !seen.insert(t.name.clone()) {
                    out.push(ValidationError {
                        path: format!("{}.tools", base_path),
                        message: format!("Duplicate tool name in package: {}", t.name),
                        severity: ValidationSeverity::Error,
                        code: Some("DUPLICATE_TOOL".to_string()),
                        suggestion: None,
                    });
                }
            }
        }

        if let Some(NamespaceConfigOrBool::Object(ns)) = pkg.namespace.as_ref() {
            if ns.prefix.is_empty() {
                out.push(ValidationError {
                    path: format!("{}.namespace.prefix", base_path),
                    message: "Namespace prefix is required when namespace is an object".to_string(),
                    severity: ValidationSeverity::Error,
                    code: Some("MISSING_NAMESPACE_PREFIX".to_string()),
                    suggestion: None,
                });
            }
        }
    }

    /// Validate a single tool config. Pushes errors into `out`.
    pub fn validate_tool(
        &self,
        tool: &crate::types::ToolConfig,
        base_path: &str,
        out: &mut Vec<ValidationError>,
    ) {
        if tool.name.is_empty() {
            out.push(ValidationError {
                path: format!("{}.name", base_path),
                message: "Tool name is required".to_string(),
                severity: ValidationSeverity::Error,
                code: Some("MISSING_TOOL_NAME".to_string()),
                suggestion: None,
            });
        }
        if tool.description.is_empty() {
            out.push(ValidationError {
                path: format!("{}.description", base_path),
                message: "Tool description is required".to_string(),
                severity: ValidationSeverity::Error,
                code: Some("MISSING_TOOL_DESCRIPTION".to_string()),
                suggestion: None,
            });
        }
        if tool.input_schema.properties.is_empty() {
            out.push(ValidationError {
                path: format!("{}.input_schema.properties", base_path),
                message: "input_schema must have at least one property".to_string(),
                severity: ValidationSeverity::Warning,
                code: Some("EMPTY_PROPERTIES".to_string()),
                suggestion: None,
            });
        }
        match &tool.handler {
            SerializedHandler::Name(s) if s.is_empty() => {
                out.push(ValidationError {
                    path: format!("{}.handler", base_path),
                    message: "Handler name is required".to_string(),
                    severity: ValidationSeverity::Error,
                    code: Some("MISSING_HANDLER_NAME".to_string()),
                    suggestion: None,
                });
            }
            SerializedHandler::Name(_) => {}
            SerializedHandler::Ref(r) => {
                self.validate_handler_ref(r, &format!("{}.handler", base_path), out);
            }
        }
    }

    fn validate_handler_ref(
        &self,
        ref_: &HandlerRef,
        base_path: &str,
        out: &mut Vec<ValidationError>,
    ) {
        match ref_ {
            HandlerRef::BuiltinName(name) if name.is_empty() => {
                out.push(ValidationError {
                    path: format!("{}.name", base_path),
                    message: "Builtin handler name is required".to_string(),
                    severity: ValidationSeverity::Error,
                    code: Some("MISSING_BUILTIN_NAME".to_string()),
                    suggestion: None,
                });
            }
            HandlerRef::BuiltinName(_) => {}
            HandlerRef::Builtin(b) if b.name.is_empty() => {
                out.push(ValidationError {
                    path: format!("{}.name", base_path),
                    message: "Builtin handler name is required".to_string(),
                    severity: ValidationSeverity::Error,
                    code: Some("MISSING_BUILTIN_NAME".to_string()),
                    suggestion: None,
                });
            }
            HandlerRef::Builtin(_) => {}
            HandlerRef::Http(h) if h.url.is_empty() => {
                out.push(ValidationError {
                    path: format!("{}.url", base_path),
                    message: "HTTP handler URL is required".to_string(),
                    severity: ValidationSeverity::Error,
                    code: Some("MISSING_HTTP_URL".to_string()),
                    suggestion: None,
                });
            }
            HandlerRef::Http(_) => {}
            HandlerRef::Script(s) if s.command.is_empty() => {
                out.push(ValidationError {
                    path: format!("{}.command", base_path),
                    message: "Script handler command is required".to_string(),
                    severity: ValidationSeverity::Error,
                    code: Some("MISSING_SCRIPT_COMMAND".to_string()),
                    suggestion: None,
                });
            }
            HandlerRef::Script(_) => {}
            HandlerRef::Composite(c) if c.chain.is_empty() => {
                out.push(ValidationError {
                    path: format!("{}.chain", base_path),
                    message: "Composite handler must have a non-empty chain array".to_string(),
                    severity: ValidationSeverity::Error,
                    code: Some("INVALID_COMPOSITE_CHAIN".to_string()),
                    suggestion: None,
                });
            }
            HandlerRef::Composite(_) => {}
        }
    }
}

impl Default for ConfigValidatorImpl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{NamespaceConfig, NamespaceConfigOrBool, ToolConfig, ToolPackageConfig};

    #[test]
    fn missing_version() {
        let cfg = ToolManagerSerializedConfig {
            version: "".into(),
            config: None,
            packages: vec![],
            standalone_tools: None,
        };
        let r = ConfigValidatorImpl::new().validate(&cfg);
        assert!(!r.valid);
    }

    #[test]
    fn valid_minimal_config() {
        let cfg = ToolManagerSerializedConfig {
            version: "1.0".into(),
            config: None,
            packages: vec![ToolPackageConfig {
                name: "demo".into(),
                version: None,
                namespace: Some(NamespaceConfigOrBool::Object(NamespaceConfig {
                    prefix: "demo".into(),
                    separator: '.',
                    auto_prefix: true,
                })),
                description: None,
                dependencies: None,
                tools: vec![ToolConfig {
                    name: "hello".into(),
                    description: "say hi".into(),
                    input_schema: Default::default(),
                    handler: SerializedHandler::Name("hello".into()),
                    concurrency_safe: true,
                    timeout: None,
                    max_retries: None,
                    metadata: None,
                    version: None,
                    tags: None,
                    deprecated: false,
                    examples: None,
                }],
            }],
            standalone_tools: None,
        };
        let r = ConfigValidatorImpl::new().validate(&cfg);
        assert!(r.valid, "{:?}", r.errors);
    }

    #[test]
    fn detects_duplicate_packages() {
        let cfg = ToolManagerSerializedConfig {
            version: "1.0".into(),
            config: None,
            packages: vec![
                ToolPackageConfig {
                    name: "demo".into(),
                    version: None,
                    namespace: None,
                    description: None,
                    dependencies: None,
                    tools: vec![],
                },
                ToolPackageConfig {
                    name: "demo".into(),
                    version: None,
                    namespace: None,
                    description: None,
                    dependencies: None,
                    tools: vec![],
                },
            ],
            standalone_tools: None,
        };
        let r = ConfigValidatorImpl::new().validate(&cfg);
        assert!(!r.valid);
    }
}
