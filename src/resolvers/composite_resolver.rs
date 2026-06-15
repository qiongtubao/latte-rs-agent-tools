//! Composite handler resolver. Mirrors the TS `CompositeHandlerResolver`.
//!
//! Composes builtin, http, and script resolvers and additionally resolves
//! `composite` chains with `sequence` / `parallel` / `merge` combine modes.

use futures::FutureExt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::ToolError;
use crate::resolvers::{
    BuiltinHandlerResolver, HttpHandlerResolver, ScriptHandlerResolver,
};
use crate::types::{
    CombineMode, CompositeHandlerRef, HandlerRef, HandlerResolver, SharedToolHandler,
    ToolExecutionContext,
};

/// Unified resolver: dispatches on `HandlerRef::type` and supports chains.
pub struct CompositeHandlerResolver {
    builtin: Arc<BuiltinHandlerResolver>,
    http: HttpHandlerResolver,
    script: ScriptHandlerResolver,
}

impl CompositeHandlerResolver {
    /// Construct with default timeout.
    pub fn new(default_timeout: Duration) -> Self {
        Self {
            builtin: Arc::new(BuiltinHandlerResolver::new()),
            http: HttpHandlerResolver::new(default_timeout),
            script: ScriptHandlerResolver::new(default_timeout),
        }
    }

    /// Access the inner builtin resolver (for direct registration).
    pub fn builtin(&self) -> &Arc<BuiltinHandlerResolver> {
        &self.builtin
    }

    /// Register a builtin handler.
    pub fn register(&self, name: impl Into<String>, handler: SharedToolHandler) {
        self.builtin.register(name, handler);
    }

    /// Get a registered builtin handler.
    pub fn get(&self, name: &str) -> Option<SharedToolHandler> {
        self.builtin.get(name)
    }

    /// Whether a builtin handler is registered.
    pub fn has(&self, name: &str) -> bool {
        self.builtin.has(name)
    }

    /// Remove a builtin handler.
    pub fn unregister(&self, name: &str) {
        self.builtin.unregister(name);
    }

    /// All builtin handler names.
    pub fn names(&self) -> Vec<String> {
        self.builtin.names()
    }

    /// Dispatch on the reference type.
    pub async fn resolve_ref(
        &self,
        reference: &HandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, ToolError> {
        match reference {
            HandlerRef::BuiltinName(name) => {
                self.builtin.get(name).ok_or_else(|| {
                    ToolError::handler(tool_name, format!("Handler not found: {}", name))
                })
            }
            HandlerRef::Builtin(_) => self.builtin.resolve_ref(reference, tool_name).await,
            HandlerRef::Http(_) => self.http.resolve(reference, tool_name).await,
            HandlerRef::Script(_) => self.script.resolve(reference, tool_name).await,
            HandlerRef::Composite(cref) => self.resolve_composite(cref.clone(), tool_name).await,
        }
    }

    async fn resolve_composite(
        &self,
        ref_: CompositeHandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, ToolError> {
        let mut handlers: Vec<SharedToolHandler> = Vec::with_capacity(ref_.chain.len());
        for sub in &ref_.chain {
            handlers.push(Box::pin(self.resolve_ref(sub, tool_name)).await?);
        }
        let combine = ref_.combine;
        let stop_on_error = ref_.stop_on_error;
        let chain: Vec<SharedToolHandler> = handlers;
        let tool_name = tool_name.to_string();
        let handler: SharedToolHandler = Arc::new(
            move |input: serde_json::Value, ctx: ToolExecutionContext| {
                let chain = chain.clone();
                let tool_name = tool_name.clone();
                (async move { run_chain(&chain, combine, stop_on_error, input, ctx, &tool_name).await }).boxed()
            },
        );
        Ok(handler)
    }
}

impl std::fmt::Debug for CompositeHandlerResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeHandlerResolver")
            .field("builtin", &self.builtin)
            .finish()
    }
}

async fn run_chain(
    chain: &[SharedToolHandler],
    combine: CombineMode,
    stop_on_error: bool,
    input: serde_json::Value,
    _ctx: ToolExecutionContext,
    _tool_name: &str,
) -> Result<serde_json::Value, ToolError> {
    match combine {
        CombineMode::Sequence => {
            let mut current: serde_json::Value = input;
            for handler in chain {
                let next = handler(current.clone(), _ctx.clone()).await;
                match next {
                    Ok(v) => current = v,
                    Err(e) => {
                        if stop_on_error {
                            return Err(e);
                        }
                    }
                }
            }
            Ok(current)
        }
        CombineMode::Parallel => {
            let mut futs = Vec::with_capacity(chain.len());
            for h in chain {
                futs.push(h(input.clone(), _ctx.clone()));
            }
            let results = futures::future::try_join_all(futs).await?;
            Ok(serde_json::to_value(results).unwrap_or(serde_json::Value::Null))
        }
        CombineMode::Merge => {
            let mut futs = Vec::with_capacity(chain.len());
            for h in chain {
                futs.push(h(input.clone(), _ctx.clone()));
            }
            let results = futures::future::try_join_all(futs).await?;
            let mut merged = serde_json::Map::new();
            for r in results {
                if let serde_json::Value::Object(map) = r {
                    for (k, v) in map {
                        merged.insert(k, v);
                    }
                }
            }
            Ok(serde_json::Value::Object(merged))
        }
    }
}

#[async_trait]
impl HandlerResolver for CompositeHandlerResolver {
    async fn resolve(
        &self,
        reference: &HandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, ToolError> {
        CompositeHandlerResolver::resolve_ref(self, reference, tool_name).await
    }
}
