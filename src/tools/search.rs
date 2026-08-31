//! Enhanced content search tool. Mirrors the `search` tool in oh-my-pi/coding-agent.
//!
//! 提供 `search` 工具：在文件内容中按正则搜索，支持多目标、glob 路径、gitignore 尊重、
//! 大小写不敏感、按文件分页（`skip`）、单文件匹配上限（`limit`）。
//!
//! 与项目里旧版 `search` 的区别：
//! - 旧版只有 `path`（单路径） + `filePattern`（文件名 glob） + `maxDepth`；本版用 `paths`
//!   数组接管，三者都能用 glob 表达。
//! - 旧版没有 `skip`，命中文件多了只能改 pattern；本版用 `skip` 翻页。
//! - 旧版 `ignoreCase` → 本版 `i`（对齐 TS 版本，但保留 `ignoreCase` 作为别名以兼容老调用）。
//!
//! ## 输入
//!
//! - `pattern` (string, 必填) — regex 模式。空字符串会报错。
//! - `literal` (bool, 可选, 默认 `false`) — `pattern` 按字面量匹配（内部
//!   `regex::escape` 后仍走 regex 引擎，`i` / 分页等语义不变）。搜代码
//!   符号（`LOG(`、`foo[0]`）必须用它，否则按 regex 解析会报错或错配。
//! - `paths` (string | string[], 可选, 默认 `["."]`) — 搜索目标。每个元素可以是：
//!   - 字面文件路径（只搜这一个文件）
//!   - 字面目录路径（递归搜整个目录）
//!   - glob 模式（如 `src/**/*.rs`）
//! - `path` (string, 可选) — 兼容旧版单路径输入，等价于 `paths: ["..."]`。
//! - `i` (bool, 可选, 默认 `false`) — 大小写不敏感。
//! - `ignoreCase` (bool, 可选) — 旧版别名，等价于 `i`。
//! - `gitignore` (bool, 可选, 默认 `true`) — 是否尊重 `.gitignore`。
//! - `hidden` (bool, 可选, 默认 `false`) — 是否搜索隐藏目录（`.latte/` `.git/` 等）。
//!   把隐藏目录直接作为 `paths` 目标时不受此开关影响。
//! - `skip` (number, 可选, 默认 `0`) — 跳过前 N 个有命中的文件，用于分页。
//! - `limit` (number, 可选, 默认 100, 上限 500) — 单文件最多返回的匹配行数。
//!
//! 另有一个不可配置的总字节上限（100KB，作用于分页后的 matches 文本总量）：
//! 超过即停止追加并置 `truncated: true`，防止多文件命中撑爆 agent context。
//!
//! ## 输出
//!
//! ```jsonc
//! {
//!   "scopePath": "src",              // 首个搜索根的相对路径
//!   "pattern": "TODO",
//!   "filesSearched": 42,              // 扫描过的文件数
//!   "fileCount": 3,                   // 当前页有命中的文件数
//!   "totalFileCount": 3,              // 分页前所有命中文件数（用于翻页决策）
//!   "totalMatches": 17,               // 当前页返回的匹配行数
//!   "matches": [
//!     { "file": "src/main.rs", "line": 12, "content": "// TODO: ..." },
//!     { "file": "src/lib.rs",  "line": 3,  "content": "// TODO: ..." }
//!   ],
//!   "truncated": false,               // true 表示触发了 per-file limit、单行截断或总量预算
//!   "missingPaths": []                // 多目标调用时被跳过的缺失路径
//! }
//! ```
//!
//! 排序：按文件 path 字典序，再按行号升序——便于 agent 顺序阅读。
//!
//! 防爆保护（防止工具结果撑爆模型上下文）：
//! - 单条命中行内容超过 [`MAX_LINE_CONTENT_CHARS`] 字符会被截断（jsonl
//!   会话日志等文件单行可达数 MB）；
//! - 当次返回的命中内容总量超过 [`MAX_TOTAL_CONTENT_BYTES`] 字节后丢弃
//!   后续命中，并置 `truncated: true`。

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use globset::Glob;
use regex::Regex;
use serde_json::{json, Value};

use crate::tools::find::walk_all_files;
use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};

/// 单文件匹配上限硬顶。
const MAX_PER_FILE_LIMIT: u64 = 500;
/// 默认单文件匹配上限。
const DEFAULT_PER_FILE_LIMIT: u64 = 100;
/// 整次扫描的硬超时：60 秒。
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);
/// 单条命中行内容上限（字符数）。jsonl 会话日志等文件单行可达数 MB，
/// 整行返回会直接撑爆模型上下文。
const MAX_LINE_CONTENT_CHARS: usize = 500;
/// 当次返回的命中内容总字节预算。超出后丢弃后续命中并置 `truncated`。
const MAX_TOTAL_CONTENT_BYTES: usize = 100 * 1024;

/// 截断过长的命中行内容，返回 (截断后文本, 是否发生了截断)。
fn truncate_line_content(s: &str) -> (String, bool) {
    if s.chars().count() <= MAX_LINE_CONTENT_CHARS {
        return (s.to_string(), false);
    }
    let cut: String = s.chars().take(MAX_LINE_CONTENT_CHARS).collect();
    (
        format!("{cut}…[行过长已截断，共 {} 字符]", s.chars().count()),
        true,
    )
}

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
        items: None, properties: None, required: None, additional_properties: None,
    }
}

/// 构造一个 schema：所有属性都出现，但只有 `pattern` 在 `required` 里。
fn optional_required(
    props: Vec<(&str, PropertyType, &str)>,
    required: &[&str],
) -> ToolInputSchema {
    let mut p = BTreeMap::new();
    for (name, ty, desc) in props {
        p.insert(name.to_string(), prop(ty, desc));
    }
    ToolInputSchema {
        schema_type: Default::default(),
        properties: p,
        required: Some(required.iter().map(|s| s.to_string()).collect()),
        additional_properties: None,
    }
}

/// 单路径输入 `"include src"` 这类「空格分隔多路径」误用的定向提示。
///
/// 动机：模型看到 `path` 是 string 就会把多个目录塞进一个字符串（jemalloc
/// 实锤：architect 4 次 `path: "include src"`）。原来的 `Path not found:
/// include src` 只说不存在，模型无从判断是路径拼错还是用法错，于是反复
/// 换写法重试。这里在**确认每个空格分段都真实存在**时才改写报错——
/// 避免把「路径里本来就带空格」的正常情况误导成用法错误。
///
/// 返回 `Some(提示文本)` 表示确诊误用；`None` 表示走原样报错。
fn multi_path_misuse_hint(raw: &str, cwd: &std::path::Path) -> Option<String> {
    let segs: Vec<&str> = raw.split_whitespace().collect();
    if segs.len() < 2 {
        return None;
    }
    let all_exist = segs.iter().all(|s| {
        let (root, _) = split_glob(s);
        let resolved = if root.is_absolute() {
            root
        } else {
            cwd.join(&root)
        };
        resolved.exists()
    });
    if !all_exist {
        return None;
    }
    let arr = segs
        .iter()
        .map(|s| format!("\"{}\"", s))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Path not found: {raw} —— `path` 只接受单个路径，不支持空格分隔。\
         这 {n} 个分段单独看都存在，你要搜的应该是多个目标：\
         请改用 paths 数组重试 → \"paths\": [{arr}]",
        n = segs.len()
    ))
}

/// 解析 `paths` 字段为字符串列表。接受 string 或 array 两种形式。
fn parse_paths(input: &Value) -> Vec<String> {
    if let Some(arr) = input.as_array() {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    if let Some(s) = input.as_str() {
        return vec![s.to_string()];
    }
    Vec::new()
}

/// 把 path 拆成 (search_root, glob_pattern)，与 find 工具的语义保持一致。
///
/// - 无 glob 字符 → `(path, None)`，整路径就是 root。
/// - 有 glob 字符 → 第一个 glob 字符之前是 root（""./"" 归一成 "."），之后是 glob，
///   且自动补 `**/` 前缀让 `*.rs` 也能匹配子目录。
fn split_glob(input: &str) -> (PathBuf, Option<Glob>) {
    let glob_chars = [b'*', b'?', b'['];
    let first_glob = input.as_bytes().iter().position(|b| glob_chars.contains(b));
    let Some(idx) = first_glob else {
        return (PathBuf::from(input), None);
    };
    let base = &input[..idx];
    let glob = &input[idx..];
    let glob_pattern = if glob.starts_with("**/") || glob.starts_with('/') {
        glob.to_string()
    } else {
        format!("**/{}", glob)
    };
    let root = if base.is_empty() || base == "." {
        PathBuf::from(".")
    } else {
        PathBuf::from(base.trim_end_matches('/'))
    };
    let parsed = glob_pattern.parse::<Glob>().ok();
    (root, parsed)
}

/// 在单个文件中跑 regex，逐行匹配。返回 (命中行号列表, 是否被 per-file limit 截断)。
fn grep_file(path: &std::path::Path, re: &Regex, per_file_limit: usize) -> (Vec<usize>, bool) {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return (Vec::new(), false), // 二进制或读失败：跳过
    };
    let mut hits = Vec::new();
    let mut truncated = false;
    for (idx, line) in content.lines().enumerate() {
        if re.is_match(line) {
            if hits.len() >= per_file_limit {
                truncated = true;
                break;
            }
            hits.push(idx + 1);
        }
    }
    (hits, truncated)
}

/// 一条 match 输出。
struct MatchHit {
    file: String,
    line: u64,
    content: String,
}

/// 扫描阶段返回的元数据。`hits` 之外还要带 `files_scanned`（实际扫过的文件数）
/// 和 `truncated`（是否被 per-file limit 截断），避免闭包外再去补这两个值。
#[derive(Default)]
struct ScanOutput {
    hits: Vec<MatchHit>,
    files_scanned: u64,
    truncated: bool,
}

/// 构造 `search` 工具定义。
pub fn file_search_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            // --- 1. 解析 pattern --------------------------------------------
            let pattern = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("pattern is required"))?;
            if pattern.is_empty() {
                return Err(crate::error::ToolError::other("pattern must not be empty"));
            }
            // 大小写不敏感：兼容 `i` 和旧版 `ignoreCase`，二者都设时 OR。
            let ignore_case = input
                .get("i")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || input
                    .get("ignoreCase")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            let use_gitignore = input
                .get("gitignore")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            // 默认不扫隐藏目录（.latte/ .git/ 等运行时目录）；显式传 hidden:true 才扫。
            // 注意：显式把隐藏目录本身作为 paths root（如 ".latte/logs"）不受此开关影响。
            let include_hidden = input
                .get("hidden")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            // 编译 regex。空 pattern 已在上一步拒绝；这里只处理非法 regex。
            //
            // `literal: true` 走字面量匹配（内部转义后仍用 regex 引擎，
            // 保留 `i` / 行号 / 分页等全部既有语义）。动机：搜代码符号
            // 时 `LOG(`、`foo[0]`、`a.b` 这类输入按 regex 编译必然失败
            // 或静默匹配错东西——jemalloc 实锤：programmer 搜 `LOG("`
            // 直接吃到 `invalid regex: unclosed group`，白烧一轮。
            let literal = input
                .get("literal")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let base_pattern = if literal {
                regex::escape(pattern)
            } else {
                pattern.to_string()
            };
            let re_pattern = if ignore_case {
                format!("(?i){}", base_pattern)
            } else {
                base_pattern
            };
            let re = Regex::new(&re_pattern).map_err(|e| {
                // 非 literal 模式下把 `literal: true` 作为出路写进错误里：
                // 模型拿到的原始报错只有 regex 语法细节，无从判断「我其实
                // 想搜的是字面量」。
                if literal {
                    crate::error::ToolError::other(format!(
                        "invalid regex '{}' (literal mode): {}",
                        pattern, e
                    ))
                } else {
                    crate::error::ToolError::other(format!(
                        "invalid regex '{}': {}. \
                         若本意是搜字面文本（含 ( ) [ ] . * ? + | \\ 等字符），\
                         请重试并加上 \"literal\": true",
                        pattern, e
                    ))
                }
            })?;

            // --- 2. 解析 paths（支持 string / array / 旧版 path 别名）-----
            // 优先取 `paths`，再 fallback 到 `path`。
            let raw_paths = if input.get("paths").is_some() {
                parse_paths(input.get("paths").unwrap())
            } else if let Some(s) = input.get("path").and_then(|v| v.as_str()) {
                vec![s.to_string()]
            } else {
                // 默认搜 cwd（"."）
                vec![".".to_string()]
            };
            if raw_paths.is_empty() {
                // paths 是空数组 → 也 fallback 到 cwd
                return Err(crate::error::ToolError::other(
                    "paths must not be empty (use \".\" to search cwd)",
                ));
            }

            // --- 3. 解析其它参数 --------------------------------------------
            let requested_skip = input.get("skip").and_then(|v| v.as_u64()).unwrap_or(0);
            let requested_limit = input
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_PER_FILE_LIMIT);
            let per_file_limit = requested_limit.clamp(1, MAX_PER_FILE_LIMIT) as usize;

            // cwd：context 优先，fallback 到当前进程 cwd。
            let cwd: PathBuf = ctx
                .metadata
                .as_ref()
                .and_then(|m| m.get("cwd").and_then(|v| v.as_str()))
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

            // --- 4. 解析 + 分流（missing / valid） -------------------------
            let is_single = raw_paths.len() == 1;
            let mut missing_paths: Vec<String> = Vec::new();
            // 把 (root, glob, original_input) 收集起来，下游统一扫描。
            let mut targets: Vec<(PathBuf, Option<Glob>, String)> = Vec::new();
            for raw in &raw_paths {
                let (root, glob) = split_glob(raw);
                let resolved_root = if root.is_absolute() {
                    root.clone()
                } else {
                    cwd.join(&root)
                };
                // root 不存在 → 单条报错 / 多条跳过。
                if !resolved_root.exists() {
                    if is_single {
                        return Err(crate::error::ToolError::other(
                            multi_path_misuse_hint(raw, &cwd)
                                .unwrap_or_else(|| format!("Path not found: {}", raw)),
                        ));
                    }
                    missing_paths.push(raw.clone());
                    continue;
                }
                targets.push((resolved_root, glob, raw.clone()));
            }
            if targets.is_empty() {
                return Err(crate::error::ToolError::other(format!(
                    "All paths are missing: {}",
                    missing_paths.join(", ")
                )));
            }

            // 闭包前先算出 scope_path——下面 spawn_blocking 会 move 走 `targets`。
            let first_root = targets[0].0.clone();
            let scope_path = {
                let p = first_root
                    .strip_prefix(&cwd)
                    .unwrap_or(&first_root)
                    .to_string_lossy()
                    .replace('\\', "/");
                if p.is_empty() { ".".to_string() } else { p }
            };

            // --- 5. 扫描（spawn_blocking 避免阻塞 runtime）-----------------
            let re = Arc::new(re);
            let cwd_for_blocking = cwd.clone();
            let targets = targets; // move
            let scan = tokio::task::spawn_blocking(
                move || -> Result<ScanOutput, String> {
                    let mut targets = targets;
                    targets.sort_by(|a, b| a.2.cmp(&b.2));
                    let mut out = ScanOutput::default();
                    for (root, glob, _original) in &targets {
                        let matcher = glob.as_ref().map(|g| g.compile_matcher());
                        // 字面文件路径：直接对该文件跑 regex，不再走 walkdir。
                        // `walk_all_files` 会跳过 path==root，导致单文件 target 永远返回空。
                        let files: Vec<PathBuf> = if root.is_file() {
                            vec![root.clone()]
                        } else {
                            walk_all_files(root, include_hidden, use_gitignore)
                        };
                        for f in &files {
                            if let Some(m) = matcher.as_ref() {
                                if !m.is_match(f) {
                                    continue;
                                }
                            }
                            out.files_scanned += 1;
                            let (line_hits, truncated) = grep_file(f, &re, per_file_limit);
                            if truncated {
                                out.truncated = true;
                            }
                            if line_hits.is_empty() {
                                continue;
                            }
                            let content = match std::fs::read_to_string(f) {
                                Ok(s) => s,
                                Err(_) => continue,
                            };
                            let lines: Vec<&str> = content.lines().collect();
                            let rel = f
                                .strip_prefix(&cwd_for_blocking)
                                .unwrap_or(f)
                                .to_string_lossy()
                                .replace('\\', "/");
                            for ln in &line_hits {
                                let line_idx = ln.saturating_sub(1);
                                let (text, trimmed) = truncate_line_content(
                                    lines.get(line_idx).copied().unwrap_or(""),
                                );
                                if trimmed {
                                    out.truncated = true;
                                }
                                out.hits.push(MatchHit {
                                    file: rel.clone(),
                                    line: *ln as u64,
                                    content: text,
                                });
                            }
                        }
                    }
                    Ok(out)
                },
            );

            // --- 6. 收结果 + 分页 -----------------------------------------
            let scan_result = tokio::time::timeout(SEARCH_TIMEOUT, scan)
                .await
                .map_err(|_| crate::error::ToolError::timeout("search", SEARCH_TIMEOUT))?;
            let mut scan_out = match scan_result {
                Ok(Ok(s)) => s,
                Ok(Err(msg)) => return Err(crate::error::ToolError::other(msg)),
                Err(join) => {
                    return Err(crate::error::ToolError::execution_str(
                        "search",
                        format!("scan task panicked: {}", join),
                    ));
                }
            };

            // 排序：按 file 字典序，再按 line 升序——这样分页按 distinct file 跳过的逻辑才简单。
            scan_out.hits.sort_by(|a, b| {
                a.file.cmp(&b.file).then(a.line.cmp(&b.line))
            });

            // 统计「有命中的文件」的去重数（这是分页前的总数）。
            let mut distinct_files: BTreeSet<String> = BTreeSet::new();
            for h in &scan_out.hits {
                distinct_files.insert(h.file.clone());
            }
            let total_file_count = distinct_files.len();

            // `skip`：跳过前 N 个有命中的文件。
            // hits 已按 file 字典序排好；遇到新 file 名时计数，达到 skip 后开始保留。
            if requested_skip > 0 && !scan_out.hits.is_empty() {
                let mut seen: BTreeSet<String> = BTreeSet::new();
                let mut skip_remaining = requested_skip as usize;

                scan_out.hits.retain(|h| {
                    if skip_remaining == 0 {
                        return true;
                    }
                    if seen.insert(h.file.clone()) {
                        // 第一次见到这个 file 名——它就是要被 skip 的下一个文件。
                        skip_remaining -= 1;
                        return false;
                    }
                    // 同一个 file 内的后续 hit：仍处于「整个文件被 skip」的状态。
                    false
                });
            }

            // --- 7. 组装结果 ---------------------------------------------
            // 总内容预算：海量命中（即使每行已截断）也会撑爆模型上下文。
            // 上限作用在分页之后：totalFileCount 仍是分页前的真实统计，只是
            // 返回的 matches 被截断（不破坏 skip 翻页语义）。超预算后丢弃
            // 后续命中、置 truncated，并按实际返回重算计数。
            let mut matches: Vec<Value> = Vec::with_capacity(scan_out.hits.len());
            let mut budget_left = MAX_TOTAL_CONTENT_BYTES;
            let mut kept_files: BTreeSet<String> = BTreeSet::new();
            let mut budget_truncated = false;
            for h in scan_out.hits.into_iter() {
                if h.content.len() > budget_left {
                    budget_truncated = true;
                    break;
                }
                budget_left -= h.content.len();
                kept_files.insert(h.file.clone());
                matches.push(json!({
                    "file": h.file,
                    "line": h.line,
                    "content": h.content,
                }));
            }
            // fileCount / totalMatches 反映实际返回的内容（截断后）。
            let new_file_count = kept_files.len();
            let total_matches = matches.len();

            Ok(json!({
                "scopePath": scope_path,
                "pattern": pattern,
                "fileCount": new_file_count,
                "totalFileCount": total_file_count,
                "totalMatches": total_matches,
                "filesSearched": scan_out.files_scanned,
                "matches": matches,
                "truncated": scan_out.truncated || budget_truncated,
                "missingPaths": missing_paths,
            }))
        }
        .boxed()
    };

    // Schema 必须把**所有**实现支持的参数都声明出来：模型只能看到
    // schema，看不到本文件顶部的模块文档。此前这里只声明了 `pattern`，
    // 于是 `paths` 数组形同不存在——jemalloc 实锤：architect 连续 4 次
    // 传 `path: "include src"`（想搜两个目录，只能靠猜），全部报
    // `Path not found`。
    Tool::builder(
        "search",
        "按正则（或 literal 字面量）搜索文件内容，返回 file:line + 命中行。默认在项目根（.）递归搜索，跳过隐藏目录（.latte/ .git/ 等运行时目录）并尊重 .gitignore；如需搜隐藏目录传 hidden:true，或把该目录直接作为 paths 目标。强烈建议用 paths 限制搜索范围（如 src/ tests/）以控制返回量。支持按文件分页（skip）、每文件匹配上限（limit）与单次返回总字节上限（超出置 truncated）。",
        optional_required(
            vec![
                (
                    "pattern",
                    PropertyType::String,
                    "必填。regex 模式；配合 literal=true 时按字面量匹配。大小写是否敏感由 i / ignoreCase 控制。",
                ),
                (
                    "literal",
                    PropertyType::Boolean,
                    "可选，默认 false。true = pattern 按字面文本匹配（自动转义）。\
                     搜代码符号如 LOG( 、foo[0] 、a.b 时必须用这个，否则会被当 regex 解析而报错",
                ),
                (
                    "paths",
                    PropertyType::Array,
                    "可选，string[]，默认 [\".\"]。搜索目标，每个元素可以是文件路径、\
                     目录路径（递归）或 glob（如 src/**/*.rs）。\
                     搜多个目录必须用数组：[\"include\", \"src\"]——\
                     不要写成一个空格分隔的字符串。应显式限定到 src/ tests/ 等源码目录以控制返回量。",
                ),
                (
                    "path",
                    PropertyType::String,
                    "可选。单路径写法，等价于 paths: [该值]。只接受一个路径，\
                     不支持空格分隔多路径",
                ),
                (
                    "hidden",
                    PropertyType::Boolean,
                    "是否搜索隐藏目录（.latte/ .git/ 等），默认 false。把隐藏目录直接作为 paths 目标时不受此开关影响。",
                ),
                (
                    "i",
                    PropertyType::Boolean,
                    "可选，默认 false。大小写不敏感（别名 ignoreCase）",
                ),
                (
                    "gitignore",
                    PropertyType::Boolean,
                    "可选，默认 true。是否尊重 .gitignore",
                ),
                (
                    "skip",
                    PropertyType::Integer,
                    "可选，默认 0。跳过前 N 个有命中的文件，用于翻页（配合返回的 totalFileCount）",
                ),
                (
                    "limit",
                    PropertyType::Integer,
                    "可选，默认 100，上限 500。单个文件最多返回的匹配行数",
                ),
            ],
            &["pattern"],
        ),
        Arc::new(handler),
    )
    .concurrency_safe(true)
    .strict(true)
    .timeout(SEARCH_TIMEOUT)
    .build()
}

/// 把 `search` 注册到 `FileToolsPackage`。
pub fn add_file_search(pkg: &mut ToolPackage) {
    pkg.tools.push(file_search_tool());
}

// =============================================================================
// 单测
// =============================================================================
//
// 测试策略：用 `tempfile::tempdir` 建一个真实文件树，跑 search tool，
// 断言：基本命中、大小写不敏感、多目标、missing path 分页、gitignore、glob 路径。
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 建一棵搜索测试用的小文件树。
    ///
    /// 结构（相对 `root`）：
    /// ```text
    /// root/
    ///   src/
    ///     main.rs        // contains: "TODO main", "fn main"
    ///     lib.rs         // contains: "todo lib", "fn lib"
    ///   tests/
    ///     test_main.rs   // contains: "TODO test"
    ///   README.md        // contains: "TODO: write docs"
    ///   .gitignore       // target/
    ///   target/
    ///     ignored.rs     // contains: "TODO"  — 应被 .gitignore 排除
    /// ```
    fn build_tree() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("tests")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("src").join("main.rs"), "// TODO main\nfn main() {}\n").unwrap();
        fs::write(root.join("src").join("lib.rs"), "// todo lib\nfn lib() {}\n").unwrap();
        fs::write(
            root.join("tests").join("test_main.rs"),
            "// TODO test\nfn test() {}\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "# README\n\nTODO: write docs\n").unwrap();
        fs::write(
            root.join("target").join("ignored.rs"),
            "// TODO\nfn ignored() {}\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        dir
    }

    async fn run_in(cwd: PathBuf, input: Value) -> Result<Value, crate::error::ToolError> {
        let tool = file_search_tool();
        let mut ctx = ToolExecutionContext::fresh("search", 0);
        ctx.metadata = Some(json!({ "cwd": cwd.to_string_lossy() }));
        (tool.handler)(input, ctx).await
    }

    /// 测试：基础搜索 — 在 . 上找 "TODO"，应命中 3 个文件（README + src/main + tests/test），
    /// 跳过 target/ignored（被 gitignore 排除）。
    #[tokio::test]
    async fn search_basic_finds_todo_across_files() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "TODO" }),
        )
        .await
        .unwrap();
        let files: BTreeSet<String> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap().to_string())
            .collect();
        assert!(files.contains("README.md"), "files: {:?}", files);
        assert!(files.contains("src/main.rs"), "files: {:?}", files);
        assert!(files.contains("tests/test_main.rs"), "files: {:?}", files);
        // target/ignored.rs 不应在结果里
        assert!(!files.iter().any(|f| f.contains("target")), "files: {:?}", files);
    }

    /// 测试：大小写不敏感 — 搜 "todo"（小写）应同时命中 "TODO"（大写）。
    #[tokio::test]
    async fn search_case_insensitive() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "todo", "i": true }),
        )
        .await
        .unwrap();
        let files: BTreeSet<String> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap().to_string())
            .collect();
        assert!(files.contains("README.md"));
        assert!(files.contains("src/main.rs"));
        assert!(files.contains("src/lib.rs"));
    }

    /// 测试：paths 数组可以指定多个目标。
    #[tokio::test]
    async fn search_multi_paths() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "pattern": "TODO",
                "paths": ["src/main.rs", "tests/test_main.rs"]
            }),
        )
        .await
        .unwrap();
        let files: BTreeSet<String> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(files.len(), 2);
        assert!(files.contains("src/main.rs"));
        assert!(files.contains("tests/test_main.rs"));
    }

    /// 测试：paths 中一条缺失（多目标）→ 跳过缺失项，其它正常返回。
    #[tokio::test]
    async fn search_multi_path_skips_missing() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "pattern": "TODO",
                "paths": ["nope/", "src/main.rs"]
            }),
        )
        .await
        .unwrap();
        let missing: Vec<String> = out["missingPaths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(missing, vec!["nope/".to_string()]);
        let files: BTreeSet<String> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap().to_string())
            .collect();
        assert!(files.contains("src/main.rs"));
    }

    /// 测试：单条 path 缺失 → 直接报错。
    #[tokio::test]
    async fn search_single_missing_path_errors() {
        let dir = build_tree();
        let err = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "TODO", "paths": ["nope/"] }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Path not found"));
    }

    /// 测试：gitignore=false 时，target/ignored.rs 也会被搜到。
    #[tokio::test]
    async fn search_disabling_gitignore_finds_ignored_files() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "pattern": "TODO",
                "gitignore": false
            }),
        )
        .await
        .unwrap();
        let files: BTreeSet<String> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap().to_string())
            .collect();
        assert!(
            files.contains("target/ignored.rs"),
            "files: {:?}",
            files
        );
    }

    /// 测试：glob 路径模式 — `src/*.rs` 应只搜 src 下的 .rs。
    #[tokio::test]
    async fn search_glob_path_filters_scope() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "pattern": "TODO",
                "paths": ["src/*.rs"]
            }),
        )
        .await
        .unwrap();
        let files: BTreeSet<String> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(files.len(), 1);
        assert!(files.contains("src/main.rs"));
    }

    /// 测试：skip 翻页 — 跳过前 N 个有命中的文件。
    #[tokio::test]
    async fn search_skip_pagination() {
        let dir = build_tree();
        // 不 skip 应该有 3 个文件命中
        let full = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "TODO" }),
        )
        .await
        .unwrap();
        let full_files: BTreeSet<String> = full["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap().to_string())
            .collect();
        assert!(full_files.len() >= 3);
        let full_total = full["totalFileCount"].as_u64().unwrap();
        assert!(full_total >= 3);

        // skip=2 之后，应跳过前 2 个有命中的文件
        let paged = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "TODO", "skip": 2 }),
        )
        .await
        .unwrap();
        let paged_files: BTreeSet<String> = paged["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap().to_string())
            .collect();
        // 分页后文件数应少于全量
        assert!(
            (paged_files.len() as u64) < full_total,
            "paged={} full={}",
            paged_files.len(),
            full_total
        );
        // paged 出现的所有文件都应是 full 里的（不能凭空多出来）
        for f in &paged_files {
            assert!(full_files.contains(f), "paged 出现了全量里没见过的文件: {}", f);
        }
        // totalFileCount 不受 skip 影响
        assert_eq!(paged["totalFileCount"], full["totalFileCount"]);
    }

    /// 测试：空 pattern → 报错。
    #[tokio::test]
    async fn search_empty_pattern_errors() {
        let dir = build_tree();
        let err = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "" }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    /// 测试：非法 regex → 报错。
    #[tokio::test]
    async fn search_invalid_regex_errors() {
        let dir = build_tree();
        let err = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "(unclosed" }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }

    /// 测试：非法 regex 的报错必须把 `literal: true` 这条出路写出来。
    /// 回归防线：模型只能从报错文本里学到修正手段。
    #[tokio::test]
    async fn search_invalid_regex_error_suggests_literal() {
        let dir = build_tree();
        let err = run_in(dir.path().to_path_buf(), json!({ "pattern": "LOG(\"" }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid regex"), "msg = {msg}");
        assert!(msg.contains("literal"), "报错未提示 literal 出路: {msg}");
    }

    /// 测试：`literal: true` 让 regex 元字符按字面量匹配。
    #[tokio::test]
    async fn search_literal_mode_matches_metacharacters() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.c"),
            "LOG(\"hit\");\nLOGX(\"miss\");\nfoo[0] = 1;\n",
        )
        .unwrap();

        // 不加 literal：`LOG("` 是非法 regex，直接报错（现状）。
        let err = run_in(dir.path().to_path_buf(), json!({ "pattern": "LOG(\"" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid regex"));

        // 加 literal：命中且只命中字面量那一行。
        let out = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "LOG(\"", "literal": true }),
        )
        .await
        .unwrap();
        let lines: Vec<u64> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["line"].as_u64().unwrap())
            .collect();
        assert_eq!(lines, vec![1], "literal 应只命中第 1 行: {out}");

        // `foo[0]` 在 regex 下是字符类（匹配 "foo0"，本文件里没有）；
        // literal 下应当命中第 3 行。
        let out = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "foo[0]", "literal": true }),
        )
        .await
        .unwrap();
        assert_eq!(out["totalMatches"].as_u64().unwrap(), 1, "{out}");

        // literal 与 i 组合仍然生效：小写 `log("` 只有在忽略大小写时命中。
        let out = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "log(\"", "literal": true }),
        )
        .await
        .unwrap();
        assert_eq!(
            out["totalMatches"].as_u64().unwrap(),
            0,
            "literal 不该忽略大小写: {out}"
        );
        let out = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "log(\"", "literal": true, "i": true }),
        )
        .await
        .unwrap();
        assert_eq!(out["totalMatches"].as_u64().unwrap(), 1, "{out}");
    }

    /// 测试：`path: "include src"`（空格分隔多路径）→ 报错要指出用法错误，
    /// 并给出可直接照抄的 `paths` 数组。
    #[tokio::test]
    async fn search_space_separated_path_suggests_paths_array() {
        let dir = build_tree();
        let err = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "TODO", "path": "src tests" }),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("不支持空格分隔"), "msg = {msg}");
        assert!(
            msg.contains("\"paths\": [\"src\", \"tests\"]"),
            "未给出可照抄的数组: {msg}"
        );
    }

    /// 测试：分段并非都存在时不误判为用法错误——路径里本来就带空格
    /// （或真的拼错了）应保持原样 `Path not found`，不要给误导性建议。
    #[tokio::test]
    async fn search_space_in_path_is_not_misreported_as_misuse() {
        let dir = build_tree();
        let err = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "TODO", "path": "src nope" }),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Path not found"), "msg = {msg}");
        assert!(!msg.contains("不支持空格分隔"), "误判为用法错误: {msg}");
    }

    /// 测试：匹配项按 file 字典序、再按 line 升序排。
    #[tokio::test]
    async fn search_results_sorted_by_file_then_line() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({ "pattern": "TODO" }),
        )
        .await
        .unwrap();
        let matches: Vec<Value> = out["matches"].as_array().unwrap().clone();
        let mut prev: Option<(&str, u64)> = None;
        for m in &matches {
            let f = m["file"].as_str().unwrap();
            let l = m["line"].as_u64().unwrap();
            if let Some((pf, pl)) = prev {
                assert!(
                    (f, l) >= (pf, pl),
                    "results not sorted: prev=({},{}) curr=({},{})",
                    pf, pl, f, l
                );
            }
            prev = Some((f, l));
        }
    }

    /// 测试：旧版 `path` + `ignoreCase` 字段仍可用（向后兼容）。
    #[tokio::test]
    async fn search_backward_compat_old_field_names() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "pattern": "todo",
                "path": "src",
                "ignoreCase": true
            }),
        )
        .await
        .unwrap();
        let matches = out["matches"].as_array().unwrap();
        assert!(!matches.is_empty());
    }

    /// 测试：per-file limit 触发截断标记。
    #[tokio::test]
    async fn search_per_file_limit_truncates() {
        // 建一个文件，里面有 200 行 "TODO x"，超出默认 limit=100
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!("// TODO line {}\n", i));
        }
        fs::write(root.join("dense.rs"), content).unwrap();

        let out = run_in(
            root.to_path_buf(),
            json!({ "pattern": "TODO" }),
        )
        .await
        .unwrap();
        assert_eq!(out["truncated"], true);
        // 200 个匹配被截断到默认 100
        let matches = out["matches"].as_array().unwrap();
        assert_eq!(matches.len(), DEFAULT_PER_FILE_LIMIT as usize);
    }

    /// 测试：超长命中行被截断到 MAX_LINE_CONTENT_CHARS，并置 truncated。
    /// （jsonl 会话日志单行可达数 MB，整行返回会撑爆模型上下文。）
    #[tokio::test]
    async fn search_long_line_content_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let long_line = format!("TODO {}", "x".repeat(MAX_LINE_CONTENT_CHARS * 10));
        fs::write(root.join("long.rs"), format!("{long_line}\n")).unwrap();

        let out = run_in(root.to_path_buf(), json!({ "pattern": "TODO" }))
            .await
            .unwrap();
        assert_eq!(out["truncated"], true);
        let matches = out["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        let content = matches[0]["content"].as_str().unwrap();
        assert!(
            content.chars().count() <= MAX_LINE_CONTENT_CHARS + 30,
            "content len: {}",
            content.chars().count()
        );
        assert!(content.contains("行过长已截断"), "content: {content}");
    }

    /// 测试：命中内容总量超预算后丢弃后续命中并置 truncated。
    #[tokio::test]
    async fn search_total_content_budget_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // 每行 ~400 字符（截断阈值内），行数足以超过 100KB 总预算。
        let line = format!("// TODO {}", "y".repeat(400));
        let mut content = String::new();
        for _ in 0..500 {
            content.push_str(&line);
            content.push('\n');
        }
        fs::write(root.join("big.rs"), content).unwrap();

        let out = run_in(
            root.to_path_buf(),
            json!({ "pattern": "TODO", "limit": 500 }),
        )
        .await
        .unwrap();
        assert_eq!(out["truncated"], true);
        let matches = out["matches"].as_array().unwrap();
        let total_bytes: usize = matches
            .iter()
            .map(|m| m["content"].as_str().unwrap().len())
            .sum();
        assert!(total_bytes <= MAX_TOTAL_CONTENT_BYTES, "total: {total_bytes}");
        assert!(matches.len() < 500, "matches: {}", matches.len());
        assert_eq!(out["totalMatches"].as_u64().unwrap(), matches.len() as u64);
    }

    /// 测试：`.git` 目录即使 hidden=true 也永远跳过。
    #[tokio::test]
    async fn search_never_enters_git_dir() {
        let dir = build_tree();
        let root = dir.path();
        fs::create_dir_all(root.join(".git").join("logs")).unwrap();
        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/TODO\n").unwrap();
        fs::write(root.join(".git").join("logs").join("HEAD"), "TODO commit\n").unwrap();

        let out = run_in(root.to_path_buf(), json!({ "pattern": "TODO" }))
            .await
            .unwrap();
        let files: Vec<&str> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap())
            .collect();
        assert!(
            !files.iter().any(|f| f.contains(".git")),
            "files: {:?}",
            files
        );
        // 正常命中不受影响
        assert!(files.contains(&"README.md"), "files: {:?}", files);
    }

    /// 测试：`.latte` 运行时状态目录（ui-sessions / workflow-runs 日志）即使
    /// hidden=true 也永远跳过——日志包含 agent 搜过的关键词，搜它会自引用污染。
    #[tokio::test]
    async fn search_never_enters_latte_dir() {
        let dir = build_tree();
        let root = dir.path();
        fs::create_dir_all(root.join(".latte").join("ui-sessions")).unwrap();
        fs::write(
            root.join(".latte").join("ui-sessions").join("ui-1-0.jsonl"),
            "{\"type\":\"ToolUse\",\"args\":\"TODO\"\n",
        )
        .unwrap();

        let out = run_in(root.to_path_buf(), json!({ "pattern": "TODO" }))
            .await
            .unwrap();
        let files: Vec<&str> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap())
            .collect();
        assert!(
            !files.iter().any(|f| f.contains(".latte")),
            "files: {:?}",
            files
        );
        // 正常命中不受影响
        assert!(files.contains(&"README.md"), "files: {:?}", files);
    }

    /// 测试：默认不搜隐藏目录；hidden:true 时才搜。（.git/.latte 例外：
    /// 两者永远跳过，由 search_never_enters_* 两条锁定。）
    #[tokio::test]
    async fn search_excludes_hidden_dirs_by_default() {
        let dir = build_tree();
        let root = dir.path();
        fs::create_dir_all(root.join(".hidden").join("logs")).unwrap();
        fs::write(
            root.join(".hidden").join("logs").join("run.log"),
            "TODO hidden hit\n",
        )
        .unwrap();

        // 默认：隐藏目录里的命中不应出现
        let out = run_in(root.to_path_buf(), json!({ "pattern": "TODO" }))
            .await
            .unwrap();
        let files: Vec<&str> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap())
            .collect();
        assert!(
            !files.iter().any(|f| f.contains(".hidden")),
            "files: {:?}",
            files
        );

        // hidden:true → 能搜到
        let out = run_in(
            root.to_path_buf(),
            json!({ "pattern": "TODO", "hidden": true }),
        )
        .await
        .unwrap();
        let files: Vec<&str> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap())
            .collect();
        assert!(
            files.iter().any(|f| f.contains(".hidden")),
            "files: {:?}",
            files
        );
    }

    /// 测试：把 .latte 显式作为 paths root 时，不受硬跳过影响。
    #[tokio::test]
    async fn search_hidden_dir_as_explicit_root_still_works() {
        let dir = build_tree();
        let root = dir.path();
        fs::create_dir_all(root.join(".latte").join("logs")).unwrap();
        fs::write(
            root.join(".latte").join("logs").join("run.log"),
            "TODO hidden hit\n",
        )
        .unwrap();

        let out = run_in(
            root.to_path_buf(),
            json!({ "pattern": "TODO", "paths": [".latte"] }),
        )
        .await
        .unwrap();
        let files: Vec<&str> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["file"].as_str().unwrap())
            .collect();
        assert!(
            files.iter().any(|f| f.contains(".latte")),
            "files: {:?}",
            files
        );
    }
}
