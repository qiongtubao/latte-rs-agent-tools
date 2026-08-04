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
//! - `paths` (string | string[], 可选, 默认 `["."]`) — 搜索目标。每个元素可以是：
//!   - 字面文件路径（只搜这一个文件）
//!   - 字面目录路径（递归搜整个目录）
//!   - glob 模式（如 `src/**/*.rs`）
//! - `path` (string, 可选) — 兼容旧版单路径输入，等价于 `paths: ["..."]`。
//! - `i` (bool, 可选, 默认 `false`) — 大小写不敏感。
//! - `ignoreCase` (bool, 可选) — 旧版别名，等价于 `i`。
//! - `gitignore` (bool, 可选, 默认 `true`) — 是否尊重 `.gitignore`。
//! - `skip` (number, 可选, 默认 `0`) — 跳过前 N 个有命中的文件，用于分页。
//! - `limit` (number, 可选, 默认 100, 上限 500) — 单文件最多返回的匹配行数。
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
//!   "truncated": false,               // true 表示某些文件触发了 per-file limit
//!   "missingPaths": []                // 多目标调用时被跳过的缺失路径
//! }
//! ```
//!
//! 排序：按文件 path 字典序，再按行号升序——便于 agent 顺序阅读。

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

            // 编译 regex。空 pattern 已在上一步拒绝；这里只处理非法 regex。
            let re_pattern = if ignore_case {
                format!("(?i){}", pattern)
            } else {
                pattern.to_string()
            };
            let re = Regex::new(&re_pattern).map_err(|e| {
                crate::error::ToolError::other(format!("invalid regex '{}': {}", pattern, e))
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
                        return Err(crate::error::ToolError::other(format!(
                            "Path not found: {}",
                            raw
                        )));
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
                            walk_all_files(root, /* hidden */ true, use_gitignore)
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
                                let text = lines
                                    .get(line_idx)
                                    .copied()
                                    .unwrap_or("")
                                    .to_string();
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

            // 重新统计分页后的 fileCount + totalMatches。
            let mut new_files: BTreeSet<String> = BTreeSet::new();
            for h in &scan_out.hits {
                new_files.insert(h.file.clone());
            }
            let new_file_count = new_files.len();
            let total_matches = scan_out.hits.len();

            // --- 7. 组装结果 ---------------------------------------------
            let matches: Vec<Value> = scan_out
                .hits
                .into_iter()
                .map(|h| {
                    json!({
                        "file": h.file,
                        "line": h.line,
                        "content": h.content,
                    })
                })
                .collect();

            Ok(json!({
                "scopePath": scope_path,
                "pattern": pattern,
                "fileCount": new_file_count,
                "totalFileCount": total_file_count,
                "totalMatches": total_matches,
                "filesSearched": scan_out.files_scanned,
                "matches": matches,
                "truncated": scan_out.truncated,
                "missingPaths": missing_paths,
            }))
        }
        .boxed()
    };

    Tool::builder(
        "search",
        "按正则搜索文件内容。默认在项目根（.）递归搜索全部文件（含 .latte/ 等运行时目录）；强烈建议用 paths 限制搜索范围，避免命中历史日志/大文件导致返回过大。支持按文件分页（skip）与每文件匹配上限（limit）。",
        optional_required(
            vec![
                (
                    "pattern",
                    PropertyType::String,
                    "regex 模式，必填。大小写是否敏感由 i / ignoreCase 控制。",
                ),
                (
                    "paths",
                    PropertyType::Array,
                    "搜索目标：文件、目录或 glob（如 \"src/**/*.rs\"）。可传字符串或字符串数组。默认为 \".\"（整个项目根，会扫入运行时目录）。应显式限定到 src/ tests/ 等源码目录以控制返回量。",
                ),
                (
                    "i",
                    PropertyType::Boolean,
                    "是否大小写不敏感（默认 false）。",
                ),
                (
                    "skip",
                    PropertyType::Number,
                    "跳过前 N 个有命中的文件，用于分页（配合 totalFileCount 翻页）。默认 0。",
                ),
                (
                    "limit",
                    PropertyType::Number,
                    "单个文件最多返回的匹配行数，上限 500，默认 100。",
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
}
