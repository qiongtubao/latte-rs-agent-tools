//! Built-in handler resolver. Mirrors the TS `BuiltinHandlerResolver`.

use futures::FutureExt;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::error::ToolError;
use crate::types::{
    BuiltinHandlerRef, HandlerRef, HandlerResolver, SharedToolHandler, ToolExecutionContext,
};

/// Maps string IDs to handlers.
pub struct BuiltinHandlerResolver {
    inner: RwLock<HashMap<String, SharedToolHandler>>,
}

impl BuiltinHandlerResolver {
    /// Construct an empty resolver.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Register a handler by name.
    pub fn register(&self, name: impl Into<String>, handler: SharedToolHandler) {
        self.inner
            .write()
            .expect("poisoned lock")
            .insert(name.into(), handler);
    }

    /// Get a registered handler.
    pub fn get(&self, name: &str) -> Option<SharedToolHandler> {
        self.inner.read().expect("poisoned lock").get(name).cloned()
    }

    /// Whether the named handler is registered.
    pub fn has(&self, name: &str) -> bool {
        self.inner.read().expect("poisoned lock").contains_key(name)
    }

    /// Remove a registered handler.
    pub fn unregister(&self, name: &str) {
        self.inner.write().expect("poisoned lock").remove(name);
    }

    /// All registered handler names.
    pub fn names(&self) -> Vec<String> {
        self.inner
            .read()
            .expect("poisoned lock")
            .keys()
            .cloned()
            .collect()
    }

    /// Resolve a `HandlerRef::BuiltinName` or `HandlerRef::Builtin`.
    pub async fn resolve_ref(
        &self,
        reference: &HandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, ToolError> {
        match reference {
            HandlerRef::BuiltinName(name) => self.get(name).ok_or_else(|| {
                ToolError::handler(tool_name, format!("Handler not found: {}", name))
            }),
            HandlerRef::Builtin(BuiltinHandlerRef { name, params, .. }) => {
                let handler = self.get(name).ok_or_else(|| {
                    ToolError::handler(tool_name, format!("Handler not found: {}", name))
                })?;
                if let Some(params) = params.as_ref() {
                    let params = params.clone();
                    let inner = handler.clone();
                    let wrapped: SharedToolHandler = Arc::new(
                        move |input: serde_json::Value,
                              ctx: ToolExecutionContext|
                              -> futures::future::BoxFuture<
                            'static,
                            Result<serde_json::Value, ToolError>,
                        > {
                            let merged = merge_params(&params, &input);
                            let inner = inner.clone();
                            (async move { inner(merged, ctx).await }).boxed()
                        },
                    );
                    Ok(wrapped)
                } else {
                    Ok(handler)
                }
            }
            other => Err(ToolError::handler(
                tool_name,
                format!("Not a builtin handler reference: {:?}", other),
            )),
        }
    }
}

impl Default for BuiltinHandlerResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BuiltinHandlerResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = self.names();
        f.debug_struct("BuiltinHandlerResolver")
            .field("names", &names)
            .finish()
    }
}

#[async_trait]
impl HandlerResolver for BuiltinHandlerResolver {
    async fn resolve(
        &self,
        reference: &HandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, ToolError> {
        self.resolve_ref(reference, tool_name).await
    }
}

fn merge_params(params: &serde_json::Value, input: &serde_json::Value) -> serde_json::Value {
    match (params, input) {
        (serde_json::Value::Object(p), serde_json::Value::Object(i)) => {
            let mut merged = serde_json::Map::with_capacity(p.len() + i.len());
            for (k, v) in p {
                merged.insert(k.clone(), v.clone());
            }
            for (k, v) in i {
                merged.insert(k.clone(), v.clone());
            }
            serde_json::Value::Object(merged)
        }
        _ => input.clone(),
    }
}
