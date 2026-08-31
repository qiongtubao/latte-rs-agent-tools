use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};

fn prop(ty: PropertyType, desc: &str) -> ToolInputProperty {
    ToolInputProperty { property_type: ty, description: Some(desc.into()), enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None, items: None, properties: None, required: None, additional_properties: None }
}

fn all_props_connect() -> ToolInputSchema {
    let mut p = BTreeMap::new();
    p.insert("command".into(), prop(PropertyType::String, "Shell command to start the MCP server"));
    ToolInputSchema { schema_type: Default::default(), properties: p, required: Some(vec!["command".into()]), additional_properties: None }
}

fn all_props_call() -> ToolInputSchema {
    let mut p = BTreeMap::new();
    p.insert("tool".into(), prop(PropertyType::String, "Name of the MCP tool to call"));
    p.insert(
        "arguments".into(),
        prop(PropertyType::Object, "JSON arguments for the tool")
            .with_additional_properties(true),
    );
    p.insert("server_index".into(), prop(PropertyType::Integer, "Server index (default: 0)"));
    ToolInputSchema { schema_type: Default::default(), properties: p, required: Some(vec!["tool".into(), "arguments".into()]), additional_properties: None }
}

fn all_props_list() -> ToolInputSchema {
    ToolInputSchema { schema_type: Default::default(), properties: BTreeMap::new(), required: None, additional_properties: None }
}

static MCP_CONNS: std::sync::LazyLock<Mutex<Vec<(String, Child)>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

fn mcp_list_tool() -> Tool {
    let h: Arc<dyn Fn(Value, ToolExecutionContext) -> futures::future::BoxFuture<'static, Result<Value, crate::error::ToolError>> + Send + Sync> = Arc::new(|_, _| {
        async move {
            let conns = MCP_CONNS.lock().await;
            let servers: Vec<Value> = conns.iter().map(|(c, _)| json!({"server": c})).collect();
            Ok(json!({"servers": servers, "count": servers.len()}))
        }.boxed()
    });
    Tool::builder("mcp_list", "List connected MCP servers", all_props_list(), h).build()
}

fn mcp_connect_tool() -> Tool {
    let h: Arc<dyn Fn(Value, ToolExecutionContext) -> futures::future::BoxFuture<'static, Result<Value, crate::error::ToolError>> + Send + Sync> = Arc::new(|input, _| {
        async move {
            let cmd = input.get("command").and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("command required"))?;
            let mut parts = cmd.split_whitespace();
            let program = parts.next().unwrap_or("npx");
            let args: Vec<&str> = parts.collect();

            let mut child = Command::new(program).args(&args)
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
                .spawn().map_err(|e| crate::error::ToolError::other(format!("spawn: {e}")))?;

            let mut stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let mut reader = BufReader::new(stdout).lines();

            let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"0.1.0","capabilities":{},"clientInfo":{"name":"latte-agent","version":"0.1.0"}}});
            stdin.write_all(format!("{}\n", serde_json::to_string(&init).unwrap()).as_bytes()).await.ok();
            stdin.flush().await.ok();
            let _ = reader.next_line().await;

            let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}});
            stdin.write_all(format!("{}\n", serde_json::to_string(&list).unwrap()).as_bytes()).await.ok();
            stdin.flush().await.ok();
            let list_resp = reader.next_line().await.ok().flatten().unwrap_or_default();

            let tools: Vec<Value> = serde_json::from_str::<Value>(&list_resp).ok()
                .and_then(|v| v.get("result").and_then(|r| r.get("tools")).and_then(|t| t.as_array().cloned()))
                .unwrap_or_default();

            MCP_CONNS.lock().await.push((cmd.to_string(), child));

            Ok(json!({"server":cmd,"tools_found":tools.len(),"tools":tools.iter().map(|t|json!({"name":t.get("name"),"description":t.get("description")})).collect::<Vec<_>>(),"status":"connected"}))
        }.boxed()
    });
    Tool::builder("mcp_connect", "Connect to an MCP server and discover tools", all_props_connect(), h).build()
}

fn mcp_call_tool() -> Tool {
    let h: Arc<dyn Fn(Value, ToolExecutionContext) -> futures::future::BoxFuture<'static, Result<Value, crate::error::ToolError>> + Send + Sync> = Arc::new(|input, _| {
        async move {
            let tool_name = input.get("tool").and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("tool name required"))?;
            let arguments = input.get("arguments").cloned().unwrap_or(json!({}));
            let idx = input.get("server_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let mut conns = MCP_CONNS.lock().await;
            let (_, child) = conns.get_mut(idx)
                .ok_or_else(|| crate::error::ToolError::other("no server; use mcp_connect first"))?;

            let stdin = child.stdin.as_mut().unwrap();
            let stdout = child.stdout.as_mut().unwrap();

            let req = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":tool_name,"arguments":arguments}});
            stdin.write_all(format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes()).await
                .map_err(|e| crate::error::ToolError::other(format!("write: {e}")))?;
            stdin.flush().await.ok();

            let mut reader = BufReader::new(stdout).lines();
            let resp = reader.next_line().await.ok().flatten().unwrap_or_default();
            match serde_json::from_str::<Value>(&resp) {
                Ok(v) => Ok(v.get("result").cloned().unwrap_or(v)),
                Err(_) => Ok(json!({"raw": resp})),
            }
        }.boxed()
    });
    Tool::builder("mcp_call", "Call a tool on a connected MCP server", all_props_call(), h).build()
}

pub struct McpToolsPackage;
impl McpToolsPackage {
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "mcp".into(), version: Some("1.0.0".into()),
            namespace: None,
            description: Some("MCP protocol: connect to servers, list and call tools".into()),
            dependencies: None, tools: vec![mcp_list_tool(), mcp_connect_tool(), mcp_call_tool()],
            on_init: None, on_destroy: None, before_execute: None, after_execute: None,
            metadata: Some(json!({"category": "protocol", "tags": ["mcp", "protocol"]})),
        }
    }
}
impl Default for McpToolsPackage { fn default() -> Self { Self } }
