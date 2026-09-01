//! File system tool package. Mirrors the TS `FileToolsPackage`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};
use crate::error::ToolError;

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

#[allow(dead_code)]
fn optional(props: Vec<(&str, PropertyType, &str)>) -> ToolInputSchema {
    let mut p = BTreeMap::new();
    for (name, ty, desc) in props {
        p.insert(name.to_string(), prop(ty, desc));
    }
    ToolInputSchema {
        schema_type: Default::default(),
        properties: p,
        required: None,
        additional_properties: None,
    }
}

fn resolve_tool_path(path: &str, ctx: &ToolExecutionContext) -> PathBuf {
    let path_buf = PathBuf::from(path);
    if path_buf.is_absolute() {
        return path_buf;
    }
    ctx.metadata
        .as_ref()
        .and_then(|m| m.get("cwd"))
        .and_then(|v| v.as_str())
        .map(|cwd| PathBuf::from(cwd).join(&path_buf))
        .unwrap_or(path_buf)
}

/// 从执行上下文中取 cwd（调用方通过 metadata["cwd"] 提供）。
fn ctx_cwd<'a>(ctx: &'a ToolExecutionContext) -> Option<&'a str> {
    ctx.metadata
        .as_ref()
        .and_then(|m| m.get("cwd"))
        .and_then(|v| v.as_str())
}

/// 构造带回显路径与 cwd 的 stat 错误。
/// 模型幻觉路径（如拼错的绝对路径）时，仅凭 "No such file or directory"
/// 无法自我纠正；回显实际解析的路径和 cwd 让模型一眼看出错在哪。
fn stat_error(tool: &str, path_buf: &Path, ctx: &ToolExecutionContext, e: &std::io::Error) -> ToolError {
    let msg = match ctx_cwd(ctx) {
        Some(cwd) => format!("stat: {} | path='{}' (cwd: '{}')", e, path_buf.display(), cwd),
        None => format!("stat: {} | path='{}'", e, path_buf.display()),
    };
    ToolError::execution_str(tool, msg)
}

/// 解析 path 中的行范围选择器。
/// 格式：`:N-M`、`:raw`、`:N`、`:N+count`。
fn parse_path_selector(path: &str) -> (&str, Option<&str>) {
    if let Some(pos) = path.rfind(':') {
        let after_colon = &path[pos + 1..];
        if after_colon.starts_with('\\') {
            return (path, None);
        }
        let before = &path[..pos];
        if before.is_empty() {
            return (path, None);
        }
        if after_colon == "raw"
            || after_colon == "conflicts"
            || after_colon.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '+' || c == ',')
        {
            return (before, Some(after_colon));
        }
    }
    (path, None)
}

/// 解析行范围选择器，返回 (start, end) 1-indexed inclusive。
fn parse_line_range(sel: &str) -> Result<(usize, usize), ToolError> {
    if let Some(plus_pos) = sel.find('+') {
        let start: usize = sel[..plus_pos].parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
        let count: usize = sel[plus_pos + 1..].parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
        if start < 1 { return Err(ToolError::other("start line must be >= 1")); }
        if count < 1 { return Err(ToolError::other("count must be >= 1")); }
        return Ok((start, start + count - 1));
    }
    if let Some(dash_pos) = sel.find('-') {
        let start: usize = sel[..dash_pos].parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
        let end: usize = sel[dash_pos + 1..].parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
        if start < 1 || end < 1 { return Err(ToolError::other("line numbers must be >= 1")); }
        if end < start { return Err(ToolError::other("end line must be >= start line")); }
        return Ok((start, end));
    }
    let line: usize = sel.parse().map_err(|_| ToolError::other(format!("invalid selector: {}", sel)))?;
    if line < 1 { return Err(ToolError::other("line number must be >= 1")); }
    Ok((line, line))
}

/// 列出目录内容。
async fn list_directory(dir: &std::path::Path, path_str: &str) -> Result<Value, ToolError> {
    let mut entries: Vec<String> = Vec::new();
    let mut rd = fs::read_dir(dir).await.map_err(|e| ToolError::execution_str("read", e.to_string()))?;
    while let Some(entry) = rd.next_entry().await.map_err(|e| ToolError::execution_str("read", e.to_string()))? {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().await.map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir { entries.push(format!("{}/", name)); } else { entries.push(name); }
    }
    entries.sort();
    Ok(json!({"path": path_str, "isDirectory": true, "entries": entries, "entryCount": entries.len()}))
}

/// 把内容截断到 max_size（字符边界对齐）。返回 (内容, 是否截断)。
/// maxSize 的语义是"返回内容上限"，不是"文件总大小硬校验"——
/// 模型用它控制返回量，大文件应截断返回而不是报错（硬报错曾导致
/// agent 对大文件反复调小 maxSize 连续失败）。
fn truncate_to_max(content: &str, max_size: u64) -> (String, bool) {
    if content.len() as u64 <= max_size {
        return (content.to_string(), false);
    }
    let mut end = max_size as usize;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    (content[..end].to_string(), true)
}

fn truncated_note() -> &'static str {
    "内容已按 maxSize 截断；需要更多内容时用行范围选择器分段读取（path:start-end 或 path:start+count）"
}

/// 读取文件，支持选择器。
// ─── 代码结构摘要（structural summary）────────────────────────────
//
// 读代码文件、且没带行范围选择器时，默认不回传整文件，而是回传一份
// 「只有顶层声明的签名 + 行号」的摘要，函数体折叠掉，末尾附一行 footer
// 告诉模型「要看实现就重读这些行范围」。这样模型习惯性地 read 一个源文件
// 就自动省 token —— 不需要它主动想起去用 code_graph 之类的工具。抄的是
// oh-my-pi 的 read「结构化默认」思路（见 read-summary.ts / read-format.ts）。

/// 扩展名 → ast-grep 语言名（与 ast.rs 保持一致的子集）。空串表示不支持。
fn read_summary_lang(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        _ => "",
    }
}

/// 需要折叠正文的「定义类」顶层节点类型（各语言 tree-sitter kind）。
/// 命中这些的节点 → 只留签名行、折叠 body；其它顶层节点（宏、import、
/// 全局变量声明等）原样保留，因为它们本来就短。
fn is_definition_kind(kind: &str) -> bool {
    matches!(
        kind,
        // C / C++
        "function_definition"
        | "struct_specifier"
        | "class_specifier"
        // Rust
        | "function_item"
        | "struct_item"
        | "enum_item"
        | "trait_item"
        | "impl_item"
        // Go
        | "function_declaration"
        | "method_declaration"
        | "type_declaration"
        // Python
        | "function_definition_python" // 占位，实际 python 用 function_definition
        | "class_definition"
        // TS / JS
        | "class_declaration"
        | "interface_declaration"
        | "method_definition"
        // Java
        // (method_declaration / class_declaration 已在上面)
    )
}

/// 一段被折叠的行范围（1-based，闭区间）。
struct ElidedRange {
    start: usize,
    end: usize,
}

/// 把源码折叠成结构摘要。返回 `None` 表示不适合摘要（语言不支持、解析
/// 不出定义、或没有任何可折叠的正文——那种情况回传整文件更有用）。
///
/// 返回 `(summary_text, elided_ranges, kept_symbols)`：
/// - `summary_text`：折叠后的文本，定义体用 `{ … Nln }` 占位，带真实行号。
/// - `elided_ranges`：被折叠的行范围，用于生成 footer。
/// - `kept_symbols`：折叠掉的定义数量（统计用）。
fn summarize_code(content: &str, ext: &str) -> Option<(String, Vec<ElidedRange>, usize)> {
    use ast_grep_language::LanguageExt;

    let lang_str = read_summary_lang(ext);
    if lang_str.is_empty() {
        return None;
    }
    let lang: ast_grep_language::SupportLang = lang_str.parse().ok()?;
    let grep = lang.ast_grep(content);
    let root = grep.root();

    // 收集所有「定义类」节点的行范围（1-based 闭区间）。用 find_all 按 kind
    // 逐类找，避免依赖具体遍历 API；对每个定义节点，折叠「签名行之后到节点
    // 结束」的部分。
    let mut fold_spans: Vec<(usize, usize)> = Vec::new(); // (body_start_line, node_end_line)
    // 各语言要折叠的 kind 列表。
    let kinds: &[&str] = match lang_str {
        "c" => &["function_definition", "struct_specifier"],
        "cpp" => &["function_definition", "struct_specifier", "class_specifier"],
        "rust" => &["function_item", "struct_item", "enum_item", "trait_item", "impl_item"],
        "go" => &["function_declaration", "method_declaration", "type_declaration"],
        "python" => &["function_definition", "class_definition"],
        "typescript" => &["function_declaration", "method_definition", "class_declaration", "interface_declaration"],
        "javascript" => &["function_declaration", "method_definition", "class_declaration"],
        "java" => &["method_declaration", "class_declaration", "interface_declaration"],
        _ => &[],
    };
    let _ = is_definition_kind; // kind 判定内联在 kinds 列表里，保留函数供将来用

    for k in kinds {
        // ast-grep pattern：用 `kind:` 需要 rule API；这里用 find_all 配裸 pattern
        // 不方便，改走 root 的 kind 遍历。ast_grep_core 的 Node 支持按 kind 匹配，
        // 但简单起见用 pattern 匹配「该 kind 的任意节点」。回退：用 tree 遍历。
        // 这里用 find_all + 一个能匹配该 kind 的最宽 pattern 不可靠，改用节点遍历。
        collect_kind_ranges(&root, k, &mut fold_spans);
    }

    if fold_spans.is_empty() {
        return None;
    }

    // 折叠：把落在任一 body span 内的行替换成占位。为简单和稳健，采用
    // 「行级折叠」——签名行（span 的起始行）保留，body（起始行+1 .. 结束行）
    // 折叠成一行 `{ … N ln elided }`。
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    // 标记要折叠的行：body 内部行。key=行号(1-based) → 属于哪个 span 的结束行。
    // 先按起始行去重排序。
    fold_spans.sort_by_key(|(s, _)| *s);
    fold_spans.dedup();

    let mut elided: Vec<ElidedRange> = Vec::new();
    // 折叠区间：对每个 span，折叠 (sig_end_line+1 ..= node_end_line)。这里
    // span.0 是「定义节点起始行」，我们把「起始行的下一行 到 结束行」折叠。
    // 用一个 bool 数组标记折叠行。
    let mut folded = vec![false; total + 2];
    for (start_line, end_line) in &fold_spans {
        let s = *start_line;
        let e = (*end_line).min(total);
        if e > s {
            for ln in (s + 1)..=e {
                folded[ln] = true;
            }
            elided.push(ElidedRange { start: s + 1, end: e });
        }
    }

    // 生成摘要文本：非折叠行原样带行号；连续折叠行压成一行占位。
    let mut out = String::new();
    let mut ln = 1usize;
    while ln <= total {
        if folded[ln] {
            // 找连续折叠段。
            let seg_start = ln;
            while ln <= total && folded[ln] {
                ln += 1;
            }
            let seg_end = ln - 1;
            out.push_str(&format!("    … {} ln elided ({}-{})\n", seg_end - seg_start + 1, seg_start, seg_end));
        } else {
            out.push_str(&format!("{}: {}\n", ln, lines[ln - 1]));
            ln += 1;
        }
    }

    Some((out, elided, fold_spans.len()))
}

/// 遍历 AST，收集指定 kind 节点的 (起始行, 结束行)（1-based）。
/// 泛型化 over Doc，避免拼写具体 StrDoc<...> 类型（会触发隐藏生命周期错误）。
fn collect_kind_ranges<D: ast_grep_core::Doc>(
    node: &ast_grep_core::Node<'_, D>,
    kind: &str,
    out: &mut Vec<(usize, usize)>,
) {
    if node.kind() == kind {
        let sl = node.start_pos().byte_point().0 + 1;
        let el = node.end_pos().byte_point().0 + 1;
        out.push((sl, el));
        // 不再递归进这个定义体（避免嵌套函数被重复折叠——外层已覆盖）。
        return;
    }
    for child in node.children() {
        collect_kind_ranges(&child, kind, out);
    }
}

/// Markdown/文本文档大纲折叠（doc 侧的结构摘要）。
///
/// 把文档折叠成「只有标题（ATX `#`..`######`）+ 行号」的大纲，标题之间的
/// 正文折叠成 `… N ln elided (start-end)` 占位，footer 指引重读。这样
/// `read` 一篇长文档就自动拿到目录/大纲，而不必整篇灌进上下文——文档侧
/// 对齐代码侧的「结构化默认」。
///
/// 返回 `None` 表示不适合大纲（没有任何标题、或折叠不出可观行数——那种
/// 情况整篇回传更有用）。返回 `(outline_text, elided_ranges, heading_count)`。
fn summarize_markdown(content: &str) -> Option<(String, Vec<ElidedRange>, usize)> {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    // 找出所有 ATX 标题行（1-based 行号）。跳过围栏代码块内的 `#`。
    let mut heading_lines: Vec<usize> = Vec::new();
    let mut in_fence = false;
    let mut fence_marker = "";
    for (i, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        // 代码围栏 ``` 或 ~~~ 切换。
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = &trimmed[..3];
            if !in_fence {
                in_fence = true;
                fence_marker = marker;
            } else if trimmed.starts_with(fence_marker) {
                in_fence = false;
            }
            continue;
        }
        if in_fence {
            continue;
        }
        // ATX 标题：1-6 个 # 后跟空格。
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            heading_lines.push(i + 1);
        }
    }

    if heading_lines.is_empty() {
        return None; // 没标题，不折叠
    }

    // 折叠每个标题**之后**到下一个标题**之前**的正文。
    // heading 行本身保留；(heading+1 .. next_heading-1) 折叠。
    // 第一个标题之前的前言（frontmatter/导语）也折叠。
    let mut folded = vec![false; total + 2];
    let mut elided: Vec<ElidedRange> = Vec::new();

    // 前言：1 .. first_heading-1
    let first = heading_lines[0];
    if first > 1 {
        for ln in 1..first {
            folded[ln] = true;
        }
        elided.push(ElidedRange { start: 1, end: first - 1 });
    }
    // 各标题之间的正文。
    for (idx, &h) in heading_lines.iter().enumerate() {
        let next = heading_lines.get(idx + 1).copied().unwrap_or(total + 1);
        let body_start = h + 1;
        let body_end = next.saturating_sub(1);
        if body_end >= body_start {
            for ln in body_start..=body_end.min(total) {
                folded[ln] = true;
            }
            elided.push(ElidedRange { start: body_start, end: body_end.min(total) });
        }
    }

    // 生成大纲文本：标题行原样带行号，折叠段压成占位。
    let mut out = String::new();
    let mut ln = 1usize;
    while ln <= total {
        if folded[ln] {
            let seg_start = ln;
            while ln <= total && folded[ln] {
                ln += 1;
            }
            let seg_end = ln - 1;
            out.push_str(&format!("    … {} ln elided ({}-{})\n", seg_end - seg_start + 1, seg_start, seg_end));
        } else {
            out.push_str(&format!("{}: {}\n", ln, lines[ln - 1]));
            ln += 1;
        }
    }

    Some((out, elided, heading_lines.len()))
}

/// 生成 footer：告诉模型被折叠了多少、怎么把某段读回来。
/// `unit` 描述折叠单位（代码=「定义体」，文档=「小节」）。
fn summary_footer(read_path: &str, elided: &[ElidedRange], unit: &str) -> String {
    if elided.is_empty() {
        return String::new();
    }
    let total_elided: usize = elided.iter().map(|r| r.end - r.start + 1).sum();
    // 取前两段做示例，演示多段选择器语法。
    let sample: Vec<String> = elided
        .iter()
        .take(2)
        .map(|r| format!("{}-{}", r.start, r.end))
        .collect();
    format!(
        "\n[结构摘要：已折叠 {} 处{}、共 {} 行。要看某段，用行范围重读，例如 `{}:{}`。整文件原文用 `{}:raw`。]",
        elided.len(),
        unit,
        total_elided,
        read_path,
        sample.join(","),
        read_path
    )
}

async fn read_file_sel(path_buf: &std::path::Path, selector: Option<&str>, max_size: u64) -> Result<Value, ToolError> {
    let meta = fs::metadata(path_buf).await.map_err(|e| ToolError::execution_str("read", format!("stat: {}", e)))?;
    if !meta.is_file() { return Err(ToolError::other(format!("Not a file: {}", path_buf.display()))); }
    // 运行时内部目录硬禁读：会话日志 / workflow checkpoint 是系统
    // 自产物，agent 读它们只会自污染上下文（实际事故：agent 反复读
    // 自己会话的实时日志，越读越大，14 连失败）。
    {
        let parts: Vec<String> = path_buf
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let blocked = parts.windows(2).any(|w| {
            w[0] == ".latte" && (w[1] == "ui-sessions" || w[1] == "workflow-runs")
        });
        if blocked {
            return Err(ToolError::other(format!(
                "系统运行时内部文件（{}），对 agent 不可读；如需排查运行状态请告知用户",
                path_buf.display()
            )));
        }
    }
    let bytes = fs::read(path_buf).await.map_err(|e| ToolError::execution_str("read", format!("read: {}", e)))?;
    let total_bytes = bytes.len() as u64;
    let content = String::from_utf8_lossy(&bytes).to_string();
    let total_lines = content.lines().count();
    let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0);
    // 内容快照 tag（全文，非选区）：把它带回 `edit` 就能让工具校验
    // 「文件自本次 read 之后有没有被改过」，从而拒绝基于过期行号的编辑。
    // 见 `crate::tools::edit` 顶部关于 hashline 移植的说明。
    let tag = crate::tools::edit::content_tag(&content);

    if let Some(sel) = selector {
        if sel == "raw" {
            let (content, truncated) = truncate_to_max(&content, max_size);
            return Ok(json!({"content": content, "path": path_buf.to_string_lossy(), "size": total_bytes, "totalLines": total_lines, "truncated": truncated, "note": if truncated { truncated_note() } else { "" }, "encoding": "utf-8", "modifiedAt": modified, "selector": "raw", "tag": tag}));
        }
        let (start_line, end_line) = parse_line_range(sel)?;
        if start_line > total_lines { return Err(ToolError::other(format!("start_line {} exceeds file length {}", start_line, total_lines))); }
        let end = end_line.min(total_lines);
        let selected: Vec<&str> = content.lines().skip(start_line - 1).take(end - start_line + 1).collect();
        let (selected_content, truncated) = truncate_to_max(&selected.join("\n"), max_size);
        let numbered: Vec<String> = selected_content.lines().enumerate().map(|(i, l)| format!("{}:{}", start_line + i, l)).collect();
        return Ok(json!({"content": selected_content, "numberedContent": numbered.join("\n"), "path": path_buf.to_string_lossy(), "size": total_bytes, "totalLines": total_lines, "startLine": start_line, "endLine": end, "selectedLines": selected.len(), "truncated": truncated, "note": if truncated { truncated_note() } else { "" }, "encoding": "utf-8", "modifiedAt": modified, "selector": sel, "tag": tag}));
    }
    // 无选择器：读**代码**文件 → code-graph 结构摘要（签名+行号，折叠函数体）；
    // 读**文档**（.md/.markdown/.mdx/.txt）→ doc 大纲折叠（标题+行号，折叠正文）。
    // 让习惯性 read 就自动省 token，而不必依赖模型主动改用别的工具。
    // 只在文件够大（>80 行）且确实折叠出可观行数时才摘要；否则回传整文件。
    // 需要整文件用 `path:raw`，需要某段用 `path:start-end`。
    // 环境变量 LATTE_READ_SUMMARY=0 可全局关闭。
    let summary_enabled = std::env::var("LATTE_READ_SUMMARY")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    if summary_enabled && total_lines > 80 {
        let ext = path_buf
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let rel = path_buf.to_string_lossy().to_string();

        // ── 代码文件：code-graph 折叠 ──
        if !read_summary_lang(&ext).is_empty() {
            if let Some((summary, elided, folded)) = summarize_code(&content, &ext) {
                let elided_lines: usize = elided.iter().map(|r| r.end - r.start + 1).sum();
                if elided_lines >= 20 {
                    let footer = summary_footer(&rel, &elided, "定义体");
                    return Ok(json!({
                        "content": format!("{summary}{footer}"),
                        "path": rel,
                        "size": total_bytes,
                        "totalLines": total_lines,
                        "encoding": "utf-8",
                        "modifiedAt": modified,
                        "mode": "structural_summary",
                        "foldedDefinitions": folded,
                        "elidedLines": elided_lines,
                        "tag": tag
                    }));
                }
            }
        }
        // ── 文档：doc 大纲折叠 ──
        else if matches!(ext.as_str(), "md" | "markdown" | "mdx" | "txt") {
            if let Some((outline, elided, headings)) = summarize_markdown(&content) {
                let elided_lines: usize = elided.iter().map(|r| r.end - r.start + 1).sum();
                if elided_lines >= 20 {
                    let footer = summary_footer(&rel, &elided, "小节");
                    return Ok(json!({
                        "content": format!("{outline}{footer}"),
                        "path": rel,
                        "size": total_bytes,
                        "totalLines": total_lines,
                        "encoding": "utf-8",
                        "modifiedAt": modified,
                        "mode": "doc_outline",
                        "headings": headings,
                        "elidedLines": elided_lines,
                        "tag": tag
                    }));
                }
            }
        }
    }

    let (content, truncated) = truncate_to_max(&content, max_size);
    Ok(json!({"content": content, "path": path_buf.to_string_lossy(), "size": total_bytes, "totalLines": total_lines, "truncated": truncated, "note": if truncated { truncated_note() } else { "" }, "encoding": "utf-8", "modifiedAt": modified, "tag": tag}))
}

/// Standalone `read` tool constructor.
///
/// 单文件实现是这里的唯一职责；批量读（paths 数组）由 agent-core 的
/// `add_batch_read_contract` 适配器在本工具之上编排，schema 里不要出现
/// paths——否则适配器会判定「已自带批量」而跳过包装。
pub fn file_read_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            let max_size = input.get("maxSize").and_then(|v| v.as_u64()).unwrap_or(10 * 1024 * 1024);
            let path = input.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::other("path is required")
            })?;
            let (file_path, selector) = parse_path_selector(path);
            let path_buf = resolve_tool_path(file_path, &ctx);
            let meta = fs::metadata(&path_buf)
                .await
                .map_err(|e| stat_error("read", &path_buf, &ctx, &e))?;
            if meta.is_dir() {
                return list_directory(&path_buf, path).await;
            }
            read_file_sel(&path_buf, selector, max_size).await
        }.boxed()
    };
    let mut props = BTreeMap::new();
    props.insert(
        "path".to_string(),
        prop(
            PropertyType::String,
            "单个文件路径。选择器：:N-M 行范围 / :N+count / :raw 整文件原文（跳过结构摘要）",
        ),
    );
    props.insert(
        "maxSize".to_string(),
        prop(
            PropertyType::Number,
            "返回内容上限（字节），默认 10MB（10485760），超出截断返回（truncated=true），不报错。",
        ),
    );
    let schema = ToolInputSchema {
        schema_type: Default::default(),
        properties: props,
        required: Some(vec!["path".to_string()]),
        additional_properties: None,
    };
    Tool::builder("read", "读取文件内容。支持行范围选择器：path:start-end、path:start+count、path:raw；读代码文件且不带行范围时默认回传【结构摘要】：只列顶层定义的签名+行号、折叠函数体，末尾 footer 告诉你要看实现该重读哪几行；要整文件原文用 path:raw。maxSize 是返回内容上限（默认 10MB），超出截断并在 truncated/note 字段说明，不会报错。也支持读取目录列表。注意：.latte/ 是系统运行时内部目录（会话日志、workflow checkpoint、任务看板数据），不要读取，除非用户明确要求。", schema, std::sync::Arc::new(handler))
        .concurrency_safe(true)
        .strict(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
}
fn file_write_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("path is required"))?;
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("content is required"))?;
            let overwrite = input
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let create_dirs = input
                .get("createDirs")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            let path_buf = PathBuf::from(path);
            if !overwrite && path_buf.exists() {
                return Err(crate::error::ToolError::other(format!(
                    "File exists and overwrite=false: {}",
                    path
                )));
            }
            if create_dirs {
                if let Some(parent) = path_buf.parent() {
                    if !parent.as_os_str().is_empty() {
                        fs::create_dir_all(parent).await.map_err(|e| {
                            crate::error::ToolError::execution_str(
                                "write",
                                format!("create_dir_all: {}", e),
                            )
                        })?;
                    }
                }
            }
            let created = !path_buf.exists();
            let mut file = fs::File::create(&path_buf).await.map_err(|e| {
                crate::error::ToolError::execution_str("write", format!("create: {}", e))
            })?;
            file.write_all(content.as_bytes()).await.map_err(|e| {
                crate::error::ToolError::execution_str("write", format!("write: {}", e))
            })?;
            let _ = file.flush().await;
            Ok(json!({
                "success": true,
                "path": path,
                "bytesWritten": content.len(),
                "created": created,
            }))
        }
        .boxed()
    };
    Tool::builder(
        "write",
        "写入文件内容",
        required(vec![
            ("path", PropertyType::String, "文件路径"),
            ("content", PropertyType::String, "文件内容"),
        ]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(std::time::Duration::from_secs(10))
    .build()
}

fn file_list_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("path is required"))?;
            let recursive = input
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let include_hidden = input
                .get("includeHidden")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let root = resolve_tool_path(path, &ctx);
            if !root.is_dir() {
                return Err(crate::error::ToolError::other(format!(
                    "Not a directory: {}",
                    path
                )));
            }
            let mut entries = Vec::new();
            list_dir(&root, &root, recursive, include_hidden, &mut entries).await?;

            Ok(json!({
                "rootPath": path,
                "entries": entries,
                "total": entries.len(),
            }))
        }
        .boxed()
    };
    Tool::builder(
        "list",
        "列出目录内容",
        required(vec![("path", PropertyType::String, "目录路径")]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(true)
    .timeout(std::time::Duration::from_secs(30))
    .build()
}

async fn list_dir(
    dir: &Path,
    root: &Path,
    recursive: bool,
    include_hidden: bool,
    out: &mut Vec<Value>,
) -> Result<(), crate::error::ToolError> {
    let mut reader = match tokio::fs::read_dir(dir).await {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| crate::error::ToolError::execution_str("list", e.to_string()))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let entry_path = entry.path();
        let metadata = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let entry_type = if metadata.is_dir() {
            "directory"
        } else if metadata.is_symlink() {
            "symlink"
        } else {
            "file"
        };
        let extension = if entry_type == "file" {
            Path::new(&name)
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| format!(".{}", s))
        } else {
            None
        };
        let relative = entry_path
            .strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push(json!({
            "name": name,
            "path": entry_path.to_string_lossy().to_string(),
            "relativePath": relative,
            "type": entry_type,
            "size": metadata.len(),
            "extension": extension,
            "modifiedAt": modified,
        }));
        if recursive && entry_type == "directory" {
            let _ = Box::pin(list_dir(&entry_path, root, recursive, include_hidden, out)).await;
        }
    }
    Ok(())
}

fn file_delete_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| crate::error::ToolError::other("path is required"))?;
            let recursive = input
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let force = input
                .get("force")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let path_buf = PathBuf::from(path);
            let exists = path_buf.exists();
            if !exists {
                if force {
                    return Ok(json!({"success": true, "path": path, "wasDirectory": false}));
                }
                return Err(crate::error::ToolError::other(format!(
                    "Path does not exist: {}",
                    path
                )));
            }
            let was_dir = path_buf.is_dir();
            if was_dir && !recursive {
                return Err(crate::error::ToolError::other(format!(
                    "Path is a directory; pass recursive=true to delete: {}",
                    path
                )));
            }
            if was_dir {
                fs::remove_dir_all(&path_buf).await.map_err(|e| {
                    crate::error::ToolError::execution_str("delete", format!("rmdir: {}", e))
                })?;
            } else {
                fs::remove_file(&path_buf).await.map_err(|e| {
                    crate::error::ToolError::execution_str("delete", format!("rm: {}", e))
                })?;
            }
            Ok(json!({"success": true, "path": path, "wasDirectory": was_dir}))
        }
        .boxed()
    };
    Tool::builder(
        "delete",
        "删除文件或目录",
        required(vec![("path", PropertyType::String, "文件或目录路径")]),
        std::sync::Arc::new(handler),
    )
    .concurrency_safe(false)
    .timeout(std::time::Duration::from_secs(10))
    .build()
}

/// The `file` tool package. Mirrors `FileToolsPackage` in TS.
pub struct FileToolsPackage;

impl FileToolsPackage {
    /// Construct the package (5 tools: read, write, list, delete, search).
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "file".into(),
            version: Some("1.0.0".into()),
            namespace: None,
            description: Some("文件操作工具：读取、写入、列表、删除、搜索、查找".into()),
            dependencies: None,
            tools: vec![
                file_read_tool(),
                file_write_tool(),
                file_list_tool(),
                file_delete_tool(),
                crate::tools::search::file_search_tool(),
                crate::tools::find::file_find_tool(),
            ],
            on_init: None,
            on_destroy: None,
            before_execute: None,
            after_execute: None,
            metadata: Some(json!({"category": "filesystem", "tags": ["file", "io"]})),
        }
    }
}

impl Default for FileToolsPackage {
    fn default() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::create_tool_manager;
    use crate::types::ToolManager;
    use serde_json::json;
    use tempfile::TempDir;

    async fn setup_file(content: &str) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, content).await.unwrap();
        (dir, path.to_string_lossy().to_string())
    }

    async fn run_read(path: &str) -> Value {
        let m = create_tool_manager();
        m.register_package(FileToolsPackage::new()).await.unwrap();
        m.execute("read", json!({"path": path}), None).await.unwrap()
    }

    /// 单路径调用的返回形状**逐字段不变**：不包 `files`，直接是文件对象。
    /// 这条锁住向后兼容——UI 渲染、`edit_anchor`、各处消费方都依赖它。
    #[tokio::test]
    async fn single_path_read_shape_is_unchanged() {
        let (_dir, path) = setup_file("hello\n").await;
        let r = run_read(&path).await;
        assert!(r.get("files").is_none(), "单路径不得包成 files 数组");
        assert_eq!(r["content"].as_str().unwrap(), "hello\n");
        assert_eq!(r["path"].as_str().unwrap(), path);
    }


    #[tokio::test]
    async fn test_read_selector_range() {
        let (_dir, path) = setup_file("line1\nline2\nline3\nline4\nline5\n").await;
        let r = run_read(&format!("{}:2-4", path)).await;
        assert_eq!(r["startLine"].as_u64().unwrap(), 2);
        assert_eq!(r["selectedLines"].as_u64().unwrap(), 3);
        assert!(r["content"].as_str().unwrap().contains("line2"));
        assert!(!r["content"].as_str().unwrap().contains("line1"));
    }

    #[tokio::test]
    async fn test_read_selector_single_line() {
        let (_dir, path) = setup_file("a\nb\nc\n").await;
        let r = run_read(&format!("{}:2", path)).await;
        assert_eq!(r["content"].as_str().unwrap(), "b");
    }

    #[tokio::test]
    async fn test_read_selector_raw() {
        let (_dir, path) = setup_file("hello\nworld\n").await;
        let r = run_read(&format!("{}:raw", path)).await;
        assert_eq!(r["selector"].as_str().unwrap(), "raw");
        assert_eq!(r["content"].as_str().unwrap(), "hello\nworld\n");
    }

    /// 大 C 文件、无选择器 → 默认回传结构摘要：签名保留、函数体折叠、带 footer。
    #[tokio::test]
    async fn test_read_code_file_returns_structural_summary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.c");
        // 造一个 >80 行、含两个胖函数的 C 文件。
        let mut src = String::from("#include <stdio.h>\n\n");
        for f in ["alpha", "beta"] {
            src.push_str(&format!("int {f}(int a, int b) {{\n"));
            for _ in 0..50 {
                src.push_str("    a += b; /* body line */\n");
            }
            src.push_str("    return a;\n}\n\n");
        }
        fs::write(&path, &src).await.unwrap();
        let p = path.to_string_lossy().to_string();

        let r = run_read(&p).await;
        assert_eq!(r["mode"].as_str(), Some("structural_summary"), "big C file should be summarized: {r}");
        let content = r["content"].as_str().unwrap();
        // 签名行保留。
        assert!(content.contains("int alpha(int a, int b)"), "signature must be kept: {content}");
        assert!(content.contains("int beta(int a, int b)"), "signature must be kept");
        // 函数体被折叠成占位。
        assert!(content.contains("ln elided"), "bodies should be elided: {content}");
        // footer 指引重读。
        assert!(content.contains(":raw"), "footer should mention :raw recovery");
        assert!(r["foldedDefinitions"].as_u64().unwrap() >= 2);
    }

    /// `:raw` 绕过摘要，拿整文件原文。
    #[tokio::test]
    async fn test_read_code_file_raw_bypasses_summary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.c");
        let mut src = String::from("int f(void) {\n");
        for _ in 0..100 { src.push_str("    ;\n"); }
        src.push_str("}\n");
        fs::write(&path, &src).await.unwrap();
        let p = format!("{}:raw", path.to_string_lossy());
        let r = run_read(&p).await;
        assert_eq!(r["selector"].as_str(), Some("raw"));
        assert!(r["content"].as_str().unwrap().contains("int f(void)"));
        assert!(!r["content"].as_str().unwrap().contains("ln elided"));
    }

    /// 小文件不摘要（阈值以下），整文件回传。
    #[tokio::test]
    async fn test_read_small_code_file_not_summarized() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("small.c");
        fs::write(&path, "int f(void){return 1;}\n").await.unwrap();
        let r = run_read(&path.to_string_lossy()).await;
        assert_ne!(r["mode"].as_str(), Some("structural_summary"));
    }

    /// 非代码文件（.txt）无标题 → 不摘要。
    #[tokio::test]
    async fn test_read_text_file_not_summarized() {
        let mut big = String::new();
        for i in 0..200 { big.push_str(&format!("line {i}\n")); }
        let (_dir, path) = setup_file(&big).await; // test.txt, 无标题
        let r = run_read(&path).await;
        assert_ne!(r["mode"].as_str(), Some("structural_summary"));
        assert_ne!(r["mode"].as_str(), Some("doc_outline"));
    }

    /// 大 Markdown 文档、无选择器 → doc 大纲折叠：标题保留、正文折叠、带 footer。
    #[tokio::test]
    async fn test_read_markdown_returns_doc_outline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("guide.md");
        let mut md = String::from("# 项目指南\n\n导语一段。\n\n");
        for sec in ["安装", "配置", "使用"] {
            md.push_str(&format!("## {sec}\n\n"));
            for _ in 0..40 {
                md.push_str("这是正文说明行。\n");
            }
            md.push('\n');
        }
        fs::write(&path, &md).await.unwrap();
        let p = path.to_string_lossy().to_string();

        let r = run_read(&p).await;
        assert_eq!(r["mode"].as_str(), Some("doc_outline"), "big md should be outlined: {r}");
        let content = r["content"].as_str().unwrap();
        // 标题保留。
        assert!(content.contains("# 项目指南"), "top heading kept: {content}");
        assert!(content.contains("## 安装"), "section heading kept");
        assert!(content.contains("## 配置"));
        assert!(content.contains("## 使用"));
        // 正文折叠。
        assert!(content.contains("ln elided"), "body should be elided");
        // footer 用「小节」而非「定义体」。
        assert!(content.contains("小节"), "doc footer should say 小节: {content}");
        assert!(content.contains(":raw"));
        assert!(r["headings"].as_u64().unwrap() >= 4);
    }

    /// Markdown `:raw` 绕过大纲。
    #[tokio::test]
    async fn test_read_markdown_raw_bypasses_outline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("doc.md");
        let mut md = String::from("# T\n");
        for _ in 0..100 { md.push_str("body\n"); }
        fs::write(&path, &md).await.unwrap();
        let r = run_read(&format!("{}:raw", path.to_string_lossy())).await;
        assert_eq!(r["selector"].as_str(), Some("raw"));
        assert!(!r["content"].as_str().unwrap().contains("ln elided"));
    }

    /// 无标题的 Markdown（纯散文）→ 不折叠（回退整文件）。
    #[tokio::test]
    async fn test_read_markdown_without_headings_not_outlined() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("prose.md");
        let mut md = String::new();
        for i in 0..120 { md.push_str(&format!("散文第 {i} 行，没有任何标题。\n")); }
        fs::write(&path, &md).await.unwrap();
        let r = run_read(&path.to_string_lossy()).await;
        assert_ne!(r["mode"].as_str(), Some("doc_outline"));
    }

    #[tokio::test]
    async fn test_read_directory() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        fs::create_dir(dir.path().join("subdir")).await.unwrap();
        fs::write(dir.path().join("f.txt"), "hi").await.unwrap();
        let r = run_read(&p).await;
        assert!(r["isDirectory"].as_bool().unwrap());
        let entries = r["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e.as_str().unwrap() == "f.txt"));
        assert!(entries.iter().any(|e| e.as_str().unwrap() == "subdir/"));
    }

    #[tokio::test]
    async fn test_read_missing_path_error_echoes_path_and_cwd() {
        let m = create_tool_manager();
        m.register_package(FileToolsPackage::new()).await.unwrap();
        let mut ctx = crate::types::ToolExecutionContext::fresh("read", 0);
        ctx.metadata = Some(json!({"cwd": "/Users/zhouguodong/Documents/github/jemalloc"}));
        let err = m
            .execute("read", json!({"path": "/Users/zhouguong/nope.md"}), Some(ctx))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/Users/zhouguong/nope.md"), "echo path: {}", msg);
        assert!(msg.contains("cwd: '/Users/zhouguodong/Documents/github/jemalloc'"), "echo cwd: {}", msg);
    }

    #[tokio::test]
    async fn test_read_missing_path_error_without_cwd() {
        let m = create_tool_manager();
        m.register_package(FileToolsPackage::new()).await.unwrap();
        let err = m
            .execute("read", json!({"path": "/definitely/not/here.md"}), None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/definitely/not/here.md"), "echo path: {}", msg);
        assert!(!msg.contains("cwd:"), "no cwd when unset: {}", msg);
    }

    /// maxSize 是"返回内容上限"：大文件不再报 File too large，
    /// 而是截断返回并带 truncated=true + 提示 note。
    #[tokio::test]
    async fn test_read_max_size_truncates_instead_of_erroring() {
        let big = "x".repeat(5000);
        let (_dir, path) = setup_file(&big).await;
        let m = create_tool_manager();
        m.register_package(FileToolsPackage::new()).await.unwrap();
        // 全量读 + 小 maxSize → 截断，不报错
        let r = m
            .execute("read", json!({"path": path, "maxSize": 1000}), None)
            .await
            .expect("大文件 + 小 maxSize 必须成功（截断）而不是报错");
        assert_eq!(r["truncated"].as_bool(), Some(true));
        assert_eq!(r["content"].as_str().unwrap().len(), 1000);
        assert!(r["note"].as_str().unwrap().contains("截断"));
        // 行范围选择器 + 小 maxSize 同样不再被文件总大小误杀
        let r2 = m
            .execute("read", json!({"path": format!("{path}:1+1"), "maxSize": 10}), None)
            .await
            .expect("行范围读取大文件必须成功");
        assert_eq!(r2["selectedLines"].as_u64(), Some(1));
        assert_eq!(r2["content"].as_str().unwrap().len(), 10);
        assert_eq!(r2["truncated"].as_bool(), Some(true));
    }

    /// .latte/ui-sessions 与 .latte/workflow-runs 是系统运行时内部
    /// 目录，对 agent 硬禁读。
    #[tokio::test]
    async fn test_read_blocks_latte_runtime_internals() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join(".latte").join("ui-sessions");
        fs::create_dir_all(&sub).await.unwrap();
        fs::write(sub.join("s.jsonl"), "{}").await.unwrap();
        let p = sub.join("s.jsonl").to_string_lossy().to_string();
        let m = create_tool_manager();
        m.register_package(FileToolsPackage::new()).await.unwrap();
        let err = m
            .execute("read", json!({"path": p}), None)
            .await
            .expect_err("运行时内部文件必须被拒绝");
        assert!(err.to_string().contains("系统运行时内部文件"), "{err}");
    }
}
