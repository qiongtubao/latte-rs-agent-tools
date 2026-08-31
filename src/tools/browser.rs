//! Browser automation tool — full browser control via Playwright.
//! 使用持久化 Node.js worker 进程，通过 stdin/stdout JSON-RPC 通信。

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

use crate::error::ToolError;
use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};

static BROWSER_WORKER: LazyLock<Mutex<Option<BrowserWorker>>> = std::sync::LazyLock::new(|| Mutex::new(None));
static REQ_ID: AtomicU64 = AtomicU64::new(1);

struct BrowserWorker {
    stdin: ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    child: Child,
}

impl Drop for BrowserWorker {
    fn drop(&mut self) { let _ = self.child.start_kill(); }
}

const WORKER_SCRIPT: &str = r##"const{chromium}=require('playwright-core');let browser=null,page=null;let buf='';process.stdin.on('data',chunk=>{buf+=chunk.toString();const lines=buf.split('\n');buf=lines.pop()||'';for(const line of lines){if(!line.trim())continue;let req;try{req=JSON.parse(line)}catch(e){process.stdout.write(JSON.stringify({error:'parse:'+e.message})+'\n');continue}const{id,method,params}=req;(async()=>{try{let result;switch(method){case'open':{const{url,width=1280,height=720,timeout=30000}=params||{};if(!browser)browser=await chromium.launch({headless:true,executablePath:'/usr/bin/google-chrome'});const ctx=await browser.newContext({viewport:{width,height}});page=await ctx.newPage();if(url)await page.goto(url,{waitUntil:'domcontentloaded',timeout});result={url:page.url(),title:await page.title()};break}case'goto':{const{url,timeout=30000}=params||{};if(!page)throw Error('No page');await page.goto(url,{waitUntil:'domcontentloaded',timeout});result={url:page.url(),title:await page.title()};break}case'click':{const{selector,timeout=5000}=params||{};if(!page)throw Error('No page');await page.click(selector,{timeout});result={clicked:selector};break}case'type':{const{selector,text,timeout=5000}=params||{};if(!page)throw Error('No page');await page.fill(selector,'',{timeout});await page.type(selector,text,{delay:10,timeout});result={selector,text};break}case'fill':{const{selector,text,timeout=5000}=params||{};if(!page)throw Error('No page');await page.fill(selector,text,{timeout});result={filled:selector};break}case'extract':{if(!page)throw Error('No page');const title=await page.title();const text=await page.evaluate(()=>document.body?.innerText||'');result={title,text:text.slice(0,50000),url:page.url()};break}case'evaluate':{const{script}=params||{};if(!page)throw Error('No page');const value=await page.evaluate(script);result={value};break}case'screenshot':{if(!page)throw Error('No page');const buf=await page.screenshot({type:'png'});result={data:buf.toString('base64'),mimeType:'image/png',bytes:buf.length};break}case'close':{if(browser){await browser.close();browser=null;page=null}result={closed:true};break}default:throw Error('Unknown:'+method)}process.stdout.write(JSON.stringify({id,result})+'\n')}catch(e){process.stdout.write(JSON.stringify({id,error:e.message})+'\n')}})()}});process.stdout.write(JSON.stringify({ready:true})+'\n');"##;

fn prop(ty: PropertyType, d: &str) -> ToolInputProperty {
    ToolInputProperty { property_type: ty, description: Some(d.into()), enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None }
}

fn browser_schema() -> ToolInputSchema {
    let mut p = BTreeMap::new();
    p.insert("action".into(), prop(PropertyType::String, "Action: open/goto/click/type/fill/screenshot/extract/evaluate/close"));
    p.insert("url".into(), prop(PropertyType::String, "URL for open/goto"));
    p.insert("selector".into(), prop(PropertyType::String, "CSS selector for click/type/fill"));
    p.insert("text".into(), prop(PropertyType::String, "Text for type/fill"));
    p.insert("script".into(), prop(PropertyType::String, "JS expression for evaluate"));
    p.insert("width".into(), prop(PropertyType::Integer, "Viewport width (1280)"));
    p.insert("height".into(), prop(PropertyType::Integer, "Viewport height (720)"));
    p.insert("timeout".into(), prop(PropertyType::Number, "Timeout ms (30000)"));
    ToolInputSchema { schema_type: Default::default(), properties: p, required: Some(vec!["action".into()]), additional_properties: None }
}

fn find_pw_modules() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let dir = std::fs::read_dir(format!("{}/.npm/_npx", home)).ok()?;
    for entry in dir.flatten() {
        let nm = entry.path().join("node_modules");
        if nm.join("playwright-core").is_dir() || nm.join("playwright").is_dir() {
            return Some(nm.to_string_lossy().to_string());
        }
    }
    None
}

async fn ensure_worker() -> Result<(), ToolError> {
    let mut guard = BROWSER_WORKER.lock().await;
    if let Some(ref mut w) = *guard {
        match w.child.try_wait() {
            Ok(Some(_)) => { guard.take(); }
            Ok(None) => return Ok(()),
            Err(_) => { guard.take(); }
        }
    }

    let nm = find_pw_modules().ok_or_else(|| ToolError::other("playwright-core not found. Run: npx playwright@latest install"))?;
    let tmp_dir = std::env::temp_dir().join("latte-browser");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let wp = tmp_dir.join("worker.cjs");
    std::fs::write(&wp, WORKER_SCRIPT).map_err(|e| ToolError::other(format!("write: {}", e)))?;

    let mut child = Command::new("node")
        .arg(&wp).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .env("NODE_PATH", &nm).spawn()
        .map_err(|e| ToolError::execution("browser", e))?;

    let stdin = child.stdin.take().ok_or_else(|| ToolError::other("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| ToolError::other("no stdout"))?;
    let mut reader = BufReader::new(stdout);
    let mut ready = false;
    let mut last = String::new();
    for _ in 0..10 {
        last.clear();
        match tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_line(&mut last)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => { if last.contains("ready") { ready = true; break; } }
            _ => { break; }
        }
    }
    if !ready { return Err(ToolError::other(format!("worker not ready: {}", last.trim()))); }
    *guard = Some(BrowserWorker { stdin, reader, child });
    Ok(())
}

async fn send_req(method: &str, params: Value) -> Result<Value, ToolError> {
    ensure_worker().await?;
    let id = REQ_ID.fetch_add(1, Ordering::Relaxed);
    let req_str = format!("{}\n", serde_json::to_string(&json!({"id": id, "method": method, "params": params})).unwrap());
    let mut guard = BROWSER_WORKER.lock().await;
    let w = guard.as_mut().ok_or_else(|| ToolError::other("worker lost"))?;
    w.stdin.write_all(req_str.as_bytes()).await.map_err(|e| ToolError::execution("browser", e))?;
    w.stdin.flush().await.map_err(|e| ToolError::execution("browser", e))?;
    let mut line = String::new();
    w.reader.read_line(&mut line).await.map_err(|e| ToolError::execution("browser", e))?;
    let resp: Value = serde_json::from_str(&line).map_err(|e| ToolError::other(format!("parse: {}", e)))?;
    if let Some(err) = resp.get("error").and_then(|v| v.as_str()) { return Err(ToolError::other(format!("browser: {}", err))); }
    Ok(resp.get("result").cloned().unwrap_or(json!({})))
}

fn browser_tool() -> Tool {
    let h = |input: Value, _ctx: ToolExecutionContext| async move {
        let action = input.get("action").and_then(|v| v.as_str()).ok_or_else(|| ToolError::other("action required"))?.to_string();
        let mut params = input;
        params.as_object_mut().map(|m| m.remove("action"));
        match action.as_str() {
            "open" | "goto" | "click" | "type" | "fill" | "screenshot" | "extract" | "evaluate" | "close" => send_req(&action, params).await,
            _ => Err(ToolError::other(format!("unknown: {}", action))),
        }
    }.boxed();
    Tool::builder("browser", "Browser automation: open/goto/click/type/fill/screenshot/extract/evaluate/close.", browser_schema(), std::sync::Arc::new(h))
        .concurrency_safe(false).timeout(std::time::Duration::from_secs(120)).build()
}

pub struct BrowserToolsPackage;
impl BrowserToolsPackage {
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "browser".into(), version: Some("1.0.0".into()),
            namespace: None,
            description: Some("浏览器自动化工具".into()), dependencies: None,
            tools: vec![browser_tool()], on_init: None, on_destroy: None,
            before_execute: None, after_execute: None,
            metadata: Some(json!({"category": "browser", "tags": ["browser", "playwright"]})),
        }
    }
}
impl Default for BrowserToolsPackage { fn default() -> Self { Self } }

#[cfg(test)]
mod tests {
    use super::*; use crate::core::create_tool_manager; use crate::types::ToolManager; use serde_json::json;
    async fn exec(action: &str, params: &Value) -> Value {
        let m = create_tool_manager(); m.register_package(BrowserToolsPackage::new()).await.unwrap();
        let mut p = params.clone(); p.as_object_mut().unwrap().insert("action".into(), json!(action));
        m.execute("browser", p, None).await.unwrap()
    }
    #[tokio::test] async fn test_browser_open_close() {
        let r = exec("open", &json!({"url":"about:blank","width":800,"height":600})).await;
        assert!(r.get("title").is_some());
        assert_eq!(exec("close", &json!({})).await["closed"].as_bool(), Some(true));
    }
    #[tokio::test] async fn test_browser_evaluate() {
        let _ = exec("open", &json!({"url":"about:blank"})).await;
        let r = exec("evaluate", &json!({"script":"1+2"})).await;
        assert_eq!(r["value"], 3);
        let _ = exec("close", &json!({})).await;
    }
}