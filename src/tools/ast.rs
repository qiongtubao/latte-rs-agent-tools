//! AST structural code search and replace tools.
//! 提供 `grep` 和 `ast_edit` 工具：基于 AST 模式的代码搜索和替换。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use ast_grep_core::source::Edit;
use ast_grep_language::LanguageExt;
use futures::FutureExt;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::types::{
    PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema,
    ToolPackage,
};

fn prop(ty: PropertyType, description: &str) -> ToolInputProperty {
    ToolInputProperty { property_type: ty, description: Some(description.into()), enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None, items: None, properties: None, required: None, additional_properties: None }
}

fn ext_to_lang_str(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust", "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript", "py" => "python",
        "go" => "go", "java" => "java", "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp", "rb" => "ruby",
        "php" => "php", "swift" => "swift", "kt" | "kts" => "kotlin",
        "scala" => "scala", "css" => "css", "html" | "htm" => "html",
        "json" => "json", "yaml" | "yml" => "yaml", "toml" => "toml",
        "sh" | "bash" | "zsh" => "bash", "lua" => "lua", "dart" => "dart",
        "hs" => "haskell", "clj" | "cljs" | "edn" => "clojure",
        "ex" | "exs" => "elixir", "erl" => "erlang", "sql" => "sql",
        "vue" => "vue", "svelte" => "svelte", "zig" => "zig",
        "nix" => "nix", "proto" => "protobuf", _ => "",
    }
}

fn infer_lang(path: &str) -> Result<ast_grep_language::SupportLang, ToolError> {
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
    let s = ext_to_lang_str(ext);
    if s.is_empty() { return Err(ToolError::other(format!("unsupported: .{}", ext))); }
    s.parse().map_err(|_| ToolError::other(format!("bad lang: {}", s)))
}

fn ast_grep_schema() -> ToolInputSchema {
    let mut p = BTreeMap::new();
    p.insert("pat".into(), prop(PropertyType::String, "AST pattern. $NAME/$_ = one node, $$$NAME/$$$ = zero-or-more."));
    p.insert("paths".into(), prop(PropertyType::Array, "Files, dirs, or globs.").with_items(prop(PropertyType::String, "File, dir, or glob.")));
    p.insert("lang".into(), prop(PropertyType::String, "Language override."));
    p.insert("skip".into(), prop(PropertyType::Number, "Skip first N matches."));
    p.insert("limit".into(), prop(PropertyType::Number, "Max matches (default 100)."));
    ToolInputSchema { schema_type: Default::default(), properties: p, required: Some(vec!["pat".into(), "paths".into()]), additional_properties: None }
}

fn ast_edit_schema() -> ToolInputSchema {
    let mut p = BTreeMap::new();
    p.insert("pat".into(), prop(PropertyType::String, "AST pattern."));
    p.insert("out".into(), prop(PropertyType::String, "Replacement pattern."));
    p.insert("paths".into(), prop(PropertyType::Array, "Files, dirs, or globs.").with_items(prop(PropertyType::String, "File, dir, or glob.")));
    p.insert("lang".into(), prop(PropertyType::String, "Language override."));
    ToolInputSchema { schema_type: Default::default(), properties: p, required: Some(vec!["pat".into(), "out".into(), "paths".into()]), additional_properties: None }
}

fn collect_files(paths: &[String], cwd: &std::path::Path) -> Result<Vec<PathBuf>, ToolError> {
    let mut files: Vec<PathBuf> = Vec::new();
    for raw in paths {
        let p = if std::path::Path::new(raw).is_absolute() { PathBuf::from(raw) } else { cwd.join(raw) };
        if p.is_file() { files.push(p); }
        else if p.is_dir() {
            for e in walkdir::WalkDir::new(&p).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
                let ep = e.path().to_path_buf();
                if !ext_to_lang_str(ep.extension().and_then(|e| e.to_str()).unwrap_or("")).is_empty() { files.push(ep); }
            }
        } else {
            for e in glob::glob(raw).map_err(|_| ToolError::other(format!("bad glob: {}", raw)))?.flatten() { if e.is_file() { files.push(e); } }
        }
    }
    files.sort(); files.dedup(); Ok(files)
}

fn search_file(path: &PathBuf, pat: &str, lo: &Option<String>, skip: usize, limit: usize, cwd: &std::path::Path) -> Result<Vec<Value>, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let lang_str = lo.clone().unwrap_or_else(|| {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        ext_to_lang_str(ext).to_string()
    });
    if lang_str.is_empty() { return Ok(vec![]); }
    let lang: ast_grep_language::SupportLang = lang_str.parse().map_err(|_| format!("bad lang: {}", lang_str))?;
    let grep = lang.ast_grep(&content);
    let results: Vec<_> = grep.root().find_all(pat).collect();
    let rel = path.strip_prefix(cwd).unwrap_or(path).to_string_lossy().to_string();
    let mut matches: Vec<Value> = Vec::new();
    for (i, node) in results.iter().enumerate() {
        if i < skip { continue; }
        if matches.len() >= limit { break; }
        let start = node.start_pos();
        let end = node.end_pos();
        let (sl, sc) = start.byte_point();
        let (el, ec) = end.byte_point();
        matches.push(json!({
            "file": rel,
            "text": node.text(),
            "startLine": sl + 1,
            "startColumn": sc + 1,
            "endLine": el + 1,
            "endColumn": ec + 1,
        }));
    }
    Ok(matches)
}

fn edit_file(path: &PathBuf, pat: &str, out: &str, lo: &Option<String>, cwd: &std::path::Path) -> Result<Option<Value>, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let lang_str = lo.clone().unwrap_or_else(|| {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        ext_to_lang_str(ext).to_string()
    });
    if lang_str.is_empty() { return Ok(None); }
    let lang: ast_grep_language::SupportLang = lang_str.parse().map_err(|_| format!("bad lang: {}", lang_str))?;
    let grep = lang.ast_grep(&content);
    let edits: Vec<Edit<String>> = grep.root().replace_all(pat, out);
    if edits.is_empty() { return Ok(None); }
    let mut bytes = content.into_bytes();
    for edit in edits.iter().rev() {
        let range = edit.position..edit.position + edit.deleted_length;
        bytes.splice(range, edit.inserted_text.clone());
    }
    let new_content = String::from_utf8(bytes).map_err(|e| format!("utf8: {}", e))?;
    let rel = path.strip_prefix(cwd).unwrap_or(path).to_string_lossy().to_string();
    std::fs::write(path, &new_content).map_err(|e| e.to_string())?;
    Ok(Some(json!({"file": rel, "replacements": edits.len()})))
}

fn ast_grep_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            let pat = input.get("pat").and_then(|v| v.as_str()).ok_or_else(|| ToolError::other("pat required"))?.to_string();
            let paths: Vec<String> = input.get("paths").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).ok_or_else(|| ToolError::other("paths required"))?;
            let lo = input.get("lang").and_then(|v| v.as_str()).map(String::from);
            let skip = input.get("skip").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let cwd: PathBuf = ctx.metadata.as_ref().and_then(|m| m.get("cwd").and_then(|v| v.as_str())).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let files = collect_files(&paths, &cwd)?;
            let mut all_matches: Vec<Value> = Vec::new();
            let mut file_count = 0;
            for f in &files {
                let m = tokio::task::spawn_blocking({
                    let f = f.clone(); let pat = pat.clone(); let lo = lo.clone(); let cwd = cwd.clone();
                    move || search_file(&f, &pat, &lo, 0, limit, &cwd)
                }).await.map_err(|e| ToolError::execution_str("grep", format!("panic: {}", e)))?;
                let m = m.map_err(ToolError::other)?;
                if !m.is_empty() { file_count += 1; all_matches.extend(m); }
            }
            let all = if skip < all_matches.len() { all_matches[skip..].to_vec() } else { vec![] };
            let all = if all.len() > limit { all[..limit].to_vec() } else { all };
            Ok(json!({"matchCount": all.len(), "fileCount": file_count, "matches": all}))
        }.boxed()
    };
    Tool::builder("grep", "AST structural code search. 26+ languages. Use $NAME/$_ for one node, $$$NAME/$$$ for zero-or-more.", ast_grep_schema(), Arc::new(handler))
        .concurrency_safe(true).timeout(std::time::Duration::from_secs(60)).build()
}

fn ast_edit_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            let pat = input.get("pat").and_then(|v| v.as_str()).ok_or_else(|| ToolError::other("pat required"))?.to_string();
            let out = input.get("out").and_then(|v| v.as_str()).ok_or_else(|| ToolError::other("out required"))?.to_string();
            let paths: Vec<String> = input.get("paths").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).ok_or_else(|| ToolError::other("paths required"))?;
            let lo = input.get("lang").and_then(|v| v.as_str()).map(String::from);
            let cwd: PathBuf = ctx.metadata.as_ref().and_then(|m| m.get("cwd").and_then(|v| v.as_str())).map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let files = collect_files(&paths, &cwd)?;
            let mut results: Vec<Value> = Vec::new();
            let mut total = 0usize;
            for f in &files {
                let r = tokio::task::spawn_blocking({
                    let f = f.clone(); let pat = pat.clone(); let out = out.clone(); let lo = lo.clone(); let cwd = cwd.clone();
                    move || edit_file(&f, &pat, &out, &lo, &cwd)
                }).await.map_err(|e| ToolError::execution_str("ast_edit", format!("panic: {}", e)))?;
                let r = r.map_err(ToolError::other)?;
                if let Some(v) = r { total += v["replacements"].as_u64().unwrap_or(0) as usize; results.push(v); }
            }
            Ok(json!({"fileCount": results.len(), "totalReplacements": total, "replacements": results}))
        }.boxed()
    };
    Tool::builder("ast_edit", "AST structural code replace. Rewrites code matching `pat` to `out` using AST-aware replacement.", ast_edit_schema(), Arc::new(handler))
        .concurrency_safe(false).timeout(std::time::Duration::from_secs(60)).build()
}

pub struct AstToolsPackage;
impl AstToolsPackage {
    pub fn new() -> ToolPackage {
        ToolPackage { name: "ast".into(), version: Some("1.0.0".into()), namespace: None, description: Some("AST 结构代码搜索和替换工具".into()), dependencies: None, tools: vec![ast_grep_tool(), ast_edit_tool()], on_init: None, on_destroy: None, before_execute: None, after_execute: None, metadata: Some(json!({"category": "code", "tags": ["ast", "search", "replace"]})) }
    }
}
impl Default for AstToolsPackage { fn default() -> Self { Self } }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::create_tool_manager;
    use crate::types::ToolManager;
    use serde_json::json;
    use tempfile::TempDir;

    fn w(dir: &TempDir, name: &str, content: &str) -> String {
        let p = dir.path().join(name); std::fs::write(&p, content).unwrap(); p.to_string_lossy().to_string()
    }
    async fn grep(pat: &str, path: &str) -> Value {
        let m = create_tool_manager(); m.register_package(AstToolsPackage::new()).await.unwrap();
        m.execute("grep", json!({"pat": pat, "paths": [path]}), None).await.unwrap()
    }
    async fn edit(pat: &str, out: &str, path: &str) -> Value {
        let m = create_tool_manager(); m.register_package(AstToolsPackage::new()).await.unwrap();
        m.execute("ast_edit", json!({"pat": pat, "out": out, "paths": [path]}), None).await.unwrap()
    }

    #[tokio::test]
    async fn test_ast_grep_rust_fn() {
        let dir = TempDir::new().unwrap();
        let p = w(&dir, "test.rs", "fn hello() { return 42; }\nfn world() { return 0; }\n");
        let r = grep("fn $NAME() { $$$ }", &p).await;
        assert_eq!(r["matchCount"].as_u64().unwrap(), 2);
    }

    #[tokio::test]
    async fn test_ast_grep_no_match() {
        let dir = TempDir::new().unwrap();
        let p = w(&dir, "test.rs", "fn hello() {}\n");
        let r = grep("class $NAME { $$$ }", &p).await;
        assert_eq!(r["matchCount"].as_u64().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_ast_edit_rust() {
        let dir = TempDir::new().unwrap();
        let p = w(&dir, "test.rs", "fn old_name() { return 1; }\n");
        let r = edit("fn $NAME() { $$$ }", "fn new_name() { $$$ }", &p).await;
        assert!(r["totalReplacements"].as_u64().unwrap() >= 1);
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(content.contains("new_name"));
    }

    #[tokio::test]
    async fn test_ast_grep_js() {
        let dir = TempDir::new().unwrap();
        let p = w(&dir, "test.js", "console.log('hello');\nconsole.log('world');\n");
        let r = grep("console.log($MSG)", &p).await;
        assert_eq!(r["matchCount"].as_u64().unwrap(), 2);
    }
}