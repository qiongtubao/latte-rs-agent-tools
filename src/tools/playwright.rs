//! Playwright tool — browser automation via Playwright/Chromium.
//!
//! Takes screenshots of web pages using headless Chromium,
//! returning the image as a base64 data URI for vision models.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::error::ToolError;
use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};

fn prop(ty: PropertyType, desc: &str) -> ToolInputProperty {
    ToolInputProperty { property_type: ty, description: Some(desc.into()), enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None }
}

fn all_props() -> ToolInputSchema {
    let mut p = BTreeMap::new();
    p.insert("url".into(), prop(PropertyType::String, "URL to screenshot (default: http://localhost:4567)"));
    p.insert("width".into(), prop(PropertyType::Integer, "Viewport width (default: 1920)"));
    p.insert("height".into(), prop(PropertyType::Integer, "Viewport height (default: 1080)"));
    ToolInputSchema { schema_type: Default::default(), properties: p, required: None, additional_properties: None }
}

fn screenshot_tool() -> Tool {
    let handler: Arc<dyn Fn(Value, ToolExecutionContext) -> futures::future::BoxFuture<'static, Result<Value, ToolError>> + Send + Sync> = Arc::new(|input: Value, _ctx: ToolExecutionContext| {
        async move {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("http://localhost:4567/");
            let w = input.get("width").and_then(|v| v.as_u64()).unwrap_or(1920);
            let h = input.get("height").and_then(|v| v.as_u64()).unwrap_or(1080);
            let out = "/tmp/latte-screenshot.png";

            let chrome = ["/usr/bin/google-chrome","/usr/bin/chromium-browser","/usr/bin/chromium"]
                .iter().find(|p| std::path::Path::new(p).exists())
                .ok_or_else(|| ToolError::other("no chromium found; install chromium-browser"))?;

            let status = Command::new(chrome)
                .args(["--headless","--disable-gpu","--no-sandbox","--disable-dev-shm-usage",
                       &format!("--screenshot={out}"), &format!("--window-size={w},{h}"), "--hide-scrollbars", url])
                .stdout(Stdio::null()).stderr(Stdio::null())
                .status().await.map_err(|e| ToolError::other(format!("chromium error: {e}")))?;

            if !status.success() {
                return Err(ToolError::other(format!("chromium exited {:?}", status.code())));
            }

            let data = tokio::fs::read(&out).await.map_err(|e| ToolError::other(format!("read: {e}")))?;
            // Simple hex encoding as fallback (base64 not available in this crate)
            let hex_str: String = data.iter().map(|b| format!("{:02x}", b)).collect();
            Ok(json!({
                "path": out,
                "size_bytes": data.len(),
                "width": w,
                "height": h,
                "hex_data": hex_str,
            }))
        }.boxed()
    });

    Tool::builder("screenshot", "Take a screenshot of a web page using headless Chromium. Returns image dimensions and hex-encoded pixel data.", all_props(), handler)
        .concurrency_safe(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

fn script_tool() -> Tool {
    let handler: Arc<dyn Fn(Value, ToolExecutionContext) -> futures::future::BoxFuture<'static, Result<Value, ToolError>> + Send + Sync> = Arc::new(|input: Value, _ctx: ToolExecutionContext| {
        async move {
            let script = input.get("script").and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::other("script required"))?;
            let tmp = "/tmp/latte-pw.mjs";
            tokio::fs::write(tmp, script).await.map_err(|e| ToolError::other(format!("write: {e}")))?;
            let out = Command::new("node").arg(tmp)
                .stdout(Stdio::piped()).stderr(Stdio::piped())
                .output().await.map_err(|e| ToolError::other(format!("run: {e}")))?;
            Ok(json!({"stdout":String::from_utf8_lossy(&out.stdout),"stderr":String::from_utf8_lossy(&out.stderr),"exit_code":out.status.code().unwrap_or(-1)}))
        }.boxed()
    });

    let mut p2 = BTreeMap::new();
    p2.insert("script".into(), prop(PropertyType::String, "Full Node.js Playwright script to execute"));
    Tool::builder("playwright_script", "Execute a custom Playwright Node.js script. Returns stdout/stderr.", ToolInputSchema { schema_type: Default::default(), properties: p2, required: Some(vec!["script".into()]), additional_properties: None }, handler)
        .concurrency_safe(true)
        .timeout(std::time::Duration::from_secs(60))
        .build()
}

/// Playwright tool package.
pub struct PlaywrightToolsPackage;

impl PlaywrightToolsPackage {
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "playwright".into(),
            version: Some("1.0.0".into()),
            namespace: None,
            description: Some("Playwright browser automation: screenshots and script execution".into()),
            dependencies: None,
            tools: vec![screenshot_tool(), script_tool()],
            on_init: None,
            on_destroy: None,
            before_execute: None,
            after_execute: None,
            metadata: Some(json!({"category": "testing", "tags": ["playwright", "screenshot", "browser"]})),
        }
    }
}

impl Default for PlaywrightToolsPackage {
    fn default() -> Self { Self }
}
