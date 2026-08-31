//! File-system find tool. Mirrors the `find` tool in oh-my-pi/coding-agent.
//!
//! 提供 `find` 工具：在目录树下按 glob 模式查找文件，并按 mtime 倒序排列。
//! 与 `list`（只列一层）不同，`find` 走 `ignore::WalkBuilder` 跨目录递归，
//! 自动尊重 `.gitignore` / `.ignore` / `.git/info/exclude`，并支持 `**` 递归 glob。
//!
//! ## 输入
//!
//! 字段除 `paths` 外都可选：
//!
//! - `paths` (array<string>, 必填) — glob 模式数组，每条都形如 `dir/**/glob`。
//!   - 不含 glob 字符的条目按字面路径处理（文件则单条返回，目录则递归）。
//!   - 相对路径按 `ToolExecutionContext.metadata.cwd` 解析（fallback 到当前进程 cwd）。
//!   - 多目标时单条丢失会跳过，全部丢失才报错。
//! - `hidden` (bool, 默认 `true`) — 是否包含以 `.` 开头的隐藏文件 / 目录。
//! - `gitignore` (bool, 默认 `true`) — 是否尊重 `.gitignore` / `.git/info/exclude`。
//!   注意：本工具不要求传入目录是 git 仓库——非 git 目录里就只是没有规则可读。
//! - `limit` (number, 默认 200, 上限 200) — 最多返回的条目数；超过会被截断。
//! - `timeout` (number, 默认 5, 范围 0.5–60, 秒) — 单次扫描的硬超时；超时后
//!   返回 `ToolError::ToolTimeout`（保留与其它工具一致的超时语义）。
//!
//! ## 输出
//!
//! ```jsonc
//! {
//!   "scopePath": "src",              // 首个搜索根的相对路径
//!   "fileCount": 12,                  // 实际返回的条目数（已截断）
//!   "files": [                        // 相对 cwd 的路径列表，目录会带尾斜杠
//!     "src/main.rs",
//!     "src/utils/mod.rs",
//!     "src/tools/git.rs"
//!   ],
//!   "truncated": false,               // true 表示触发了 limit 截断
//!   "missingPaths": [],               // 多目标调用时被跳过的缺失路径
//!   "recursiveFromDirectory": true    // path 是字面目录时为 true
//! }
//! ```
//!
//! 排序：按 mtime 倒序（新文件在前），相同 mtime 时按 path 字典序稳定排序。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use futures::FutureExt;
use globset::Glob;
use serde_json::{json, Value};

use crate::types::{
    PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage,
};

/// 默认 limit：200。
const DEFAULT_LIMIT: u64 = 200;
/// limit 硬上限：200（与 TS 版本对齐）。
const MAX_LIMIT: u64 = 200;
/// 默认超时：5 秒。
const DEFAULT_TIMEOUT_SECS: f64 = 5.0;
/// 超时下 / 上限：0.5 秒 / 60 秒。
const MIN_TIMEOUT_SECS: f64 = 0.5;
const MAX_TIMEOUT_SECS: f64 = 60.0;

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

/// 解析后的单条搜索目标。
struct FindTarget {
    /// 要遍历的根目录。空字符串代表当前目录。
    search_path: PathBuf,
    /// glob 模式（无 glob 时为空串）。
    glob_pattern: String,
    /// 是否含 glob 字符。
    has_glob: bool,
}

/// 在 `input` 中找第一个 glob 字符（`*` `?` `[`）的字节位置。
fn first_glob_byte(input: &str) -> Option<usize> {
    input
        .as_bytes()
        .iter()
        .position(|&b| b == b'*' || b == b'?' || b == b'[')
}

/// 解析单条 find pattern：把 `dir/**/glob` 拆成 `search_path=dir, glob_pattern=**/glob`。
///
/// 规则：
/// - 第一个 glob 字符之前是 `search_path`，之后是 `glob_pattern`。
/// - `glob_pattern` 若不以 `**/` 开头且不是绝对 glob，自动加 `**/` 前缀，
///   这样 `*.rs` 能匹配 `src/foo.rs`，而不是只在 base 目录里找。
/// - 没有 glob 字符时 `has_glob=false`。
fn parse_find_pattern(input: &str) -> FindTarget {
    if let Some(idx) = first_glob_byte(input) {
        let base = &input[..idx];
        let glob = &input[idx..];
        let glob_pattern = if glob.starts_with("**/") || glob.starts_with('/') {
            glob.to_string()
        } else {
            format!("**/{}", glob)
        };
        let search_path = if base.is_empty() || base == "." {
            PathBuf::from(".")
        } else {
            // 去掉末尾的 `/`（如果有的话）
            let trimmed = base.trim_end_matches('/');
            PathBuf::from(trimmed)
        };
        FindTarget {
            search_path,
            glob_pattern,
            has_glob: true,
        }
    } else {
        FindTarget {
            search_path: PathBuf::from(input),
            glob_pattern: String::new(),
            has_glob: false,
        }
    }
}

/// 一条搜索结果。
struct FindHit {
    /// 绝对路径。
    abs_path: PathBuf,
    /// mtime（毫秒 since epoch）。
    mtime_ms: i64,
    /// 是否目录（影响是否在结果里加尾斜杠）。
    is_dir: bool,
}

/// 走 `root` 下的所有文件，应用 hidden / gitignore 过滤，返回绝对路径列表。
///
/// 这是 `walk_target` 的去 glob 化版本，供 `search` 等需要自己处理每个文件的工具复用。
/// 阻塞遍历——调用方应放在 `spawn_blocking` 里，并配 `tokio::time::timeout`。
pub(crate) fn walk_all_files(
    root: &Path,
    include_hidden: bool,
    use_gitignore: bool,
) -> Vec<PathBuf> {
    // 读 `.gitignore`（如果要求）。`Gitignore::new` 在文件不存在时返回 Ok + 空 matcher，
    // 所以非 git 目录也能跑。
    let gitignore = if use_gitignore {
        let (gi, _err) = ignore::gitignore::Gitignore::new(root.join(".gitignore"));
        Some(gi)
    } else {
        None
    };

    // `.git` / `.latte` 目录永远跳过（无论 include_hidden / gitignore 如何设置）：
    // `.git` 内容对搜索类工具没有价值且体积巨大（objects/pack、logs），rg 等
    // 工具默认也跳过；`.latte` 是 latte 自己的运行时状态目录（ui-sessions /
    // workflow-runs 日志、task board），里面的日志包含 agent 搜过的每个关键词，
    // 搜它会产生自引用污染（把日志里的提案文本误认为项目内容）。
    // root 自身是这些目录时仍允许显式搜索。
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.path() == root
                || !(e.file_type().is_dir()
                    && (e.file_name() == ".git" || e.file_name() == ".latte"))
        });

    // 判断 `path` 自身或任何祖先目录（不含 root）是否被 gitignore 标记为 ignore。
    // `gi.matched` 不会沿父链上溯，所以必须自己走一遍。
    let is_ignored = |p: &Path, is_dir: bool| -> bool {
        let Some(gi) = gitignore.as_ref() else { return false };
        if gi.matched(p, is_dir).is_ignore() {
            return true;
        }
        let mut ancestor = p.parent();
        while let Some(a) = ancestor {
            if a == root || a.parent().is_none() {
                break;
            }
            if gi.matched(a, /* is_dir */ true).is_ignore() {
                return true;
            }
            ancestor = a.parent();
        }
        false
    };

    // 判断 `path` 的任何祖先目录（不含 root）的 basename 是否以 `.` 开头。
    let path_has_hidden_ancestor = |p: &Path| -> bool {
        let mut current = p;
        loop {
            let Some(parent) = current.parent() else { return false };
            if parent == root || parent.parent().is_none() {
                return false;
            }
            if let Some(name) = parent.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') {
                    return true;
                }
            }
            current = parent;
        }
    };

    let mut out: Vec<PathBuf> = Vec::new();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path == root {
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if !include_hidden && path_has_hidden_ancestor(path) {
            continue;
        }
        if is_ignored(path, /* is_dir */ false) {
            continue;
        }
        out.push(path.to_path_buf());
    }
    out
}

/// 走 `root` 下的所有文件，按 `glob_pattern` 过滤，并收集 mtime。
fn walk_target(
    root: &Path,
    glob_pattern: &str,
    include_hidden: bool,
    use_gitignore: bool,
) -> Result<Vec<FindHit>, crate::error::ToolError> {
    let matcher = match glob_pattern.parse::<Glob>() {
        Ok(g) => g.compile_matcher(),
        Err(e) => {
            return Err(crate::error::ToolError::other(format!(
                "invalid glob pattern '{}': {}",
                glob_pattern, e
            )))
        }
    };

    let mut hits: Vec<FindHit> = Vec::new();
    for path in walk_all_files(root, include_hidden, use_gitignore) {
        if !matcher.is_match(&path) {
            continue;
        }
        let mtime_ms = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        hits.push(FindHit {
            abs_path: path,
            mtime_ms,
            is_dir: false,
        });
    }
    Ok(hits)
}

/// mtime 倒序排；同 mtime 时按 path 字典序稳定排序。
fn sort_hits(hits: &mut [FindHit]) {
    hits.sort_by(|a, b| {
        b.mtime_ms
            .cmp(&a.mtime_ms)
            .then_with(|| a.abs_path.cmp(&b.abs_path))
    });
}

/// 把绝对路径转成相对 `cwd` 的展示路径。目录会加尾斜杠。
fn display_relative(abs: &Path, cwd: &Path, is_dir: bool) -> String {
    let rel = abs.strip_prefix(cwd).unwrap_or(abs);
    let mut s = rel.to_string_lossy().replace('\\', "/");
    if is_dir && !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// 构造 `find` 工具定义。
pub fn file_find_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            // --- 1. 解析 paths ------------------------------------------------
            let paths_arr = input
                .get("paths")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    crate::error::ToolError::other("paths is required (array of globs)")
                })?;
            if paths_arr.is_empty() {
                return Err(crate::error::ToolError::other("paths must not be empty"));
            }
            let raw_patterns: Vec<String> = paths_arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if raw_patterns.is_empty() {
                return Err(crate::error::ToolError::other(
                    "paths must contain at least one string",
                ));
            }

            // --- 2. 解析其它参数 ----------------------------------------------
            let include_hidden = input
                .get("hidden")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let use_gitignore = input
                .get("gitignore")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            let requested_limit = input
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_LIMIT);
            if requested_limit == 0 {
                return Err(crate::error::ToolError::other("limit must be > 0"));
            }
            let effective_limit = requested_limit.clamp(1, MAX_LIMIT);

            let requested_timeout = input
                .get("timeout")
                .and_then(|v| v.as_f64())
                .unwrap_or(DEFAULT_TIMEOUT_SECS);
            let timeout_secs = requested_timeout.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS);
            let timeout = Duration::from_secs_f64(timeout_secs);

            // --- 3. 解析 cwd（context 优先，fallback 到当前进程 cwd）---------
            let cwd: PathBuf = ctx
                .metadata
                .as_ref()
                .and_then(|m| m.get("cwd").and_then(|v| v.as_str()))
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                });

            // --- 4. 解析 + 分流（missing / valid） ---------------------------
            let is_single = raw_patterns.len() == 1;
            let mut targets: Vec<FindTarget> = Vec::new();
            let mut missing_paths: Vec<String> = Vec::new();
            for raw in &raw_patterns {
                let t = parse_find_pattern(raw);
                let resolved = if t.search_path.is_absolute() {
                    t.search_path.clone()
                } else {
                    cwd.join(&t.search_path)
                };
                if !resolved.exists() {
                    if is_single {
                        return Err(crate::error::ToolError::other(format!(
                            "Path not found: {}",
                            raw
                        )));
                    }
                    missing_paths.push(raw.clone());
                    continue;
                }
                targets.push(t);
            }
            if targets.is_empty() {
                return Err(crate::error::ToolError::other(format!(
                    "All paths are missing: {}",
                    missing_paths.join(", ")
                )));
            }

            // --- 5. 并行扫描所有 target --------------------------------------
            // 整体包一层 timeout。`ignore::Walk` 是阻塞迭代器，所以放在
            // `spawn_blocking` 里跑，再套 `tokio::time::timeout`。
            let cwd_for_blocking = cwd.clone();
            let scan = tokio::task::spawn_blocking(
                move || -> Result<(Vec<FindHit>, bool), String> {
                    let mut all: Vec<FindHit> = Vec::new();
                    let mut saw_dir_root = false;
                    for t in &targets {
                        let resolved = if t.search_path.is_absolute() {
                            t.search_path.clone()
                        } else {
                            cwd_for_blocking.join(&t.search_path)
                        };
                        let meta = match std::fs::metadata(&resolved) {
                            Ok(m) => m,
                            Err(e) => {
                                return Err(format!("stat '{}': {}", resolved.display(), e));
                            }
                        };
                        if !t.has_glob {
                            // 没 glob：直接 stat。文件就一条，目录就递归。
                            if meta.is_file() {
                                let mtime_ms = meta
                                    .modified()
                                    .ok()
                                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                    .map(|d| d.as_millis() as i64)
                                    .unwrap_or(0);
                                all.push(FindHit {
                                    abs_path: resolved,
                                    mtime_ms,
                                    is_dir: false,
                                });
                            } else if meta.is_dir() {
                                saw_dir_root = true;
                                match walk_target(&resolved, "**/*", include_hidden, use_gitignore) {
                                    Ok(mut hits) => all.append(&mut hits),
                                    Err(e) => return Err(e.to_string()),
                                }
                            }
                            continue;
                        }
                        // 有 glob：文件就一条，目录就按 glob 走。
                        if meta.is_file() {
                            let mtime_ms = meta
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0);
                            all.push(FindHit {
                                abs_path: resolved,
                                mtime_ms,
                                is_dir: false,
                            });
                        } else if meta.is_dir() {
                            saw_dir_root = true;
                            match walk_target(
                                &resolved,
                                &t.glob_pattern,
                                include_hidden,
                                use_gitignore,
                            ) {
                                Ok(mut hits) => all.append(&mut hits),
                                Err(e) => return Err(e.to_string()),
                            }
                        }
                        // 既不是文件也不是目录（如符号链接被 follow_links(false) 跳过）→ 跳过
                    }
                    Ok((all, saw_dir_root))
                },
            );

            let scan_result = tokio::time::timeout(timeout, scan)
                .await
                .map_err(|_| crate::error::ToolError::timeout("find", timeout))?;

            let (mut hits, saw_dir_root) = match scan_result {
                Ok(Ok(pair)) => pair,
                Ok(Err(msg)) => return Err(crate::error::ToolError::other(msg)),
                Err(join) => {
                    return Err(crate::error::ToolError::execution_str(
                        "find",
                        format!("scan task panicked: {}", join),
                    ));
                }
            };

            // --- 6. 排序 + 去重 + 截断 ---------------------------------------
            hits.sort_by_key(|h| h.abs_path.clone());
            hits.dedup_by_key(|h| h.abs_path.clone());
            sort_hits(&mut hits);

            let total = hits.len();
            let truncated_by_limit = total > effective_limit as usize;
            if truncated_by_limit {
                hits.truncate(effective_limit as usize);
            }

            // --- 7. 组装结果 -------------------------------------------------
            let files: Vec<String> = hits
                .iter()
                .map(|h| display_relative(&h.abs_path, &cwd, h.is_dir))
                .collect();
            let file_count = files.len();

            // scopePath 来自第一个 target 的 search_path。
            let first = parse_find_pattern(&raw_patterns[0]);
            let first_resolved = if first.search_path.is_absolute() {
                first.search_path
            } else {
                cwd.join(&first.search_path)
            };
            let scope_path = display_relative(&first_resolved, &cwd, false);

            let mut out = json!({
                "scopePath": scope_path,
                "fileCount": file_count,
                "files": files,
                "truncated": truncated_by_limit,
                "missingPaths": missing_paths,
            });
            if saw_dir_root {
                out["recursiveFromDirectory"] = json!(true);
            }
            Ok(out)
        }
        .boxed()
    };

    Tool::builder(
        "find",
        "按 glob 模式在目录树下查找文件",
        required(vec![(
            "paths",
            PropertyType::String,
            "glob 模式数组，元素可以是文件路径、目录路径或带 `*` `?` `**` 的 glob",
        )]),
        Arc::new(handler),
    )
    .concurrency_safe(true)
    .timeout(Duration::from_secs(60))
    .build()
}

/// 把 `find` 注册到 `FileToolsPackage`。
pub fn add_file_find(pkg: &mut ToolPackage) {
    pkg.tools.push(file_find_tool());
}

// =============================================================================
// 单测
// =============================================================================
//
// 测试策略：用 `tempfile::tempdir` 构造一个真实的小型文件系统，
// 跑 find tool，断言文件列表、截断行为、gitignore 行为等。
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 建一棵测试用的小文件树。
    ///
    /// 结构（相对于 `root`）：
    /// ```text
    /// root/
    ///   README.md
    ///   src/
    ///     main.rs
    ///     lib.rs
    ///     nested/
    ///       deep.rs
    ///   tests/
    ///     test_main.rs
    ///   .hidden/
    ///     secret.md
    ///   target/
    ///     build.o           ← .gitignore 会忽略
    ///   .gitignore          ← 内容：target/
    /// ```
    fn build_tree() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("README.md"), "readme").unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main(){}").unwrap();
        fs::write(root.join("src").join("lib.rs"), "// lib").unwrap();
        fs::create_dir(root.join("src").join("nested")).unwrap();
        fs::write(root.join("src").join("nested").join("deep.rs"), "// deep").unwrap();
        fs::create_dir(root.join("tests")).unwrap();
        fs::write(root.join("tests").join("test_main.rs"), "// test").unwrap();
        fs::create_dir(root.join(".hidden")).unwrap();
        fs::write(root.join(".hidden").join("secret.md"), "shh").unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target").join("build.o"), "binary").unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        dir
    }

    /// 跑 find tool，cwd 直接用 tempdir 的路径，metadata 注入。
    async fn run_in(cwd: PathBuf, input: Value) -> Result<Value, crate::error::ToolError> {
        let tool = file_find_tool();
        let mut ctx = ToolExecutionContext::fresh("find", 0);
        ctx.metadata = Some(json!({ "cwd": cwd.to_string_lossy() }));
        (tool.handler)(input, ctx).await
    }

    /// 测试：`*.md` 应同时匹配根目录的 `README.md` 和 `.hidden/secret.md`（默认 hidden=true）。
    /// 验证：自动加 `**/` 前缀生效。
    #[tokio::test]
    async fn find_matches_nested_files_with_auto_prefix() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["*.md"]
            }),
        )
        .await
        .unwrap();
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(files.contains(&"README.md".to_string()), "files: {:?}", files);
        assert!(
            files.contains(&".hidden/secret.md".to_string()),
            "files: {:?}",
            files
        );
    }

    /// 测试：`src/**/*.rs` 应只匹配 src 树下的 .rs 文件。
    #[tokio::test]
    async fn find_respects_subdirectory_scope() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["src/**/*.rs"]
            }),
        )
        .await
        .unwrap();
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(files.len(), 3, "files: {:?}", files);
        assert!(files.contains(&"src/main.rs".to_string()));
        assert!(files.contains(&"src/lib.rs".to_string()));
        assert!(files.contains(&"src/nested/deep.rs".to_string()));
    }

    /// 测试：gitignore=true 时，target/ 下的 build.o 应被忽略。
    #[tokio::test]
    async fn find_respects_gitignore() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["**/*.o"]
            }),
        )
        .await
        .unwrap();
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            files.is_empty(),
            "target/* 应被 .gitignore 排除, got: {:?}",
            files
        );
    }

    /// 测试：gitignore=false 时，target/ 下的 build.o 不会被忽略。
    #[tokio::test]
    async fn find_ignores_gitignore_when_disabled() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["**/*.o"],
                "gitignore": false
            }),
        )
        .await
        .unwrap();
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            files.contains(&"target/build.o".to_string()),
            "files: {:?}",
            files
        );
    }

    /// 测试：hidden=false 时 .hidden/secret.md 不应出现。
    #[tokio::test]
    async fn find_honors_hidden_flag() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["**/*.md"],
                "hidden": false
            }),
        )
        .await
        .unwrap();
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(files.contains(&"README.md".to_string()));
        assert!(
            !files.iter().any(|f| f.contains("secret")),
            "files: {:?}",
            files
        );
    }

    /// 测试：limit 触发截断。
    #[tokio::test]
    async fn find_truncates_by_limit() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["**/*.rs"],
                "limit": 1
            }),
        )
        .await
        .unwrap();
        assert_eq!(out["truncated"], true);
        assert_eq!(out["fileCount"], 1);
    }

    /// 测试：单条 path 缺失 → 直接报错。
    #[tokio::test]
    async fn find_single_missing_path_errors() {
        let dir = build_tree();
        let err = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["nope/does_not_exist/**"]
            }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Path not found"));
    }

    /// 测试：多条 path 中一条缺失 → 跳过缺失项，其它正常返回。
    #[tokio::test]
    async fn find_multi_path_skips_missing_individually() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["nope/**", "src/**/*.rs"]
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
        assert_eq!(missing, vec!["nope/**".to_string()]);
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(files.len(), 3);
    }

    /// 测试：paths 是字面文件路径（无 glob）→ 直接返回该文件。
    #[tokio::test]
    async fn find_literal_file_path() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["src/main.rs"]
            }),
        )
        .await
        .unwrap();
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(files, vec!["src/main.rs".to_string()]);
    }

    /// 测试：path 是字面目录（无 glob）→ 递归列出该目录所有文件。
    #[tokio::test]
    async fn find_literal_directory_recurses() {
        let dir = build_tree();
        let out = run_in(
            dir.path().to_path_buf(),
            json!({
                "paths": ["src"],
                "gitignore": false
            }),
        )
        .await
        .unwrap();
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(files.len(), 3, "files: {:?}", files);
        assert_eq!(out["recursiveFromDirectory"], true);
    }

    /// 测试：mtime 排序：后写入的文件排在前面。
    #[tokio::test]
    async fn find_sorts_by_mtime_descending() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("older.txt"), "older").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        fs::write(root.join("newer.txt"), "newer").unwrap();
        let out = run_in(
            root.to_path_buf(),
            json!({
                "paths": ["*.txt"]
            }),
        )
        .await
        .unwrap();
        let files: Vec<String> = out["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            files,
            vec!["newer.txt".to_string(), "older.txt".to_string()]
        );
    }
}
