//! HTTP fetch tool package. Mirrors the `fetch` tool in oh-my-pi/coding-agent.
//!
//! 提供 `fetch` 工具：发起 HTTP/HTTPS 请求并返回响应内容。
//! 与 `bash`（走 shell）不同，本工具直接通过 `reqwest` 发起请求，
//! 适用于 agent 需要联网拉取文档 / 调 API 的场景。
//!
//! ## 输入
//!
//! 字段如下，所有字段除 `url` 外都可选：
//!
//! - `url` (string, 必填) — 完整 URL，支持 `http://` 与 `https://`。
//! - `method` (string, 默认 `GET`) — 大写 HTTP 方法。
//! - `headers` (object<string,string>, 默认空) — 自定义请求头。
//! - `body` (string, 默认空) — 请求体（PUT/POST/PATCH 时使用）。
//! - `timeoutMs` (number, 默认 30000) — 总超时（连接 + 读取），毫秒。
//! - `maxSize` (number, 默认 10485760 = 10MB) — 响应体最大字节数。
//!   超出后会被截断并把 `truncated` 标记为 `true`。
//! - `followRedirects` (bool, 默认 `true`) — 是否跟随 3xx 跳转。
//!
//! ## 输出
//!
//! ```jsonc
//! {
//!   "url": "https://example.com/",        // 原始请求 URL
//!   "finalUrl": "https://example.com/",   // 跟随重定向后的最终 URL
//!   "status": 200,
//!   "statusText": "200 OK",
//!   "contentType": "text/html; charset=utf-8",
//!   "headers": { "...": "..." },
//!   "body": "...",
//!   "encoding": "utf-8" | "base64",        // 二进制内容时为 base64
//!   "size": 12345,                         // body 实际字节数
//!   "truncated": false,
//!   "elapsedMs": 42                        // 实际耗时
//! }
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use futures::FutureExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder, Method, StatusCode};
use serde_json::{json, Value};

use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};

/// 默认总超时（连接 + 读取）：30 秒。
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// 默认最大响应体大小：10 MiB。
const DEFAULT_MAX_SIZE: u64 = 10 * 1024 * 1024;
/// 工具自身允许的硬上限：60 秒。
const HARD_TIMEOUT_CAP_MS: u64 = 60_000;
/// 工具自身允许的最大响应体：64 MiB。
const HARD_MAX_SIZE_CAP: u64 = 64 * 1024 * 1024;

/// 把 `(name, type, desc)` 三元组转成 `ToolInputProperty`。
fn prop(ty: PropertyType, description: &str) -> ToolInputProperty {
    ToolInputProperty {
        property_type: ty,
        description: Some(description.into()),
        enum_values: None,
        minimum: None,
        maximum: None,
        min_length: None,
        max_length: None,
    }
}

/// 构造一个 `required` schema：所有属性都出现在 `required` 列表里。
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

/// 校验 URL：必须是 `http://` 或 `https://`，其他协议直接拒绝。
///
/// 工具不应让 agent 用任意协议（如 `file://`）访问本地资源。
fn validate_url(url: &str) -> Result<reqwest::Url, crate::error::ToolError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| crate::error::ToolError::other(format!("invalid url: {}", e)))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(crate::error::ToolError::other(format!(
            "unsupported url scheme: {} (only http/https allowed)",
            other
        ))),
    }
}

/// 把输入中的 `headers` 对象（`serde_json::Value`）转成 `reqwest::HeaderMap`。
///
/// 非法的 header 名 / 值会被跳过并记入 `warnings`；不抛错，避免一个错别字
/// 让整个请求失败。
fn parse_headers(headers: &Value, warnings: &mut Vec<String>) -> HeaderMap {
    let mut out = HeaderMap::new();
    let Some(obj) = headers.as_object() else {
        return out;
    };
    for (k, v) in obj {
        let value_str = match v.as_str() {
            Some(s) => s,
            None => {
                warnings.push(format!("header '{}' must be a string, skipped", k));
                continue;
            }
        };
        let name = match HeaderName::from_bytes(k.as_bytes()) {
            Ok(n) => n,
            Err(e) => {
                warnings.push(format!("invalid header name '{}': {}", k, e));
                continue;
            }
        };
        let value = match HeaderValue::from_str(value_str) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("invalid header value for '{}': {}", k, e));
                continue;
            }
        };
        out.insert(name, value);
    }
    out
}

/// 判断 MIME 是否应该按文本处理。
///
/// 走文本分支的：text/*、application/json、application/xml 等；
/// 其他全部按二进制（base64）返回。
fn is_text_mime(mime: &str) -> bool {
    let m = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if m.is_empty() {
        // 没有 Content-Type：保守地按文本处理（多数纯文本 API 都不显式带 type）。
        return true;
    }
    if m.starts_with("text/") {
        return true;
    }
    matches!(
        m.as_str(),
        "application/json"
            | "application/xml"
            | "application/xhtml+xml"
            | "application/javascript"
            | "application/ld+json"
            | "application/x-www-form-urlencoded"
            | "image/svg+xml"
    )
}

/// 构造一个 `reqwest::Client`，按本次调用配置超时与跳转策略。
fn build_client(timeout: Duration, follow_redirects: bool) -> Result<Client, crate::error::ToolError> {
    let mut builder = ClientBuilder::new()
        .user_agent("latte-rs-agent-tools/0.1 (fetch)")
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(10)));
    // 默认跟随最多 10 次重定向；如关闭则直接拒绝任何 3xx。
    builder = if follow_redirects {
        builder.redirect(Policy::limited(10))
    } else {
        builder.redirect(Policy::none())
    };
    builder
        .build()
        .map_err(|e| crate::error::ToolError::execution_str("fetch", format!("client build: {}", e)))
}

/// 标准化 HTTP 方法：接收 "GET"/"get"/"Get" 都能解析。
fn parse_method(s: &str) -> Result<Method, crate::error::ToolError> {
    Method::from_bytes(s.to_ascii_uppercase().as_bytes())
        .map_err(|e| crate::error::ToolError::other(format!("invalid http method '{}': {}", s, e)))
}

/// 构造 `fetch` 工具定义。
///
/// 输入 / 输出 schema 见模块级文档。
pub fn http_fetch_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            // --- 1. 解析 + 校验必填参数 ------------------------------------
            let url_raw = input
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("url is required"))?;
            // URL 解析失败直接报错，scheme 非 http(s) 也直接拒绝。
            let parsed_url = validate_url(url_raw)?;

            // method 默认 GET；任何大写不识别的 method 都报错。
            let method_str = input
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET");
            let method = parse_method(method_str)?;

            // 自定义 headers：非法值会被收集到 warnings，不打断请求。
            let mut warnings: Vec<String> = Vec::new();
            let headers = parse_headers(input.get("headers").unwrap_or(&Value::Null), &mut warnings);

            // body：仅在客户端允许为空（GET 也可以带 body，但极少见；按透传处理）。
            let body = input
                .get("body")
                .and_then(|v| v.as_str())
                .map(String::from);

            // timeoutMs：clamp 到 [1, 60_000]，默认值 30_000。
            let timeout_ms = input
                .get("timeoutMs")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1, HARD_TIMEOUT_CAP_MS);
            let timeout = Duration::from_millis(timeout_ms);

            // maxSize：clamp 到 [1, 64 MiB]，默认 10 MiB。
            let max_size = input
                .get("maxSize")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_MAX_SIZE)
                .clamp(1, HARD_MAX_SIZE_CAP);

            // followRedirects 默认 true。
            let follow_redirects = input
                .get("followRedirects")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            // --- 2. 构造 client + request ----------------------------------
            let client = build_client(timeout, follow_redirects)?;
            let mut req = client.request(method.clone(), parsed_url.clone());
            if !headers.is_empty() {
                req = req.headers(headers);
            }
            if let Some(b) = body.as_deref() {
                req = req.body(b.to_string());
            }

            // --- 3. 发送请求 + 测量耗时 ------------------------------------
            let started = Instant::now();
            let response = req.send().await.map_err(|e| {
                if e.is_timeout() {
                    crate::error::ToolError::timeout("fetch", timeout)
                } else {
                    crate::error::ToolError::execution("fetch", e)
                }
            })?;
            let final_url = response.url().to_string();
            let status: StatusCode = response.status();
            let status_text = status
                .canonical_reason()
                .unwrap_or("")
                .to_string();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            // 收集所有响应头（小写 key）。
            let mut resp_headers = BTreeMap::new();
            for (k, v) in response.headers().iter() {
                if let Ok(s) = v.to_str() {
                    resp_headers.insert(k.as_str().to_string(), s.to_string());
                }
            }

            // --- 4. 读取 body（流式累加，超过 maxSize 立刻停止）---------
            // 不做 Content-Length 预检：部分 server 会撒谎/不带该头，agent 拿到
            // 部分内容比直接拒绝更实用。
            let mut buf: Vec<u8> = Vec::new();
            let mut truncated = false;

            // 流式累加字节数，超过上限立刻停止。
            let mut stream = response;
            while let Some(chunk) = stream.chunk().await.map_err(|e| {
                if e.is_timeout() {
                    crate::error::ToolError::timeout("fetch", timeout)
                } else {
                    crate::error::ToolError::execution("fetch", e)
                }
            })? {
                if buf.len() + chunk.len() > max_size as usize {
                    let remain = max_size as usize - buf.len();
                    buf.extend_from_slice(&chunk[..remain]);
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(&chunk);
            }
            let size = buf.len() as u64;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            // --- 5. 编码 body --------------------------------------------
            // 文本类型 → utf-8 字符串；二进制类型 → base64。
            let (body_value, encoding) = if is_text_mime(&content_type) {
                match String::from_utf8(buf) {
                    Ok(s) => (Value::String(s), "utf-8".to_string()),
                    Err(e) => {
                        // 文本类型但解码失败：仍按二进制处理，并提示一下。
                        let bytes = e.into_bytes();
                        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        warnings.push(format!(
                            "content-type '{}' claimed utf-8 but body is not valid utf-8; fell back to base64",
                            content_type
                        ));
                        (Value::String(encoded), "base64".to_string())
                    }
                }
            } else {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&buf);
                (Value::String(encoded), "base64".to_string())
            };

            let mut out = json!({
                "url": url_raw,
                "finalUrl": final_url,
                "status": status.as_u16(),
                "statusText": status_text,
                "contentType": content_type,
                "headers": resp_headers,
                "body": body_value,
                "encoding": encoding,
                "size": size,
                "truncated": truncated,
                "elapsedMs": elapsed_ms,
            });
            if !warnings.is_empty() {
                out["warnings"] = json!(warnings);
            }
            Ok(out)
        }
        .boxed()
    };

    Tool::builder(
        "fetch",
        "发起 HTTP/HTTPS 请求并返回响应",
        required(vec![("url", PropertyType::String, "完整 URL，仅支持 http:// 与 https://")]),
        Arc::new(handler),
    )
    .concurrency_safe(true)
    .timeout(Duration::from_secs(60))
    .build()
}

/// `http` 工具包：目前只包含 `fetch` 一个工具。
pub struct HttpToolsPackage;

impl HttpToolsPackage {
    /// 构造包（单个 `fetch` 工具）。
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "http".into(),
            version: Some("1.0.0".into()),
            namespace: None,
            description: Some("HTTP 工具：发起 HTTP/HTTPS 请求".into()),
            dependencies: None,
            tools: vec![http_fetch_tool()],
            on_init: None,
            on_destroy: None,
            before_execute: None,
            after_execute: None,
            metadata: Some(json!({"category": "network", "tags": ["http", "fetch", "api"]})),
        }
    }
}

impl Default for HttpToolsPackage {
    fn default() -> Self {
        Self
    }
}

// =============================================================================
// 单测
// =============================================================================
//
// 测试策略：在 tokio 运行时里起一个本地 TCP listener，accept 后读取 request，
// 回复预置的 HTTP/1.1 响应。然后用真实的 reqwest 客户端打过去，校验工具输出。
// 这样不依赖外网，也不需要 wiremock / httpmock 这类额外 dev 依赖。
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 起一个最小的 HTTP/1.1 mock server。
    ///
    /// `handler` 接收 (method, path, headers, body) 并返回 (status, content_type, body_bytes) 三元组。
    /// Server 启动后立刻返回监听的地址，处理完一次请求后自动关闭。
    /// 用 `Vec<u8>` 而不是 `String` 是为了能发原始二进制（含非 UTF-8 字节）。
    async fn spawn_mock_server(
        handler: Arc<dyn Fn(&str, &str, &str, &str) -> (u16, &'static str, Vec<u8>) + Send + Sync>,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = vec![0u8; 16 * 1024];
            let mut total = 0usize;
            // 读到 headers 结束 (\r\n\r\n) 为止。
            loop {
                let n = sock.read(&mut buf[total..]).await.unwrap_or(0);
                if n == 0 { break; }
                total += n;
                if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") { break; }
            }
            let raw = String::from_utf8_lossy(&buf[..total]).to_string();
            // 解析 request line + headers + body
            let (head, body) = raw
                .split_once("\r\n\r\n")
                .map(|(h, b)| (h, b.to_string()))
                .unwrap_or((raw.as_str(), String::new()));
            let mut head_lines = head.split("\r\n");
            let request_line = head_lines.next().unwrap_or("");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            // headers 简单拼成单字符串，便于断言（不强求格式）。
            let headers_str: String = head_lines.collect::<Vec<_>>().join("\n");
            let (status, content_type, response_body) = handler(&method, &path, &headers_str, &body);
            let reason = match status {
                200 => "OK",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let resp = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status, reason, content_type, response_body.len(),
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.write_all(&response_body).await;
            let _ = sock.shutdown().await;
        });
        addr
    }

    /// 把工具 handler 包成一个 `(input) -> Result<Value, ToolError>` 闭包。
    async fn run(input: Value) -> Result<Value, crate::error::ToolError> {
        let tool = http_fetch_tool();
        let ctx = ToolExecutionContext::fresh("fetch", 0);
        (tool.handler)(input, ctx).await
    }

    /// 测试：最基本的 GET 请求，返回 text/plain 响应。
    /// 验证：status / contentType / body / encoding / size 字段。
    #[tokio::test]
    async fn http_fetch_get_text() {
        let handler = Arc::new(|_m: &str, _p: &str, _h: &str, _b: &str| {
            (200u16, "text/plain; charset=utf-8", b"hello world".to_vec())
        });
        let addr = spawn_mock_server(handler).await;
        let url = format!("http://{}/ping", addr);
        let out = run(json!({ "url": url })).await.unwrap();
        assert_eq!(out["status"], 200);
        assert_eq!(out["body"], "hello world");
        assert_eq!(out["encoding"], "utf-8");
        assert_eq!(out["size"], 11);
        assert_eq!(out["truncated"], false);
        assert!(out["contentType"].as_str().unwrap().contains("text/plain"));
    }

    /// 测试：自定义 method + headers + body 都能透传到 server 端。
    /// 验证：method 解析大小写不敏感、headers 与 body 都能发出去。
    #[tokio::test]
    async fn http_fetch_post_with_headers_and_body() {
        let handler = Arc::new(|m: &str, p: &str, h: &str, b: &str| {
            // 把关键信息回显到 body，方便断言
            let body = format!("method={} path={} has_x_token={} body={}", m, p, h.contains("x-token: secret"), b);
            (200u16, "text/plain", body.into_bytes())
        });
        let addr = spawn_mock_server(handler).await;
        let url = format!("http://{}/echo", addr);
        let out = run(json!({
            "url": url,
            "method": "post",
            "headers": { "x-token": "secret", "X-Other": "v=1" },
            "body": "ping"
        }))
        .await
        .unwrap();
        let body = out["body"].as_str().unwrap();
        assert!(body.contains("method=POST"));
        assert!(body.contains("path=/echo"));
        assert!(body.contains("has_x_token=true"));
        assert!(body.contains("body=ping"));
    }

    /// 测试：二进制响应（image/png）应自动 base64 编码。
    /// 验证：encoding="base64" 且解码后字节完全一致。
    #[tokio::test]
    async fn http_fetch_binary_response_is_base64() {
        // 0x89 0x50 0x4E 0x47 是 PNG 文件头，包含非 UTF-8 字节。
        let raw: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let expected = base64::engine::general_purpose::STANDARD.encode(&raw);
        let handler = Arc::new(move |_m: &str, _p: &str, _h: &str, _b: &str| {
            (200u16, "image/png", raw.clone())
        });
        let addr = spawn_mock_server(handler).await;
        let url = format!("http://{}/image.png", addr);
        let out = run(json!({ "url": url })).await.unwrap();
        assert_eq!(out["encoding"], "base64");
        assert_eq!(out["body"], expected);
    }

    /// 测试：maxSize 触发截断，body 应被截断且 truncated=true。
    #[tokio::test]
    async fn http_fetch_truncates_when_exceeding_max_size() {
        let handler = Arc::new(|_m: &str, _p: &str, _h: &str, _b: &str| {
            (200u16, "text/plain", "ABCDEFGHIJ".repeat(100).into_bytes())
        });
        let addr = spawn_mock_server(handler).await;
        let url = format!("http://{}/big", addr);
        let out = run(json!({ "url": url, "maxSize": 50 })).await.unwrap();
        assert_eq!(out["truncated"], true);
        assert_eq!(out["size"], 50);
        assert_eq!(out["body"].as_str().unwrap().len(), 50);
    }

    /// 测试：URL scheme 不是 http(s) 时直接报错。
    #[tokio::test]
    async fn http_fetch_rejects_non_http_scheme() {
        let out = run(json!({ "url": "ftp://example.com/x" })).await;
        assert!(out.is_err());
    }

    /// 测试：4xx / 5xx 响应不应该报错——HTTP 错误也是合法的"结果"，由调用方决定如何处理。
    #[tokio::test]
    async fn http_fetch_returns_404_without_error() {
        let handler = Arc::new(|_m: &str, _p: &str, _h: &str, _b: &str| {
            (404u16, "text/plain", b"not found".to_vec())
        });
        let addr = spawn_mock_server(handler).await;
        let url = format!("http://{}/missing", addr);
        let out = run(json!({ "url": url })).await.unwrap();
        assert_eq!(out["status"], 404);
        assert_eq!(out["body"], "not found");
    }
}
