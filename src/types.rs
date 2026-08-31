//! Type definitions mirroring the `latte-ts-agent` / `latte-ts-models` tool contracts.
//!
//! These types are the canonical wire format for tool definitions, packages, hooks,
//! and serialized manager configuration. They are intentionally `Serialize` /
//! `Deserialize` to support the JSON config path used by `tool_manager_factory`.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// =============================================================================
// Namespace
// =============================================================================

/// Separator used in tool name namespaces.
pub type NamespaceSeparator = char;

/// Default namespace separator (`.`).
pub const DEFAULT_NAMESPACE_SEPARATOR: NamespaceSeparator = '.';

/// Namespace configuration for a `ToolPackage`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamespaceConfig {
    /// Prefix to prepend to tool names.
    pub prefix: String,
    /// Separator between prefix and tool name.
    #[serde(default = "default_separator")]
    pub separator: NamespaceSeparator,
    /// When `true` (default), automatically prepend the prefix to all tool names
    /// in the package unless they already start with the prefix.
    #[serde(default = "default_true")]
    pub auto_prefix: bool,
}

fn default_separator() -> NamespaceSeparator {
    DEFAULT_NAMESPACE_SEPARATOR
}

fn default_true() -> bool {
    true
}

impl Default for NamespaceConfig {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            separator: DEFAULT_NAMESPACE_SEPARATOR,
            auto_prefix: true,
        }
    }
}

// =============================================================================
// Tool input schema
// =============================================================================

/// JSON-schema-style description of a single property.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolInputProperty {
    /// Property type. Matches JSON Schema primitives.
    #[serde(rename = "type")]
    pub property_type: PropertyType,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional enum of allowed values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<serde_json::Value>>,
    /// Minimum for numeric types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    /// Maximum for numeric types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    /// Minimum length for strings/arrays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<usize>,
    /// Maximum length for strings/arrays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    /// Element schema for `array` properties (JSON Schema `items`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<ToolInputProperty>>,
    /// Nested properties for `object` properties (JSON Schema `properties`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<BTreeMap<String, ToolInputProperty>>,
    /// Required nested property names for `object` properties.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
    /// Whether nested objects allow additional properties.
    #[serde(
        rename = "additionalProperties",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_properties: Option<ToolAdditionalProperties>,
}

impl ToolInputProperty {
    /// Set the element schema of an `array` property (builder style).
    pub fn with_items(mut self, items: ToolInputProperty) -> Self {
        self.items = Some(Box::new(items));
        self
    }

    /// Set the nested schema of an `object` property (builder style).
    pub fn with_object(
        mut self,
        properties: BTreeMap<String, ToolInputProperty>,
        required: Option<Vec<String>>,
        additional_properties: Option<ToolAdditionalProperties>,
    ) -> Self {
        self.properties = Some(properties);
        self.required = required;
        self.additional_properties = additional_properties;
        self
    }
}

/// JSON Schema `additionalProperties`: either a boolean toggle or a nested
/// schema. Serializes untagged (`false` / `{...}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ToolAdditionalProperties {
    /// Boolean toggle (`false` = no extra keys allowed).
    Boolean(bool),
}

impl From<bool> for ToolAdditionalProperties {
    fn from(b: bool) -> Self {
        Self::Boolean(b)
    }
}

/// Supported JSON Schema property types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PropertyType {
    /// JSON string.
    String,
    /// JSON number (floating point).
    Number,
    /// JSON integer.
    Integer,
    /// JSON boolean.
    Boolean,
    /// JSON array.
    Array,
    /// JSON object.
    Object,
    /// JSON null.
    Null,
}

/// Top-level tool input schema (JSON Schema `object`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolInputSchema {
    /// Always the string `"object"`.
    #[serde(rename = "type")]
    pub schema_type: SchemaType,
    /// Map of property name → property schema.
    #[serde(default)]
    pub properties: BTreeMap<String, ToolInputProperty>,
    /// Required property names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
    /// Whether additional properties are allowed.
    #[serde(
        rename = "additionalProperties",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_properties: Option<bool>,
}

/// Marker type that always serializes to the JSON Schema type `"object"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaType;

impl Serialize for SchemaType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("object")
    }
}

impl<'de> Deserialize<'de> for SchemaType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v.as_str() {
            Some("object") => Ok(SchemaType),
            other => Err(serde::de::Error::custom(format!(
                "expected 'object', got {:?}",
                other
            ))),
        }
    }
}

// =============================================================================
// Tool execution
// =============================================================================

/// Context passed to a tool handler on every invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionContext {
    /// Unique identifier for this invocation.
    pub tool_id: String,
    /// 0-based attempt number.
    pub attempt: u32,
    /// Maximum retries permitted for this invocation.
    pub max_retries: u32,
    /// Optional free-form metadata passed by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Optional namespace derived from the resolved name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Original tool name before namespace resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    /// Wall-clock timestamp (ms since epoch).
    pub timestamp: i64,
}

impl ToolExecutionContext {
    /// Construct a fresh context for the given tool name.
    pub fn fresh(tool_name: &str, max_retries: u32) -> Self {
        Self {
            tool_id: format!("{}-{}", tool_name, uuid::Uuid::new_v4()),
            attempt: 0,
            max_retries,
            metadata: None,
            namespace: None,
            original_name: Some(tool_name.to_string()),
            timestamp: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// The async signature every tool handler must implement.
pub type ToolHandler =
    dyn Fn(serde_json::Value, ToolExecutionContext) -> futures::future::BoxFuture<'static, Result<serde_json::Value, crate::error::ToolError>>
    + Send
    + Sync;

/// Lightweight, `Send + Sync` view of a tool handler suitable for storing in
/// the registry. Wraps an `Arc`-shared boxed handler.
pub type SharedToolHandler = std::sync::Arc<ToolHandler>;

/// Full tool definition: metadata + handler + execution options.
#[derive(Clone)]
pub struct Tool {
    /// Fully-qualified name (including namespace prefix when applicable).
    pub name: String,
    /// Human-readable description surfaced to the model.
    pub description: String,
    /// JSON Schema describing the expected input.
    pub input_schema: ToolInputSchema,
    /// The async handler invoked by `ToolManager::execute`.
    pub handler: SharedToolHandler,
    /// Whether concurrent invocations of this tool are safe.
    pub concurrency_safe: bool,
    /// Per-invocation timeout. `None` defers to the manager default.
    pub timeout: Option<Duration>,
    /// Per-tool retry count. `None` defers to the manager default.
    pub max_retries: Option<u32>,
    /// Optional metadata for discovery (`category`, `tags`, ...).
    pub metadata: Option<serde_json::Value>,
    /// Optional tool version.
    pub version: Option<String>,
    /// Optional tags for filtering.
    pub tags: Option<Vec<String>>,
    /// Marks the tool as deprecated; surfaces warnings during registration.
    pub deprecated: bool,
    /// Optional examples to help the model invoke the tool correctly.
    pub examples: Option<Vec<serde_json::Value>>,
    /// OpenAI Structured Outputs 开关。`Some(true)` 时下发给模型的
    /// tool schema 带 `strict: true`，要求模型严格按 input_schema 输出
    /// （schema 需满足：所有 properties 进 required、
    /// `additionalProperties: false`）。`None` = 不开。
    pub strict: Option<bool>,
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .field("concurrency_safe", &self.concurrency_safe)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("metadata", &self.metadata)
            .field("version", &self.version)
            .field("tags", &self.tags)
            .field("deprecated", &self.deprecated)
            .field("strict", &self.strict)
            .finish()
    }
}

impl Tool {
    /// Construct a builder for `Tool` to make handler assignment ergonomic.
    pub fn builder(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: ToolInputSchema,
        handler: SharedToolHandler,
    ) -> ToolBuilder {
        ToolBuilder {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler,
            concurrency_safe: true,
            timeout: None,
            max_retries: None,
            metadata: None,
            version: None,
            tags: None,
            deprecated: false,
            examples: None,
            strict: None,
        }
    }
}

/// Fluent builder for `Tool` mirroring the TS object literal style.
pub struct ToolBuilder {
    name: String,
    description: String,
    input_schema: ToolInputSchema,
    handler: SharedToolHandler,
    concurrency_safe: bool,
    timeout: Option<Duration>,
    max_retries: Option<u32>,
    metadata: Option<serde_json::Value>,
    version: Option<String>,
    tags: Option<Vec<String>>,
    deprecated: bool,
    examples: Option<Vec<serde_json::Value>>,
    strict: Option<bool>,
}

impl ToolBuilder {
    /// Override `concurrency_safe`.
    pub fn concurrency_safe(mut self, safe: bool) -> Self {
        self.concurrency_safe = safe;
        self
    }
    /// Set the per-invocation timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
    /// Set the per-tool retry count.
    pub fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = Some(n);
        self
    }
    /// Attach free-form metadata.
    pub fn metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
    /// Set the tool version.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }
    /// Set the tool tags.
    pub fn tags(mut self, tags: Vec<String>) -> Self {
        self.tags = Some(tags);
        self
    }
    /// Mark the tool as deprecated.
    pub fn deprecated(mut self, deprecated: bool) -> Self {
        self.deprecated = deprecated;
        self
    }
    /// Attach usage examples.
    pub fn examples(mut self, examples: Vec<serde_json::Value>) -> Self {
        self.examples = Some(examples);
        self
    }
    /// Enable OpenAI Structured Outputs (`strict: true`) for this tool.
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = Some(strict);
        self
    }
    /// Finalize and return the `Tool`.
    pub fn build(self) -> Tool {
        Tool {
            name: self.name,
            description: self.description,
            input_schema: self.input_schema,
            handler: self.handler,
            concurrency_safe: self.concurrency_safe,
            timeout: self.timeout,
            max_retries: self.max_retries,
            metadata: self.metadata,
            version: self.version,
            tags: self.tags,
            deprecated: self.deprecated,
            examples: self.examples,
            strict: self.strict,
        }
    }
}

/// Definition surfaced to AI clients (omits the handler).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema describing the expected input.
    pub input_schema: ToolInputSchema,
    /// OpenAI Structured Outputs 开关（透传自 `Tool.strict`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Resolved name carries the parsed namespace alongside the original name.
#[derive(Debug, Clone)]
pub struct ResolvedToolName {
    /// The full registered tool.
    pub tool: Tool,
    /// The parsed namespace (first component before the separator), if any.
    pub namespace: Option<String>,
    /// The portion of the name after the namespace prefix.
    pub original_name: String,
}

// =============================================================================
// Tool packages
// =============================================================================

/// A bundle of related tools with shared namespace + lifecycle hooks.
#[derive(Clone)]
pub struct ToolPackage {
    /// Package name (used as the default namespace prefix).
    pub name: String,
    /// Package version.
    pub version: Option<String>,
    /// Namespace configuration. `None` disables namespacing.
    pub namespace: Option<NamespaceConfig>,
    /// Human-readable description.
    pub description: Option<String>,
    /// Other package names this package depends on.
    pub dependencies: Option<Vec<String>>,
    /// Tools contained in this package.
    pub tools: Vec<Tool>,
    /// Optional async hook invoked after the package is fully registered.
    pub on_init: Option<PackageInitFn>,
    /// Optional async hook invoked when the package is unregistered / destroyed.
    pub on_destroy: Option<PackageDestroyFn>,
    /// Per-tool hooks executed before each tool call.
    pub before_execute: Option<PackageBeforeExecuteFn>,
    /// Per-tool hooks executed after each tool call.
    pub after_execute: Option<PackageAfterExecuteFn>,
    /// Optional metadata for discovery.
    pub metadata: Option<serde_json::Value>,
}

/// Async hook signature for `on_init`.
pub type PackageInitFn = std::sync::Arc<
    dyn Fn(&dyn ToolRegistry) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// Async hook signature for `on_destroy`.
pub type PackageDestroyFn =
    std::sync::Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Async hook signature for `before_execute`.
pub type PackageBeforeExecuteFn = std::sync::Arc<
    dyn Fn(
            &str,
            &serde_json::Value,
            &ToolExecutionContext,
        ) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// Async hook signature for `after_execute`.
pub type PackageAfterExecuteFn = std::sync::Arc<
    dyn Fn(
            &str,
            &serde_json::Value,
            &ToolExecutionContext,
        ) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

// =============================================================================
// Hook system
// =============================================================================

/// All recognized lifecycle events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolHookEvent {
    /// Before a tool executes.
    BeforeExecute,
    /// After a tool executes successfully.
    AfterExecute,
    /// On tool execution error.
    OnError,
    /// Before a retry attempt.
    OnRetry,
    /// On execution timeout.
    OnTimeout,
    /// On tool registration.
    OnRegister,
    /// On tool unregistration.
    OnUnregister,
    /// On package registration.
    OnPackageRegister,
    /// On package unregistration.
    OnPackageUnregister,
    /// On manager config change.
    OnConfigChange,
    /// On manager destruction.
    OnDestroy,
}

impl ToolHookEvent {
    /// String label used in `HookManager` maps.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BeforeExecute => "before_execute",
            Self::AfterExecute => "after_execute",
            Self::OnError => "on_error",
            Self::OnRetry => "on_retry",
            Self::OnTimeout => "on_timeout",
            Self::OnRegister => "on_register",
            Self::OnUnregister => "on_unregister",
            Self::OnPackageRegister => "on_package_register",
            Self::OnPackageUnregister => "on_package_unregister",
            Self::OnConfigChange => "on_config_change",
            Self::OnDestroy => "on_destroy",
        }
    }
}

/// Optional registration metadata for a hook callback.
#[derive(Clone, Default)]
pub struct HookRegistrationOptions {
    /// Higher priority runs first. Default: 0.
    pub priority: i32,
    /// When `true`, the hook auto-unregisters after one execution.
    pub once: bool,
    /// Optional guard evaluated before each invocation; `false` skips the hook.
    pub condition: Option<HookConditionFn>,
}

/// Guard function for conditional hooks.
pub type HookConditionFn = std::sync::Arc<dyn Fn(&[serde_json::Value]) -> bool + Send + Sync>;

/// Object-style hook registration matching the TS `ToolHookCallbacks`.
#[derive(Default, Clone)]
pub struct ToolHookCallbacks {
    /// `before_execute` hook.
    pub before_execute: Option<HookFn>,
    /// `after_execute` hook.
    pub after_execute: Option<HookFn>,
    /// `on_error` hook.
    pub on_error: Option<HookFn>,
    /// `on_retry` hook.
    pub on_retry: Option<HookFn>,
    /// `on_timeout` hook.
    pub on_timeout: Option<HookFn>,
    /// `on_register` hook.
    pub on_register: Option<HookFn>,
    /// `on_unregister` hook.
    pub on_unregister: Option<HookFn>,
    /// `on_package_register` hook.
    pub on_package_register: Option<HookFn>,
    /// `on_package_unregister` hook.
    pub on_package_unregister: Option<HookFn>,
    /// `on_config_change` hook.
    pub on_config_change: Option<HookFn>,
    /// `on_destroy` hook.
    pub on_destroy: Option<HookFn>,
}

impl std::fmt::Debug for ToolHookCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("ToolHookCallbacks");
        if self.before_execute.is_some() { d.field("before_execute", &"<hook>"); }
        if self.after_execute.is_some() { d.field("after_execute", &"<hook>"); }
        if self.on_error.is_some() { d.field("on_error", &"<hook>"); }
        if self.on_timeout.is_some() { d.field("on_timeout", &"<hook>"); }
        d.finish()
    }
}

/// Hook function signature (`Arc`-shared so it can be cloned cheaply).
pub type HookFn = std::sync::Arc<
    dyn for<'a> Fn(
            &'a [serde_json::Value],
        ) -> futures::future::BoxFuture<
            'a,
            Result<(), crate::error::ToolError>,
        > + Send
        + Sync,
>;

// =============================================================================
// Resolver references
// =============================================================================

/// Reference to a handler used inside serialized config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum HandlerRef {
    /// Bare string: looked up as a builtin handler by name.
    BuiltinName(String),
    /// Builtin handler reference (object form).
    Builtin(BuiltinHandlerRef),
    /// HTTP handler reference.
    Http(HttpHandlerRef),
    /// Shell script handler reference.
    Script(ScriptHandlerRef),
    /// Composite (chain) handler reference.
    Composite(CompositeHandlerRef),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// HTTP handler reference configuration.
pub struct HttpHandlerRef {
    /// Discriminant: `"http"`.
    #[serde(rename = "type")]
    pub handler_type: HttpHandlerType,
    /// Target URL.
    pub url: String,
    /// HTTP method. Default: `POST`.
    #[serde(default)]
    pub method: HttpMethod,
    /// Optional headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Optional body template; merged with `include_input`.
    #[serde(default, rename = "bodyTemplate")]
    pub body_template: Option<serde_json::Value>,
    /// Whether to merge the caller's `input` into the body. Default: `true`.
    #[serde(default = "default_true", rename = "includeInput")]
    pub include_input: bool,
    /// Dotted path to extract from the response body.
    #[serde(default, rename = "responsePath")]
    pub response_path: Option<String>,
    /// Request timeout in milliseconds.
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Marker type that always serializes to the JSON literal `"http"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpHandlerType;

impl Serialize for HttpHandlerType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("http")
    }
}

impl<'de> Deserialize<'de> for HttpHandlerType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v.as_str() {
            Some("http") => Ok(HttpHandlerType),
            other => Err(serde::de::Error::custom(format!(
                "expected 'http', got {:?}",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "UPPERCASE")]
/// HTTP method variants for handler requests.
pub enum HttpMethod {
        /// HTTP POST.
#[default]
    Post,
    /// HTTP GET.
    Get,
    /// HTTP PUT.
    Put,
    /// HTTP DELETE.
    Delete,
    /// HTTP PATCH.
    Patch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// Shell script handler reference configuration.
pub struct ScriptHandlerRef {
    /// Discriminant: `"script"`.
    #[serde(rename = "type")]
    pub handler_type: ScriptHandlerType,
    /// Executable / shell command.
    pub command: String,
    /// Optional arguments.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Working directory.
    #[serde(default, rename = "cwd")]
    pub cwd: Option<String>,
    /// Additional environment variables.
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    /// When `true`, send input as JSON over stdin.
    #[serde(default, rename = "stdinJson")]
    pub stdin_json: bool,
    /// When `true`, parse stdout as JSON.
    #[serde(default, rename = "parseJson")]
    pub parse_json: bool,
    /// Timeout in milliseconds.
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Marker type that always serializes to the JSON literal `"script"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptHandlerType;

impl Serialize for ScriptHandlerType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("script")
    }
}

impl<'de> Deserialize<'de> for ScriptHandlerType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v.as_str() {
            Some("script") => Ok(ScriptHandlerType),
            other => Err(serde::de::Error::custom(format!(
                "expected 'script', got {:?}",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// Builtin handler reference (object form).
pub struct BuiltinHandlerRef {
    /// Discriminant: `"builtin"`.
    #[serde(rename = "type")]
    pub handler_type: BuiltinHandlerType,
    /// Handler name (registered on the builtin resolver).
    pub name: String,
    /// Optional default params merged with the caller's input.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// Marker type that always serializes to the JSON literal `"builtin"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuiltinHandlerType;

impl Serialize for BuiltinHandlerType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("builtin")
    }
}

impl<'de> Deserialize<'de> for BuiltinHandlerType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v.as_str() {
            Some("builtin") => Ok(BuiltinHandlerType),
            other => Err(serde::de::Error::custom(format!(
                "expected 'builtin', got {:?}",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// Composite (chain) handler reference configuration.
pub struct CompositeHandlerRef {
    /// Discriminant: `"composite"`.
    #[serde(rename = "type")]
    pub handler_type: CompositeHandlerType,
    /// Handler chain.
    pub chain: Vec<HandlerRef>,
    /// Combination mode. Default: `sequence`.
    #[serde(default = "default_combine")]
    pub combine: CombineMode,
    /// Whether to stop on the first error in `sequence` mode. Default: `true`.
    #[serde(default = "default_true", rename = "stopOnError")]
    pub stop_on_error: bool,
}

/// Marker type that always serializes to the JSON literal `"composite"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompositeHandlerType;

impl Serialize for CompositeHandlerType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("composite")
    }
}

impl<'de> Deserialize<'de> for CompositeHandlerType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v.as_str() {
            Some("composite") => Ok(CompositeHandlerType),
            other => Err(serde::de::Error::custom(format!(
                "expected 'composite', got {:?}",
                other
            ))),
        }
    }
}

fn default_combine() -> CombineMode {
    CombineMode::Sequence
}

/// How a composite chain combines the outputs of its sub-handlers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CombineMode {
    /// Pipe output of handler N into handler N+1.
    #[default]
    Sequence,
    /// Run all handlers in parallel; return all results.
    Parallel,
    /// Run all handlers in parallel; merge outputs into a single object.
    Merge,
}

/// Resolver contract used by `tool_manager_factory::create_from_config`.
#[async_trait]
pub trait HandlerResolver: Send + Sync {
    /// Resolve a `HandlerRef` into a concrete handler.
    async fn resolve(
        &self,
        reference: &HandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, crate::error::ToolError>;
}

// =============================================================================
// Validation
// =============================================================================

/// A single validation issue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationError {
    /// JSON-style path to the offending field.
    pub path: String,
    /// Human-readable message.
    pub message: String,
    /// Severity (`error` or `warning`).
    pub severity: ValidationSeverity,
    /// Optional machine-readable code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Optional fix suggestion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Severity of a validation issue.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ValidationSeverity {
    /// Hard failure.
    Error,
    /// Soft warning, not fatal.
    Warning,
}

/// Result of validating a `ToolManagerSerializedConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    /// `true` when no `Error`-severity issues were found.
    pub valid: bool,
    /// All errors and warnings discovered.
    pub errors: Vec<ValidationError>,
    /// Convenience: subset of `errors` with `severity == warning`.
    pub warnings: Vec<ValidationError>,
    /// Number of tools in the config (top-level + package tools).
    pub tool_count: usize,
    /// Number of packages in the config.
    pub package_count: usize,
}

impl ValidationResult {
    /// Combine the per-validator counters.
    pub fn summarize(errors: Vec<ValidationError>, tool_count: usize, package_count: usize) -> Self {
        let valid = errors.iter().all(|e| e.severity != ValidationSeverity::Error);
        let warnings = errors
            .iter()
            .filter(|e| e.severity == ValidationSeverity::Warning)
            .cloned()
            .collect::<Vec<_>>();
        Self {
            valid,
            errors,
            warnings,
            tool_count,
            package_count,
        }
    }
}

// =============================================================================
// Serialized config
// =============================================================================

/// Resolver of a single handler inside a serialized `ToolConfig`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SerializedHandler {
    /// Bare string.
    Name(String),
    /// Object form.
    Ref(HandlerRef),
}

/// Description of a single tool inside a `ToolPackageConfig`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolConfig {
    /// Tool name (without namespace prefix).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema describing the expected input.
    pub input_schema: ToolInputSchema,
    /// Handler reference.
    pub handler: SerializedHandler,
    /// Whether concurrent invocations are safe.
    #[serde(default = "default_true", rename = "concurrencySafe")]
    pub concurrency_safe: bool,
    /// Optional per-invocation timeout in ms.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Optional per-tool retry count.
    #[serde(default, rename = "maxRetries")]
    pub max_retries: Option<u32>,
    /// Optional metadata.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Optional version.
    #[serde(default)]
    pub version: Option<String>,
    /// Optional tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Whether the tool is deprecated.
    #[serde(default)]
    pub deprecated: bool,
    /// Optional examples.
    #[serde(default)]
    pub examples: Option<Vec<serde_json::Value>>,
}

/// Description of a single package inside a serialized config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPackageConfig {
    /// Package name.
    pub name: String,
    /// Optional version.
    #[serde(default)]
    pub version: Option<String>,
    /// Optional namespace config.
    #[serde(default)]
    pub namespace: Option<NamespaceConfigOrBool>,
    /// Optional description.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional dependencies.
    #[serde(default)]
    pub dependencies: Option<Vec<String>>,
    /// Tools contained in the package.
    #[serde(default)]
    pub tools: Vec<ToolConfig>,
}

/// Either a `NamespaceConfig` object or a plain boolean toggle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NamespaceConfigOrBool {
    /// Boolean toggle (true = use package name as prefix).
    Bool(bool),
    /// Full namespace config.
    Object(NamespaceConfig),
}

/// Top-level manager configuration section in a serialized config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SerializedManagerConfig {
    /// Default per-invocation timeout in ms.
    #[serde(default, rename = "defaultTimeout")]
    pub default_timeout: Option<u64>,
    /// Default per-tool retry count.
    #[serde(default, rename = "defaultMaxRetries")]
    pub default_max_retries: Option<u32>,
    /// Default namespace separator.
    #[serde(default, rename = "defaultSeparator")]
    pub default_separator: Option<NamespaceSeparator>,
    /// Default conflict strategy.
    #[serde(default, rename = "conflictStrategy")]
    pub conflict_strategy: Option<ConflictStrategy>,
    /// Whether to allow override of conflicting tools.
    #[serde(default, rename = "allowOverride")]
    pub allow_override: Option<bool>,
    /// Whether to validate input schemas on every invocation.
    #[serde(default, rename = "validateSchemas")]
    pub validate_schemas: Option<bool>,
    /// Optional metadata.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// Full serialized manager config — what `manager.export_config()` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolManagerSerializedConfig {
    /// Schema version. Always `"1.0"`.
    pub version: String,
    /// Optional manager-level overrides.
    #[serde(default)]
    pub config: Option<SerializedManagerConfig>,
    /// Registered packages.
    pub packages: Vec<ToolPackageConfig>,
    /// Optional tools registered outside of any package.
    #[serde(default, rename = "standaloneTools")]
    pub standalone_tools: Option<Vec<ToolConfig>>,
}

impl ToolManagerSerializedConfig {
    /// Construct an empty config.
    pub fn empty() -> Self {
        Self {
            version: "1.0".to_string(),
            config: None,
            packages: Vec::new(),
            standalone_tools: None,
        }
    }
}

/// How the registry behaves when a name collision is detected.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConflictStrategy {
    /// Throw an error (default).
    #[default]
    Error,
    /// Log a warning and keep the first registration.
    Warn,
    /// Silently keep the first registration.
    Skip,
    /// Overwrite the previous registration.
    Override,
}

// =============================================================================
// Tool manager configuration
// =============================================================================

/// Runtime configuration for `ToolManagerImpl`.
#[derive(Clone)]
pub struct ToolManagerConfig {
    /// Default per-invocation timeout.
    pub default_timeout: Duration,
    /// Default per-tool retry count.
    pub default_max_retries: u32,
    /// Default namespace separator.
    pub default_separator: NamespaceSeparator,
    /// Default conflict resolution strategy.
    pub conflict_strategy: ConflictStrategy,
    /// Whether to allow override when conflicts occur.
    pub allow_override: bool,
    /// Whether to validate inputs against the tool's schema on every call.
    pub validate_schemas: bool,
    /// Optional pre-registered hook callbacks.
    pub hooks: Option<ToolHookCallbacks>,
    /// Optional logger.
    pub logger: Option<std::sync::Arc<dyn ToolLogger>>,
    /// Optional metadata surfaced in `manager.config()`.
    pub metadata: Option<serde_json::Value>,
    /// Optional priority ordering used when merging packages.
    pub priority: Option<PackagePriority>,
}

/// L2 default: per-tool wall-clock budget for any handler that does not
/// set its own `.timeout()`. This is the layer below the manager's
/// `tokio_timeout(60s, ...)` wrapper in `register_delegate_tool`. A 30s
/// default here silently truncates the specialist turn before the outer
/// wrapper gets a chance to react. 1500s (25 minutes) matches
/// `DEFAULT_DELEGATE_TIMEOUT_SECS` so specialist fan-outs
/// (programmer / architect / etc.) can complete long bash+search+read
/// sequences without premature aborts.
impl Default for ToolManagerConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(1500),
            default_max_retries: 3,
            default_separator: DEFAULT_NAMESPACE_SEPARATOR,
            conflict_strategy: ConflictStrategy::Error,
            allow_override: false,
            validate_schemas: true,
            hooks: None,
            logger: None,
            metadata: None,
            priority: None,
        }
    }
}

impl std::fmt::Debug for ToolManagerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolManagerConfig")
            .field("default_timeout", &self.default_timeout)
            .field("default_max_retries", &self.default_max_retries)
            .field("default_separator", &self.default_separator)
            .field("conflict_strategy", &self.conflict_strategy)
            .field("allow_override", &self.allow_override)
            .field("validate_schemas", &self.validate_schemas)
            .field("hooks", &self.hooks.as_ref().map(|_| "<callbacks>"))
            .field("logger", &self.logger.is_some())
            .field("priority", &self.priority)
            .finish()
    }
}

/// Priority ordering when merging packages from multiple configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PackagePriority {
    /// First config in the list wins.
    #[default]
    First,
    /// Last config in the list wins.
    Last,
}

// =============================================================================
// Tool manager trait
// =============================================================================

/// The trait implemented by `ToolManagerImpl`. Mirrors `ToolManager` in TS.
#[async_trait]
pub trait ToolManager: Send + Sync {
    // ----- Configuration -----
    /// Snapshot of the manager's configuration.
    fn config(&self) -> &ToolManagerConfig;

    // ----- Registration -----
    /// Register a single tool. `package_name` is optional metadata.
    fn register(&self, tool: Tool, package_name: Option<&str>);

    /// Register a single package (with namespace + lifecycle hooks).
    async fn register_package(
        &self,
        package: ToolPackage,
    ) -> Result<(), crate::error::ToolError>;

    /// Register multiple packages in order.
    async fn register_packages(
        &self,
        packages: Vec<ToolPackage>,
    ) -> Result<(), crate::error::ToolError> {
        for pkg in packages {
            self.register_package(pkg).await?;
        }
        Ok(())
    }

    /// Unregister a single tool by name.
    fn unregister(&self, name: &str);

    /// Unregister a package and all of its tools.
    async fn unregister_package(&self, name: &str) -> Result<(), crate::error::ToolError>;
    // ----- Query -----
    /// Look up a tool by name.
    fn get_tool(&self, name: &str) -> Option<Tool>;
    /// All registered tool names.
    fn get_tool_names(&self) -> Vec<String>;
    /// All registered tool definitions.
    fn get_tool_definitions(&self) -> Vec<ToolDefinition>;
    /// Whether a tool with the given name is registered.
    fn has(&self, name: &str) -> bool;
    /// Look up a package by name.
    fn get_package(&self, name: &str) -> Option<ToolPackage>;
    /// All registered package names.
    fn get_package_names(&self) -> Vec<String>;
    /// Resolve a tool name into its namespace / original name.
    fn resolve_tool_name(&self, name: &str) -> Option<ResolvedToolName>;

    // ----- Execution -----
    /// Execute a tool by name.
    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        context: Option<ToolExecutionContext>,
    ) -> Result<serde_json::Value, crate::error::ToolError>;

    // ----- Hooks -----
    /// Register a hook for a single event.
    fn on(&self, event: ToolHookEvent, callback: HookFn);
    /// Register a hook for a single event with extra options.
    fn on_with_options(
        &self,
        event: ToolHookEvent,
        callback: HookFn,
        options: HookRegistrationOptions,
    );
    /// Unregister hooks. When `callback` is `None`, all hooks for the event are removed.
    fn off(&self, event: ToolHookEvent, callback: Option<HookFn>);
    /// Register all callbacks from a `ToolHookCallbacks` object.
    fn register_hooks(&self, callbacks: ToolHookCallbacks);
    /// Remove every registered hook.
    fn clear_hooks(&self);

    // ----- Discovery -----
    /// Find every tool in the given namespace.
    fn find_tools_by_namespace(&self, namespace: &str) -> Vec<Tool>;
    /// Find every tool whose `metadata[key]` equals `value`.
    fn find_tools_by_metadata(&self, key: &str, value: &serde_json::Value) -> Vec<Tool>;
    /// Find every tool with the given tag.
    fn find_tools_by_tag(&self, tag: &str) -> Vec<Tool>;

    // ----- Scope creation -----
    /// Create a new manager containing only the named tools.
    fn create_scope(&self, tool_names: Vec<String>) -> Box<dyn ToolManager>;
    /// Create a new manager containing only the tools in a namespace.
    fn create_namespace_scope(&self, namespace: &str) -> Box<dyn ToolManager>;

    // ----- Serialization -----
    /// Export the current configuration.
    fn export_config(&self) -> ToolManagerSerializedConfig;
    /// Destroy the manager and release its resources.
    async fn destroy(&self);
}

// =============================================================================
// Tool registry trait
// =============================================================================

/// Lower-level registry contract (no manager-level state).
#[async_trait]
pub trait ToolRegistry: Send + Sync {
    /// Register a single tool.
    fn register(&self, tool: Tool, package_name: Option<&str>);
    /// Register a whole package.
    async fn register_package(
        &self,
        package: ToolPackage,
    ) -> Result<(), crate::error::ToolError>;
    /// Unregister a single tool.
    fn unregister(&self, name: &str);
    /// Unregister a whole package.
    async fn unregister_package(
        &self,
        name: &str,
    ) -> Result<(), crate::error::ToolError>;
    /// Look up a tool.
    fn get_tool(&self, name: &str) -> Option<Tool>;
    /// All tool names.
    fn get_tool_names(&self) -> Vec<String>;
    /// All tool definitions.
    fn get_tool_definitions(&self) -> Vec<ToolDefinition>;
    /// Whether the given name is registered.
    fn has(&self, name: &str) -> bool;
    /// Look up a package.
    fn get_package(&self, name: &str) -> Option<ToolPackage>;
    /// All package names.
    fn get_package_names(&self) -> Vec<String>;
    /// Resolve a tool name into its namespace / original name.
    fn resolve_tool_name(&self, name: &str) -> Option<ResolvedToolName>;
    /// Execute a registered tool.
    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        context: Option<ToolExecutionContext>,
    ) -> Result<serde_json::Value, crate::error::ToolError>;
}

// =============================================================================
// Logger
// =============================================================================

/// Logger contract used by the manager. Methods are all optional.
pub trait ToolLogger: Send + Sync + std::fmt::Debug {
    /// Verbose debugging info.
    fn debug(&self, _message: &str, _args: &[serde_json::Value]) {}
    /// Informational message.
    fn info(&self, _message: &str, _args: &[serde_json::Value]) {}
    /// Warning.
    fn warn(&self, _message: &str, _args: &[serde_json::Value]) {}
    /// Error.
    fn error(&self, _message: &str, _args: &[serde_json::Value]) {}
}

// =============================================================================
// Diff
// =============================================================================

/// A single field-level change between two configs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigFieldChange {
    /// Field name.
    pub field: String,
    /// Previous value.
    #[serde(rename = "oldValue")]
    pub old_value: Option<serde_json::Value>,
    /// New value.
    #[serde(rename = "newValue")]
    pub new_value: Option<serde_json::Value>,
}

/// Per-tool change entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModifiedTool {
    /// Tool name.
    pub name: String,
    /// Previous definition.
    #[serde(rename = "old")]
    pub old: ToolConfig,
    /// New definition.
    #[serde(rename = "new")]
    pub new: ToolConfig,
    /// Per-field changes.
    pub changes: Vec<ConfigFieldChange>,
}

/// Result of `tool_manager_factory::diff_configs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDiff {
    /// Tools present in `B` but not in `A`.
    pub added: Vec<ToolConfig>,
    /// Tools present in `A` but not in `B`.
    pub removed: Vec<ToolConfig>,
    /// Tools present in both with different definitions.
    pub modified: Vec<ModifiedTool>,
    /// Packages present in `B` but not in `A`.
    #[serde(rename = "addedPackages")]
    pub added_packages: Vec<ToolPackageConfig>,
    /// Packages present in `A` but not in `B`.
    #[serde(rename = "removedPackages")]
    pub removed_packages: Vec<ToolPackageConfig>,
    /// Per-package changes.
    #[serde(rename = "modifiedPackages")]
    pub modified_packages: Vec<ModifiedTool>,
    /// Top-level config field changes.
    #[serde(rename = "configChanges")]
    pub config_changes: Option<Vec<ConfigFieldChange>>,
    /// `true` when `added`/`removed`/`modified` are all empty.
    pub identical: bool,
}

// =============================================================================
// Factory
// =============================================================================

/// Options passed to `create_from_config`.
pub struct CreateFromConfigOptions {
    /// The resolver used to inflate `HandlerRef`s.
    pub handler_resolver: std::sync::Arc<dyn HandlerResolver>,
    /// Validate the config first. Default: `true`.
    pub validate: bool,
    /// When set, the new packages/tools are merged into this existing manager.
    pub merge_into: Option<std::sync::Arc<crate::core::ToolManagerImpl>>,
    /// Optional config overrides applied to `merge_into` (or a fresh manager).
    pub config_overrides: Option<ToolManagerConfig>,
    /// Skip invoking `on_init` on each package.
    pub skip_init_hooks: bool,
}

/// Options for `to_config` (currently a no-op placeholder for parity with TS).
#[derive(Clone, Default)]
pub struct SerializationOptions {
    /// Placeholder.
    pub handler_mode: Option<String>,
    /// Placeholder.
    pub handler_id_resolver: Option<std::sync::Arc<dyn Fn(&ToolHandler, &str) -> Option<String>>>,
    /// Placeholder.
    pub include_metadata: bool,
}

/// Options for `merge_configs`.
#[derive(Debug, Clone, Default)]
pub struct MergeOptions {
    /// Conflict resolution strategy for tool-level duplicates.
    pub conflict_strategy: Option<ConflictStrategy>,
    /// Priority ordering when packages share a name.
    pub priority_order: Option<PackagePriority>,
    /// Placeholder for hook merging.
    pub merge_hooks: bool,
    /// Placeholder for metadata merging.
    pub merge_metadata: bool,
}
