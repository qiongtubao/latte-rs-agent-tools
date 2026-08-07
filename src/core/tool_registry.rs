//! Internal tool registry: stores tools and packages with namespace support.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::ToolError;
use crate::types::{
    ConflictStrategy, ResolvedToolName, Tool, ToolDefinition, ToolExecutionContext, ToolPackage,
    ToolRegistry, DEFAULT_NAMESPACE_SEPARATOR,
};
use crate::utils::namespace::{parse_tool_name, resolve_full_tool_name};

/// Internal entry for a registered tool.
#[derive(Clone)]
pub struct RegisteredTool {
    /// The tool itself.
    pub tool: Tool,
    /// Optional package name (for grouping).
    pub package_name: Option<String>,
    /// Wall-clock timestamp (ms).
    pub registered_at: i64,
}

/// Internal entry for a registered package.
pub struct RegisteredPackage {
    /// The package itself.
    pub pkg: ToolPackage,
    /// Wall-clock timestamp (ms).
    pub registered_at: i64,
    /// Tools owned by this package (in their fully-qualified form).
    pub tool_names: Vec<String>,
}

/// Internal registry: thread-safe via `RwLock`.
pub struct ToolRegistryImpl {
    pub(crate) tools: RwLock<HashMap<String, RegisteredTool>>,
    pub(crate) packages: RwLock<HashMap<String, RegisteredPackage>>,
    pub(crate) conflict_strategy: ConflictStrategy,
    pub(crate) default_separator: char,
}

impl ToolRegistryImpl {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            packages: RwLock::new(HashMap::new()),
            conflict_strategy: ConflictStrategy::Error,
            default_separator: DEFAULT_NAMESPACE_SEPARATOR,
        }
    }

    /// Construct with explicit configuration.
    pub fn with_config(conflict_strategy: ConflictStrategy, default_separator: char) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            packages: RwLock::new(HashMap::new()),
            conflict_strategy,
            default_separator,
        }
    }

    /// Register a single tool. The caller decides whether to use conflict handling.
    pub fn register_tool(&self, tool: Tool, package_name: Option<&str>) -> Result<(), ToolError> {
        let full_tool_name = tool.name.clone();
        let mut tools = self.tools.write().expect("poisoned");
        if tools.contains_key(&full_tool_name) {
            match self.conflict_strategy {
                ConflictStrategy::Error => {
                    return Err(ToolError::tool_already_exists(full_tool_name));
                }
                ConflictStrategy::Warn => {
                    log::warn!("Tool already exists: {}, skipping", full_tool_name);
                    return Ok(());
                }
                ConflictStrategy::Skip => return Ok(()),
                ConflictStrategy::Override => {} // fall through
            }
        }
        tools.insert(
            full_tool_name,
            RegisteredTool {
                tool,
                package_name: package_name.map(|s| s.to_string()),
                registered_at: chrono::Utc::now().timestamp_millis(),
            },
        );
        Ok(())
    }

    /// Register a whole package. Returns the list of fully-qualified tool names.
    pub async fn register_package(&self, pkg: ToolPackage) -> Result<Vec<String>, ToolError> {
        {
            let packages = self.packages.read().expect("poisoned");
            if packages.contains_key(&pkg.name) {
                return Err(ToolError::package_already_exists(pkg.name));
            }
        }

        let mut registered_names: Vec<String> = Vec::with_capacity(pkg.tools.len());
        for tool in pkg.tools.iter() {
            let full_name = resolve_full_tool_name(&tool.name, pkg.namespace.as_ref());
            let mut resolved = tool.clone();
            resolved.name = full_name.clone();
            if let Err(e) = self.register_tool(resolved, Some(&pkg.name)) {
                // Roll back partial registrations
                let mut tools = self.tools.write().expect("poisoned");
                for n in &registered_names {
                    tools.remove(n);
                }
                return Err(e);
            }
            registered_names.push(full_name);
        }

        // Insert the package
        let pkg_name_for_init = pkg.name.clone();
        {
            let mut packages = self.packages.write().expect("poisoned");
            packages.insert(
                pkg.name.clone(),
                RegisteredPackage {
                    pkg,
                    registered_at: chrono::Utc::now().timestamp_millis(),
                    tool_names: registered_names.clone(),
                },
            );
        }

        // Call on_init hook (after storing, so the hook can re-query the registry).
        let on_init = {
            let packages = self.packages.read().expect("poisoned");
            packages
                .get(&pkg_name_for_init)
                .and_then(|p| p.pkg.on_init.clone())
        };
        if let Some(hook) = on_init {
            (hook)(self).await;
        }
        Ok(registered_names)
    }

    /// Unregister a single tool by name.
    pub fn unregister_tool(&self, name: &str) {
        self.tools.write().expect("poisoned").remove(name);
    }

    /// Unregister a whole package.
    pub async fn unregister_package(&self, name: &str) -> Result<(), ToolError> {
        let entry = {
            let mut packages = self.packages.write().expect("poisoned");
            packages.remove(name)
        };
        let mut entry = entry.ok_or_else(|| ToolError::package_not_found(name))?;
        if let Some(on_destroy) = entry.pkg.on_destroy.take() {
            (on_destroy)().await;
        }
        let mut tools = self.tools.write().expect("poisoned");
        for n in entry.tool_names {
            tools.remove(&n);
        }
        Ok(())
    }

    /// Whether a tool is registered.
    pub fn has(&self, name: &str) -> bool {
        self.tools.read().expect("poisoned").contains_key(name)
    }

    /// Look up a tool.
    pub fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools
            .read()
            .expect("poisoned")
            .get(name)
            .map(|e| e.tool.clone())
    }

    /// Look up a package.
    pub fn get_package(&self, name: &str) -> Option<ToolPackage> {
        self.packages
            .read()
            .expect("poisoned")
            .get(name)
            .map(|e| e.pkg.clone())
    }

    /// All tool names.
    pub fn get_tool_names(&self) -> Vec<String> {
        self.tools.read().expect("poisoned").keys().cloned().collect()
    }

    /// All package names.
    pub fn get_package_names(&self) -> Vec<String> {
        self.packages
            .read()
            .expect("poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// All tool definitions (for AI).
    pub fn get_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .read()
            .expect("poisoned")
            .values()
            .map(|e| ToolDefinition {
                name: e.tool.name.clone(),
                description: e.tool.description.clone(),
                input_schema: e.tool.input_schema.clone(),
                strict: e.tool.strict,
            })
            .collect()
    }

    /// Resolve a tool name into its parsed namespace / original name.
    pub fn resolve_tool_name(&self, name: &str) -> Option<ResolvedToolName> {
        let tool = self.get_tool(name)?;
        let parsed = parse_tool_name(name, self.default_separator);
        Some(ResolvedToolName {
            tool,
            namespace: parsed.namespace,
            original_name: parsed.original_name,
        })
    }

    /// Snapshot of all packages (for serialization).
    pub fn packages_snapshot(&self) -> Vec<ToolPackage> {
        self.packages
            .read()
            .expect("poisoned")
            .values()
            .map(|e| e.pkg.clone())
            .collect()
    }
}

impl Default for ToolRegistryImpl {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolRegistry for ToolRegistryImpl {
    fn register(&self, tool: Tool, package_name: Option<&str>) {
        if let Err(e) = self.register_tool(tool, package_name) {
            log::warn!("registry.register failed: {}", e);
        }
    }

    async fn register_package(&self, package: ToolPackage) -> Result<(), ToolError> {
        self.register_package(package).await.map(|_| ())
    }

    fn unregister(&self, name: &str) {
        self.unregister_tool(name);
    }

    async fn unregister_package(&self, name: &str) -> Result<(), ToolError> {
        self.unregister_package(name).await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.get_tool(name)
    }

    fn get_tool_names(&self) -> Vec<String> {
        self.get_tool_names()
    }

    fn get_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.get_tool_definitions()
    }

    fn has(&self, name: &str) -> bool {
        self.has(name)
    }

    fn get_package(&self, name: &str) -> Option<ToolPackage> {
        self.get_package(name)
    }

    fn get_package_names(&self) -> Vec<String> {
        self.get_package_names()
    }

    fn resolve_tool_name(&self, name: &str) -> Option<ResolvedToolName> {
        self.resolve_tool_name(name)
    }

    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        context: Option<ToolExecutionContext>,
    ) -> Result<serde_json::Value, ToolError> {
        let resolved = self.resolve_tool_name(name);
        let tool_name = name.to_string();
        let resolved = resolved.ok_or_else(|| ToolError::tool_not_found(tool_name.clone()))?;
        let ctx = context.unwrap_or_else(|| {
            ToolExecutionContext::fresh(&tool_name, resolved.tool.max_retries.unwrap_or(0))
        });
        (resolved.tool.handler)(input, ctx).await
    }
}
