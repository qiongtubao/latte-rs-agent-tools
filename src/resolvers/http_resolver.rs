//! HTTP handler resolver. Mirrors the TS `HttpHandlerResolver`.

use futures::FutureExt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;

use crate::error::ToolError;
use crate::types::{HandlerRef, HandlerResolver, HttpHandlerRef, HttpMethod, SharedToolHandler, ToolExecutionContext};

/// Executes tools via HTTP requests.
pub struct HttpHandlerResolver {
    client: Client,
    default_timeout: Duration,
}

impl HttpHandlerResolver {
    /// Construct a new resolver.
    pub fn new(default_timeout: Duration) -> Self {
        let client = Client::builder()
            .timeout(default_timeout)
            .build()
            .expect("reqwest client");
        Self {
            client,
            default_timeout,
        }
    }

    /// Construct a resolver with a custom `reqwest::Client`.
    pub fn with_client(client: Client, default_timeout: Duration) -> Self {
        Self {
            client,
            default_timeout,
        }
    }

    /// Build the concrete handler for an HTTP `HandlerRef`.
    pub async fn resolve(
        &self,
        reference: &HandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, ToolError> {
        let http_ref = match reference {
            HandlerRef::Http(r) => r.clone(),
            other => {
                return Err(ToolError::handler(
                    tool_name,
                    format!("Not an HTTP handler reference: {:?}", other),
                ));
            }
        };
        let handler = build_http_handler(self.client.clone(), http_ref, tool_name.to_string());
        Ok(Arc::new(handler))
    }
}

impl std::fmt::Debug for HttpHandlerResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpHandlerResolver")
            .field("default_timeout", &self.default_timeout)
            .finish()
    }
}

fn build_http_handler(
    client: Client,
    ref_: HttpHandlerRef,
    tool_name: String,
) -> impl Fn(serde_json::Value, ToolExecutionContext) -> futures::future::BoxFuture<'static, Result<serde_json::Value, ToolError>>
       + Send
       + Sync
       + 'static {
    move |input: serde_json::Value, _ctx: ToolExecutionContext| {
        let client = client.clone();
        let ref_ = ref_.clone();
        let tool_name = tool_name.clone();
        (async move { execute_http(&client, &ref_, &input, &tool_name).await }).boxed()
    }
}

async fn execute_http(
    client: &Client,
    ref_: &HttpHandlerRef,
    input: &serde_json::Value,
    tool_name: &str,
) -> Result<serde_json::Value, ToolError> {
    // Build body
    let body: serde_json::Value = if let Some(template) = ref_.body_template.as_ref() {
        if ref_.include_input {
            let mut obj = match template {
                serde_json::Value::Object(m) => m.clone(),
                _ => serde_json::Map::new(),
            };
            if let Some(input_obj) = input.as_object() {
                for (k, v) in input_obj {
                    obj.insert(k.clone(), v.clone());
                }
            }
            serde_json::Value::Object(obj)
        } else {
            template.clone()
        }
    } else if ref_.include_input {
        input.clone()
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };

    // Method
    let method = match ref_.method {
        HttpMethod::Get => reqwest::Method::GET,
        HttpMethod::Post => reqwest::Method::POST,
        HttpMethod::Put => reqwest::Method::PUT,
        HttpMethod::Delete => reqwest::Method::DELETE,
        HttpMethod::Patch => reqwest::Method::PATCH,
    };

    let mut request = client.request(method, &ref_.url);
    for (k, v) in &ref_.headers {
        request = request.header(k, v);
    }
    if !matches!(ref_.method, HttpMethod::Get) {
        request = request.json(&body);
    }

    let response = request.send().await.map_err(|e| {
        ToolError::execution_str(tool_name, format!("HTTP request failed: {}", e))
    })?;

    if !response.status().is_success() {
        return Err(ToolError::execution_str(
            tool_name,
            format!("HTTP {} {}", response.status().as_u16(), response.status()),
        ));
    }

    let mut json: serde_json::Value = response.json().await.map_err(|e| {
        ToolError::execution_str(tool_name, format!("Failed to parse JSON response: {}", e))
    })?;

    if let Some(path) = ref_.response_path.as_deref() {
        json = extract_path(&json, path);
    }
    Ok(json)
}

fn extract_path(value: &serde_json::Value, path: &str) -> serde_json::Value {
    let mut current = value;
    for part in path.split('.') {
        match current {
            serde_json::Value::Object(map) => {
                if let Some(next) = map.get(part) {
                    current = next;
                } else {
                    return serde_json::Value::Null;
                }
            }
            _ => return serde_json::Value::Null,
        }
    }
    current.clone()
}

#[async_trait]
impl HandlerResolver for HttpHandlerResolver {
    async fn resolve(
        &self,
        reference: &HandlerRef,
        tool_name: &str,
    ) -> Result<SharedToolHandler, ToolError> {
        HttpHandlerResolver::resolve(self, reference, tool_name).await
    }
}
