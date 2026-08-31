//! `ToolManagerImpl` — the high-level orchestrator that owns the registry,
//! hook manager, resolver, and logger.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

// BoxFuture no longer needed (async_trait handles it).
use serde_json::Value;

use crate::core::config_validator::ConfigValidatorImpl;
use crate::core::hook_manager::HookManagerImpl;
use crate::core::tool_registry::ToolRegistryImpl;
use crate::error::ToolError;
use crate::resolvers::CompositeHandlerResolver;
use crate::types::{
    ConfigDiff, ConfigFieldChange, ConflictStrategy, CreateFromConfigOptions, HookFn,
    HookRegistrationOptions, MergeOptions, ModifiedTool, PackagePriority, ResolvedToolName,
    SerializedHandler, Tool, ToolConfig, ToolDefinition, ToolExecutionContext, ToolHookCallbacks,
    ToolHookEvent, ToolManager, ToolManagerConfig, ToolManagerSerializedConfig, ToolPackage,
    ToolPackageConfig, ValidationResult, DEFAULT_NAMESPACE_SEPARATOR,
};
use crate::utils::namespace::parse_tool_name;
use crate::utils::schema_validator::validate_input;

/// Default tool manager implementation. `Arc`-friendly via inner `Arc`s.
pub struct ToolManagerImpl {
    pub(crate) registry: Arc<ToolRegistryImpl>,
    pub(crate) hook_manager: Arc<HookManagerImpl>,
    pub(crate) handler_resolver: Arc<CompositeHandlerResolver>,
    config: ToolManagerConfig,
    destroyed: AtomicBool,
    /// Optional override logger storage.
    logger: Arc<RwLock<Option<Arc<dyn crate::types::ToolLogger>>>>,
}

impl std::fmt::Debug for ToolManagerImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolManagerImpl")
            .field("config", &self.config)
            .field("destroyed", &self.destroyed.load(Ordering::SeqCst))
            .finish()
    }
}

impl ToolManagerImpl {
    /// Construct a new manager with the given config.
    pub fn new(config: ToolManagerConfig) -> Arc<Self> {
        let registry = Arc::new(ToolRegistryImpl::with_config(
            config.conflict_strategy,
            config.default_separator,
        ));
        let hook_manager = Arc::new(HookManagerImpl::new());
        let handler_resolver = Arc::new(CompositeHandlerResolver::new(config.default_timeout));

        // Pre-register hooks from config
        if let Some(hooks) = config.hooks.as_ref() {
            hook_manager.register_callbacks(hooks.clone());
        }

        Arc::new(Self {
            registry,
            hook_manager,
            handler_resolver,
            config,
            destroyed: AtomicBool::new(false),
            logger: Arc::new(RwLock::new(None)),
        })
    }

    fn ensure_alive(&self) -> Result<(), ToolError> {
        if self.destroyed.load(Ordering::SeqCst) {
            Err(ToolError::ManagerDestroyed)
        } else {
            Ok(())
        }
    }

    /// Access the resolver (useful for tests).
    pub fn resolver(&self) -> &Arc<CompositeHandlerResolver> {
        &self.handler_resolver
    }

    /// Access the hook manager (useful for tests).
    pub fn hook_manager(&self) -> &Arc<HookManagerImpl> {
        &self.hook_manager
    }
}

// =============================================================================
// ToolManager trait implementation
// =============================================================================

#[async_trait::async_trait]
impl ToolManager for ToolManagerImpl {
    fn config(&self) -> &ToolManagerConfig {
        &self.config
    }

    // ----- Registration -----
    fn register(&self, tool: Tool, package_name: Option<&str>) {
        if let Err(e) = self.registry.register_tool(tool.clone(), package_name) {
            log::warn!("manager.register failed: {}", e);
            return;
        }
        self.hook_manager.emit_sync(
            ToolHookEvent::OnRegister,
            vec![Value::String(tool.name.clone())],
        );
    }

    async fn register_package(&self, package: ToolPackage) -> Result<(), ToolError> {
        let package_name = package.name.clone();
        self.registry.register_package(package).await?;
        self.hook_manager.emit_sync(
            ToolHookEvent::OnPackageRegister,
            vec![Value::String(package_name)],
        );
        Ok(())
    }

    fn unregister(&self, name: &str) {
        self.registry.unregister_tool(name);
        self.hook_manager.emit_sync(
            ToolHookEvent::OnUnregister,
            vec![Value::String(name.to_string())],
        );
    }

    async fn unregister_package(&self, name: &str) -> Result<(), ToolError> {
        self.registry.unregister_package(name).await?;
        self.hook_manager.emit_sync(
            ToolHookEvent::OnPackageUnregister,
            vec![Value::String(name.to_string())],
        );
        Ok(())
    }
    // ----- Query -----
    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.registry.get_tool(name)
    }

    fn get_tool_names(&self) -> Vec<String> {
        self.registry.get_tool_names()
    }

    fn get_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.registry.get_tool_definitions()
    }

    fn has(&self, name: &str) -> bool {
        self.registry.has(name)
    }

    fn get_package(&self, name: &str) -> Option<ToolPackage> {
        self.registry.get_package(name)
    }

    fn get_package_names(&self) -> Vec<String> {
        self.registry.get_package_names()
    }

    fn resolve_tool_name(&self, name: &str) -> Option<ResolvedToolName> {
        self.registry.resolve_tool_name(name)
    }

    // ----- Execution -----
    async fn execute(
        &self,
        name: &str,
        input: Value,
        context: Option<ToolExecutionContext>,
    ) -> Result<Value, ToolError> {
        if let Err(e) = self.ensure_alive() {
            return Err(e);
        }
        let resolved = match self.registry.resolve_tool_name(name) {
            Some(r) => r,
            // 带上注册表快照：模型猜错工具名（`edit` vs `write`）时能从
            // 报错里直接读到正确选项，而不是继续换名字试。
            None => {
                return Err(ToolError::tool_not_found_with_available(
                    name,
                    self.registry.get_tool_names(),
                ))
            }
        };
        let tool = resolved.tool.clone();
        let full_name = name.to_string();
        let max_retries = tool
            .max_retries
            .unwrap_or(self.config.default_max_retries);
        let timeout = tool.timeout.unwrap_or(self.config.default_timeout);
        let namespace = resolved.namespace.clone();
        let original_name = resolved.original_name.clone();
        let hook_manager = self.hook_manager.clone();
        let _registry = self.registry.clone();

        if self.config.validate_schemas {
            let errs = validate_input(&input, &tool.input_schema);
            if !errs.valid {
                return Err(ToolError::validation(
                    "input failed schema validation",
                    errs
                        .errors
                        .into_iter()
                        .map(|m| crate::error::ValidationIssue {
                            path: "<input>".into(),
                            message: m,
                        })
                        .collect(),
                ));
            }
        }

        let before_args = vec![Value::String(full_name.clone()), input.clone()];
        let after_args = before_args.clone();
        hook_manager
            .emit(ToolHookEvent::BeforeExecute, before_args)
            .await;

        let ctx = context.unwrap_or_else(|| {
            let mut c = ToolExecutionContext::fresh(&full_name, max_retries);
            c.namespace = namespace;
            c.original_name = Some(original_name);
            c
        });

        let fut = (tool.handler)(input, ctx.clone());
        let result = match tokio::time::timeout(timeout, fut).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                hook_manager
                    .emit(ToolHookEvent::OnError, vec![Value::String(full_name.clone())])
                    .await;
                return Err(e);
            }
            Err(_) => {
                hook_manager
                    .emit(ToolHookEvent::OnTimeout, vec![Value::String(full_name.clone())])
                    .await;
                return Err(ToolError::timeout(&full_name, timeout));
            }
        };

        hook_manager
            .emit(ToolHookEvent::AfterExecute, after_args)
            .await;
        Ok(result)
    }

    // ----- Hooks -----
    fn on(&self, event: ToolHookEvent, callback: HookFn) {
        self.hook_manager.on(event, callback);
    }

    fn on_with_options(
        &self,
        event: ToolHookEvent,
        callback: HookFn,
        options: HookRegistrationOptions,
    ) {
        self.hook_manager.on_with_options(event, callback, options);
    }

    fn off(&self, event: ToolHookEvent, callback: Option<HookFn>) {
        self.hook_manager.off(event, callback);
    }

    fn register_hooks(&self, callbacks: ToolHookCallbacks) {
        self.hook_manager.register_callbacks(callbacks);
    }

    fn clear_hooks(&self) {
        self.hook_manager.clear_hooks();
    }

    // ----- Discovery -----
    fn find_tools_by_namespace(&self, namespace: &str) -> Vec<Tool> {
        self.registry
            .get_tool_names()
            .into_iter()
            .filter_map(|n| {
                let parsed = parse_tool_name(&n, DEFAULT_NAMESPACE_SEPARATOR);
                if parsed.namespace.as_deref() == Some(namespace) {
                    self.registry.get_tool(&n)
                } else {
                    None
                }
            })
            .collect()
    }

    fn find_tools_by_metadata(&self, key: &str, value: &Value) -> Vec<Tool> {
        self.registry
            .get_tool_names()
            .into_iter()
            .filter_map(|n| {
                let tool = self.registry.get_tool(&n)?;
                let meta = tool.metadata.as_ref()?;
                meta.get(key).map(|v| v == value).unwrap_or(false).then_some(tool)
            })
            .collect()
    }

    fn find_tools_by_tag(&self, tag: &str) -> Vec<Tool> {
        self.registry
            .get_tool_names()
            .into_iter()
            .filter_map(|n| {
                let tool = self.registry.get_tool(&n)?;
                tool.tags
                    .as_ref()
                    .map(|tags| tags.iter().any(|t| t == tag))
                    .unwrap_or(false)
                    .then_some(tool)
            })
            .collect()
    }

    // ----- Scope creation -----
    fn create_scope(&self, tool_names: Vec<String>) -> Box<dyn ToolManager> {
        let scoped = ToolManagerImpl::new(self.config.clone());
        for name in tool_names {
            if let Some(tool) = self.registry.get_tool(&name) {
                scoped.register(tool, None);
            }
        }
        Box::new(Arc::try_unwrap(scoped).unwrap_or_else(|arc| (*arc).clone()))
    }

    fn create_namespace_scope(&self, namespace: &str) -> Box<dyn ToolManager> {
        let tools = self.find_tools_by_namespace(namespace);
        let scoped = ToolManagerImpl::new(self.config.clone());
        for tool in tools {
            scoped.register(tool, None);
        }
        Box::new(Arc::try_unwrap(scoped).unwrap_or_else(|arc| (*arc).clone()))
    }

    // ----- Serialization -----
    fn export_config(&self) -> ToolManagerSerializedConfig {
        let packages: Vec<ToolPackageConfig> = self
            .registry
            .packages_snapshot()
            .into_iter()
            .map(|pkg| ToolPackageConfig {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                namespace: pkg
                    .namespace
                    .as_ref()
                    .map(|ns| crate::types::NamespaceConfigOrBool::Object(ns.clone())),
                description: pkg.description.clone(),
                dependencies: pkg.dependencies.clone(),
                tools: pkg
                    .tools
                    .iter()
                    .map(|t| {
                        let short_name = crate::utils::namespace::remove_namespace(
                            &t.name,
                            crate::types::DEFAULT_NAMESPACE_SEPARATOR,
                        );
                        ToolConfig {
                            name: short_name,
                            description: t.description.clone(),
                            input_schema: t.input_schema.clone(),
                            handler: SerializedHandler::Name(t.name.clone()),
                        concurrency_safe: t.concurrency_safe,
                        timeout: t.timeout.map(|d| d.as_millis() as u64),
                        max_retries: t.max_retries,
                        metadata: t.metadata.clone(),
                        version: t.version.clone(),
                        tags: t.tags.clone(),
                        deprecated: t.deprecated,
                        examples: t.examples.clone(),
                    }})
                    .collect(),
            })
            .collect();

        ToolManagerSerializedConfig {
            version: "1.0".into(),
            config: Some(crate::types::SerializedManagerConfig {
                default_timeout: Some(self.config.default_timeout.as_millis() as u64),
                default_max_retries: Some(self.config.default_max_retries),
                default_separator: Some(self.config.default_separator),
                conflict_strategy: Some(self.config.conflict_strategy),
                allow_override: Some(self.config.allow_override),
                validate_schemas: Some(self.config.validate_schemas),
                metadata: self.config.metadata.clone(),
            }),
            packages,
            standalone_tools: None,
        }
    }

    // ----- Lifecycle -----
    async fn destroy(&self) {
        self.hook_manager
            .emit(ToolHookEvent::OnDestroy, vec![])
            .await;
        for name in self.registry.get_package_names() {
            if let Err(e) = self.registry.unregister_package(&name).await {
                log::warn!("failed to unregister package {}: {}", name, e);
            }
        }
        self.hook_manager.clear_hooks();
    }
}

// Implement `Clone` for `ToolManagerImpl` to allow scope creation.
impl Clone for ToolManagerImpl {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            hook_manager: self.hook_manager.clone(),
            handler_resolver: self.handler_resolver.clone(),
            config: self.config.clone(),
            destroyed: AtomicBool::new(self.destroyed.load(Ordering::SeqCst)),
            logger: self.logger.clone(),
        }
    }
}

// =============================================================================
// Factory functions
// =============================================================================

/// Construct a fresh `ToolManagerImpl`. Mirrors `createToolManager` in TS.
pub fn create_tool_manager() -> Arc<ToolManagerImpl> {
    ToolManagerImpl::new(ToolManagerConfig::default())
}

/// Construct a fresh `ToolManagerImpl` from explicit config.
pub fn create_tool_manager_with_config(config: ToolManagerConfig) -> Arc<ToolManagerImpl> {
    ToolManagerImpl::new(config)
}

// =============================================================================
// `toolManagerFactory` — JSON-driven create / diff / merge / scope / validate
// =============================================================================

/// The static factory object. Mirrors `toolManagerFactory` in TS.
pub struct ToolManagerFactory;

impl ToolManagerFactory {
    /// Build a manager from a serialized config. Honors `merge_into` and `skip_init_hooks`.
    pub async fn create_from_config(
        config: ToolManagerSerializedConfig,
        options: CreateFromConfigOptions,
    ) -> Result<Arc<ToolManagerImpl>, ToolError> {
        // 1. Validate (unless explicitly disabled).
        if options.validate {
            let result = ConfigValidatorImpl::new().validate(&config);
            if !result.valid {
                let issues = result
                    .errors
                    .into_iter()
                    .filter(|e| e.severity == crate::types::ValidationSeverity::Error)
                    .map(|e| crate::error::ValidationIssue {
                        path: e.path,
                        message: e.message,
                    })
                    .collect();
                return Err(ToolError::validation("Invalid config", issues));
            }
        }

        // 2. Resolve target manager.
        let manager = if let Some(existing) = options.merge_into.as_ref() {
            existing.clone()
        } else {
            ToolManagerImpl::new(options.config_overrides.unwrap_or_default())
        };

        // 3. Register packages
        for pkg in &config.packages {
            let mut tools = Vec::with_capacity(pkg.tools.len());
            for tc in &pkg.tools {
                let serialized = &tc.handler;
                let resolved = match serialized {
                    SerializedHandler::Name(name) => {
                        options.handler_resolver.resolve(
                            &HandlerRefEnum::from_str(name),
                            &tc.name,
                        ).await?
                    }
                    SerializedHandler::Ref(r) => {
                        options.handler_resolver.resolve(r, &tc.name).await?
                    }
                };
                let mut builder = Tool::builder(
                    tc.name.clone(),
                    tc.description.clone(),
                    tc.input_schema.clone(),
                    resolved,
                )
                .concurrency_safe(tc.concurrency_safe)
                .deprecated(tc.deprecated);
                if let Some(ms) = tc.timeout {
                    builder = builder.timeout(Duration::from_millis(ms));
                }
                if let Some(n) = tc.max_retries {
                    builder = builder.max_retries(n);
                }
                if let Some(meta) = tc.metadata.clone() {
                    builder = builder.metadata(meta);
                }
                if let Some(v) = tc.version.clone() {
                    builder = builder.version(v);
                }
                if let Some(tags) = tc.tags.clone() {
                    builder = builder.tags(tags);
                }
                if let Some(examples) = tc.examples.clone() {
                    builder = builder.examples(examples);
                }
                tools.push(builder.build());
            }

            let package = ToolPackage {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                namespace: match pkg.namespace.as_ref() {
                    Some(crate::types::NamespaceConfigOrBool::Object(o)) => Some(o.clone()),
                    _ => None,
                },
                description: pkg.description.clone(),
                dependencies: pkg.dependencies.clone(),
                tools,
                on_init: None,
                on_destroy: None,
                before_execute: None,
                after_execute: None,
                metadata: None,
            };

            if options.skip_init_hooks {
                // Register without invoking on_init: easiest is to drop the hook
                // temporarily. (The package's on_init is `None` here since we
                // built the package from config; the skip flag is therefore a
                // no-op for the config path but is preserved for parity.)
            }
            manager.register_package(package).await?;
        }

        // 4. Standalone tools
        if let Some(tools) = config.standalone_tools.as_ref() {
            for tc in tools {
                let resolved = match &tc.handler {
                    SerializedHandler::Name(name) => {
                        options
                            .handler_resolver
                            .resolve(&HandlerRefEnum::from_str(name), &tc.name)
                            .await?
                    }
                    SerializedHandler::Ref(r) => {
                        options.handler_resolver.resolve(r, &tc.name).await?
                    }
                };
                let mut builder = Tool::builder(
                    tc.name.clone(),
                    tc.description.clone(),
                    tc.input_schema.clone(),
                    resolved,
                )
                .concurrency_safe(tc.concurrency_safe)
                .deprecated(tc.deprecated);
                if let Some(ms) = tc.timeout {
                    builder = builder.timeout(Duration::from_millis(ms));
                }
                if let Some(n) = tc.max_retries {
                    builder = builder.max_retries(n);
                }
                manager.register(builder.build(), None);
            }
        }

        Ok(manager)
    }

    /// Wrap an existing manager's `export_config` for the factory surface.
    pub fn to_config(manager: &ToolManagerImpl) -> ToolManagerSerializedConfig {
        manager.export_config()
    }

    /// Validate a config without instantiating a manager.
    pub fn validate_config(config: &ToolManagerSerializedConfig) -> ValidationResult {
        ConfigValidatorImpl::new().validate(config)
    }

    /// Merge multiple configs into a single one.
    pub fn merge_configs(
        configs: Vec<ToolManagerSerializedConfig>,
        options: MergeOptions,
    ) -> ToolManagerSerializedConfig {
        if configs.is_empty() {
            return ToolManagerSerializedConfig::empty();
        }
        if configs.len() == 1 {
            return configs.into_iter().next().unwrap();
        }
        let strategy = options.conflict_strategy.unwrap_or(ConflictStrategy::Warn);
        let mut merged_packages: std::collections::BTreeMap<String, ToolPackageConfig> =
            std::collections::BTreeMap::new();

        for config in &configs {
            for pkg in &config.packages {
                if let Some(existing) = merged_packages.get_mut(&pkg.name) {
                    let merged_tools: Vec<ToolConfig> = match options.priority_order.unwrap_or(PackagePriority::First) {
                        PackagePriority::First => {
                            let mut t = existing.tools.clone();
                            t.extend(pkg.tools.iter().cloned());
                            t
                        }
                        PackagePriority::Last => {
                            let mut t = pkg.tools.clone();
                            t.extend(existing.tools.iter().cloned());
                            t
                        }
                    };
                    let deduplicated = deduplicate_tools(merged_tools, strategy);
                    *existing = ToolPackageConfig {
                        name: pkg.name.clone(),
                        version: pkg.version.clone().or_else(|| existing.version.clone()),
                        namespace: pkg.namespace.clone().or_else(|| existing.namespace.clone()),
                        description: pkg.description.clone().or_else(|| existing.description.clone()),
                        dependencies: pkg.dependencies.clone().or_else(|| existing.dependencies.clone()),
                        tools: deduplicated,
                    };
                } else {
                    merged_packages.insert(pkg.name.clone(), pkg.clone());
                }
            }
        }

        ToolManagerSerializedConfig {
            version: "1.0".into(),
            config: configs.last().and_then(|c| c.config.clone()),
            packages: merged_packages.into_values().collect(),
            standalone_tools: None,
        }
    }

    /// Filter a config down to the given tool names.
    pub fn create_scope_config(
        config: ToolManagerSerializedConfig,
        tool_names: &[String],
    ) -> ToolManagerSerializedConfig {
        let tool_set: std::collections::HashSet<&String> = tool_names.iter().collect();
        let packages: Vec<ToolPackageConfig> = config
            .packages
            .into_iter()
            .filter_map(|pkg| {
                let filtered: Vec<ToolConfig> = pkg
                    .tools
                    .into_iter()
                    .filter(|t| tool_set.contains(&t.name))
                    .collect();
                if filtered.is_empty() {
                    None
                } else {
                    Some(ToolPackageConfig {
                        tools: filtered,
                        ..pkg
                    })
                }
            })
            .collect();

        ToolManagerSerializedConfig {
            version: config.version,
            config: config.config,
            packages,
            standalone_tools: config.standalone_tools,
        }
    }

    /// Compute a diff between two configs.
    pub fn diff_configs(
        a: &ToolManagerSerializedConfig,
        b: &ToolManagerSerializedConfig,
    ) -> ConfigDiff {
        let tools_a = collect_tools(a);
        let tools_b = collect_tools(b);

        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut modified = Vec::new();

        for (name, tool) in &tools_b {
            match tools_a.get(name) {
                None => added.push(tool.clone()),
                Some(old) => {
                    if json_differs(old, tool) {
                        modified.push(ModifiedTool {
                            name: name.clone(),
                            old: old.clone(),
                            new: tool.clone(),
                            changes: diff_fields(&serde_json::to_value(old).unwrap_or(Value::Null), &serde_json::to_value(tool).unwrap_or(Value::Null)),
                        });
                    }
                }
            }
        }
        for (name, tool) in &tools_a {
            if !tools_b.contains_key(name) {
                removed.push(tool.clone());
            }
        }

        let added_pkgs: Vec<ToolPackageConfig> = b
            .packages
            .iter()
            .filter(|p| !a.packages.iter().any(|ap| ap.name == p.name))
            .cloned()
            .collect();
        let removed_pkgs: Vec<ToolPackageConfig> = a
            .packages
            .iter()
            .filter(|p| !b.packages.iter().any(|bp| bp.name == p.name))
            .cloned()
            .collect();

        let config_changes = match (a.config.as_ref(), b.config.as_ref()) {
            (None, None) => None,
            (Some(aa), Some(bb)) => diff_fields(
                &serde_json::to_value(aa).unwrap_or(Value::Null),
                &serde_json::to_value(bb).unwrap_or(Value::Null),
            )
            .into(),
            (old, new) => Some(vec![ConfigFieldChange {
                field: "config".into(),
                old_value: old.map(|v| serde_json::to_value(v).unwrap_or(Value::Null)),
                new_value: new.map(|v| serde_json::to_value(v).unwrap_or(Value::Null)),
            }]),
        };

        let identical = added.is_empty() && removed.is_empty() && modified.is_empty();
        ConfigDiff {
            added,
            removed,
            modified,
            added_packages: added_pkgs,
            removed_packages: removed_pkgs,
            modified_packages: Vec::new(),
            config_changes,
            identical,
        }
    }
}

/// Free-function alias matching the TS `toolManagerFactory` import.
pub async fn tool_manager_factory_create(
    config: ToolManagerSerializedConfig,
    options: CreateFromConfigOptions,
) -> Result<Arc<ToolManagerImpl>, ToolError> {
    ToolManagerFactory::create_from_config(config, options).await
}

// =============================================================================
// Helpers
// =============================================================================

fn deduplicate_tools(tools: Vec<ToolConfig>, strategy: ConflictStrategy) -> Vec<ToolConfig> {
    let mut seen: std::collections::HashMap<String, ToolConfig> = std::collections::HashMap::new();
    for tool in tools {
        match seen.get(&tool.name) {
            None => {
                seen.insert(tool.name.clone(), tool);
            }
            Some(_) => match strategy {
                ConflictStrategy::Override => {
                    seen.insert(tool.name.clone(), tool);
                }
                _ => {} // skip / warn / error all keep the first
            },
        }
    }
    seen.into_values().collect()
}

fn collect_tools(config: &ToolManagerSerializedConfig) -> std::collections::HashMap<String, ToolConfig> {
    let mut out = std::collections::HashMap::new();
    for pkg in &config.packages {
        for tool in &pkg.tools {
            out.insert(tool.name.clone(), tool.clone());
        }
    }
    if let Some(standalone) = &config.standalone_tools {
        for tool in standalone {
            out.insert(tool.name.clone(), tool.clone());
        }
    }
    out
}

fn json_differs(a: &ToolConfig, b: &ToolConfig) -> bool {
    serde_json::to_value(a).ok() != serde_json::to_value(b).ok()
}

fn diff_fields(old: &Value, new: &Value) -> Vec<ConfigFieldChange> {
    let mut out = Vec::new();
    if let (Some(a), Some(b)) = (old.as_object(), new.as_object()) {
        let mut keys: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
        keys.extend(a.keys());
        keys.extend(b.keys());
        for k in keys {
            let av = a.get(k).cloned().unwrap_or(Value::Null);
            let bv = b.get(k).cloned().unwrap_or(Value::Null);
            if av != bv {
                out.push(ConfigFieldChange {
                    field: k.clone(),
                    old_value: Some(av),
                    new_value: Some(bv),
                });
            }
        }
    } else if old != new {
        out.push(ConfigFieldChange {
            field: "<root>".into(),
            old_value: Some(old.clone()),
            new_value: Some(new.clone()),
        });
    }
    out
}

/// Internal helper enum used by `create_from_config` to bridge string names
/// to the `HandlerRef` enum (the latter uses `untagged` deserialization so
/// constructing from a string is non-trivial).
#[derive(Debug)]
/// Internal helper enum for bridging string handler names to `HandlerRef`.
pub enum HandlerRefEnum {
    Name(String),
    Object(crate::types::HandlerRef),
}

impl HandlerRefEnum {
    /// Construct from a bare string identifier (assumed to be a builtin name).
    pub fn from_str(s: &str) -> crate::types::HandlerRef {
        crate::types::HandlerRef::BuiltinName(s.to_string())
    }
}

impl From<crate::types::HandlerRef> for HandlerRefEnum {
    fn from(value: crate::types::HandlerRef) -> Self {
        Self::Object(value)
    }
}
