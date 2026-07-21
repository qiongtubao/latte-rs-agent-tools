//! Screenshot tool — captures browser/display screenshots via
//! headless Chromium (Playwright-compatible). Returns the image
//! as a base64 data URI so the model can "see" the current UI state.
//!
//! Two capture modes:
//!   - `url`: navigate to URL, screenshot the page (default)
//!   - `viewport`: screenshot the current display (requires Xvfb/X11)
//!
//! The tool is registered as `screenshot` and exposed to roles that
//! list it in their `allowed_tools` config.

use std::collections::BTreeMap;
use std::process::Stdio;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::process::Command;

use crate::error::ToolError;
use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};

fn prop(ty: PropertyType, desc: &str) -> ToolInputProperty {
    ToolInputProperty {
        property_type: ty,
        description: Some(desc.into()),
        enum_values: None,
        minimum: None,
        maximum: None,
        min_length: None,
        max_length: None,
    }
}

fn required(props: Vec<(&str, PropertyType, &str)>) -> ToolInputSchema {
    let mut p = BTreeMap::new();
    let mut req = Vec::new();
    for (name, ty, desc) in props {
        p.insert(name.to_string(), prop(ty, desc));
        req.push(name.to_string());
    }
    ToolInputSchema {
        schema_type: Default::default(),
        properties: p,
        required: Some(req),
        additional_properties: None,
    }
}

/// Try to find a Chromium/Chrome binary on the system.
fn resolve_chrome() -> Option<String> {
    // Priority: env var → known paths
    if let Ok(path) = std::env::var("LATTE_AGENT_CHROME") {
        if !path.is_empty() {
            return Some(path);
        }
    }
    for p in &[
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium-browser",
        "/usr/bin/chromium",
        "/snap/bin/chromium",
    ] {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    // Check playwright's bundled chromium
    let home = std::env::var("HOME").unwrap_or_default();
    let playwright_chrome = format!("{home}/.cache/ms-playwright/chromium-*/chrome-linux/chrome");
    if let Ok(entries) = glob::glob(&playwright_chrome) {
        for entry in entries.flatten() {
            return Some(entry.to_string_lossy().into_owned());
        }
    }
    None
}

fn screenshot_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let url = input.get("url").and_then(|v| v.as_str());
            let output_path = input
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("/tmp/latte-screenshot.png");

            let chrome = resolve_chrome()
                .ok_or_else(|| ToolError::other("no chromium/chrome found; install chromium or set LATTE_AGENT_CHROME"))?;

            // Build chromium args
            let mut cmd = Command::new(&chrome);
            cmd.args([
                "--headless",
                "--disable-gpu",
                "--no-sandbox",
                "--disable-dev-shm-usage",
                "--disable-software-rasterizer",
                "--screenshot",
                &format!("--window-size=1920,1080"),
            ]);

            if let Some(target_url) = url {
                cmd.arg(target_url);
            } else {
                // Default: screenshot localhost:4567 (latte-agent ui)
                cmd.arg("http://localhost:4567/");
            }

            // Redirect output to file
            cmd.arg(&output_path);
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());

            let status = cmd.status().await.map_err(|e| {
                ToolError::other(format!("failed to run chromium: {e}"))
            })?;

            if !status.success() {
                return Err(ToolError::other(format!(
                    "chromium exited with code {:?}",
                    status.code()
                )));
            }

            // Read the screenshot and encode as base64
            let image_data = tokio::fs::read(&output_path).await.map_err(|e| {
                ToolError::other(format!("failed to read screenshot: {e}"))
            })?;

            let b64 = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &image_data,
            );
            let data_uri = format!("data:image/png;base64,{b64}");

            Ok(json!({
                "path": output_path,
                "size_bytes": image_data.len(),
                "data_uri": data_uri,
                "width": 1920,
                "height": 1080,
                "format": "png",
            }))
        }
        .boxed()
    };

    Tool::builder(
        "screenshot",
        "Capture a browser screenshot via headless Chromium. Returns the image as a base64 data URI so you can 'see' the current UI state. Use this to verify UI changes, check page layout, or inspect rendered content.",
        required(vec![
            ("url", PropertyType::String, "URL to screenshot (default: http://localhost:4567)"),
        ]),
    )
    .optional("output", PropertyType::String, "File path to save the screenshot (default: /tmp/latte-screenshot.png)")
    .optional("width", PropertyType::Integer, "Viewport width (default: 1920)")
    .optional("height", PropertyType::Integer, "Viewport height (default: 1080)")
    .handler(handler)
    .build()
}

/// Tool package exposing screenshot capabilities.
pub struct ScreenshotToolsPackage;

impl ScreenshotToolsPackage {
    pub fn new() -> Self {
        Self
    }
}

impl ToolPackage for ScreenshotToolsPackage {
    fn name(&self) -> &str {
        "screenshot"
    }

    fn tools(&self) -> Vec<Tool> {
        vec![screenshot_tool()]
    }
}
