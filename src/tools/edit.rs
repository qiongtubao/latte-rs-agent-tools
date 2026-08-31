//! File edit tool — apply precise text edits to files.
//!
//! 提供 `edit` 工具：对文件做精确的基于行号的文本替换、删除、插入。

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::fs;

use crate::error::ToolError;
use crate::types::{
    PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage,
};

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

// ---------------------------------------------------------------------------
// 内容快照 tag + sloppy 匹配（移植自 oh-my-pi 的 hashline / sloppy 思路）
//
// 背景：本工具的行号操作此前**零内容校验**——模型给的 start_line 一旦
// 因文件变动或记忆偏差漂了几行，就静默改错位置，比报错更危险。而
// old_text 只有精确匹配，缩进差一个空格就硬失败，没有任何容错。
//
// oh-my-pi 的解法分两层，这里照搬：
//   1. **哈希快照锚点**：read 返回全文内容哈希 tag，edit 带 tag 回来；
//      文件已变则 tag 不符 → 拒绝执行并要求重读，绝不基于过期快照落笔。
//   2. **宽松恢复**：小错（行号偏移、行尾空白、整体缩进差异）由工具
//      自动吸收——唯一命中就订正并附 warning；有歧义（多处候选）则
//      拒绝而不猜。
// ---------------------------------------------------------------------------

/// 归一化文件文本：剥 BOM + 统一换行为 LF。tag 基于归一化后的内容，
/// 因此换行风格差异不会造成假性 stale。
fn normalize_for_tag(content: &str) -> String {
    let no_bom = content.strip_prefix('\u{feff}').unwrap_or(content);
    no_bom.replace("\r\n", "\n").replace('\r', "\n")
}

/// 4 位大写十六进制内容哈希（FNV-1a 截断）。
///
/// 用手写 FNV 而非 `DefaultHasher`：后者的种子/算法不保证跨 Rust 版本
/// 稳定，而 tag 要在「read 返回」与「后续 edit 校验」之间跨进程可比。
pub(crate) fn content_tag(content: &str) -> String {
    let normalized = normalize_for_tag(content);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalized.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    // 折叠到 16 位再转 4 位十六进制，与 oh-my-pi 的 4-hex tag 对齐。
    let folded = ((hash >> 48) ^ (hash >> 32) ^ (hash >> 16) ^ hash) as u16;
    format!("{:04X}", folded)
}

/// 行级比较的归一化档位，供 sloppy 回退链逐级放宽。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineNorm {
    /// 逐字节相同。
    Exact,
    /// 忽略行尾空白（模型常吞掉或多加尾随空格）。
    TrailingWs,
    /// 再忽略行首缩进（模型复述代码时常改变缩进层级）。
    Indent,
}

fn norm_line(line: &str, mode: LineNorm) -> &str {
    match mode {
        LineNorm::Exact => line,
        LineNorm::TrailingWs => line.trim_end(),
        LineNorm::Indent => line.trim(),
    }
}

/// 在 `lines` 中按给定归一化档位查找 `needle`（多行块）的全部起始下标（0-based）。
fn find_block(lines: &[&str], needle: &[&str], mode: LineNorm) -> Vec<usize> {
    if needle.is_empty() || needle.len() > lines.len() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for start in 0..=(lines.len() - needle.len()) {
        let matched = needle
            .iter()
            .enumerate()
            .all(|(offset, want)| norm_line(lines[start + offset], mode) == norm_line(want, mode));
        if matched {
            hits.push(start);
        }
    }
    hits
}

/// sloppy 定位：先精确，失败后依次放宽到「忽略行尾空白」「忽略缩进」。
///
/// 返回 `(起始下标, 命中档位)`。只有**唯一命中**才算恢复成功；命中多处
/// 视为歧义并报错——宁可让模型重新给更精确的锚点，也不猜。
fn locate_block_sloppy(
    lines: &[&str],
    needle: &[&str],
    what: &str,
) -> Result<(usize, LineNorm), ToolError> {
    for mode in [LineNorm::Exact, LineNorm::TrailingWs, LineNorm::Indent] {
        let hits = find_block(lines, needle, mode);
        match hits.len() {
            0 => continue,
            1 => return Ok((hits[0], mode)),
            n => {
                return Err(ToolError::other(format!(
                    "{what} 在文件中有 {n} 处候选匹配（歧义），拒绝猜测：请扩大 \
                     上下文使其唯一，或改用带 expect 的行号操作"
                )))
            }
        }
    }
    Err(ToolError::other(format!("{what} 在文件中找不到匹配")))
}

fn edit_schema() -> ToolInputSchema {
    let mut p = BTreeMap::new();
    p.insert("path".into(), prop(PropertyType::String, "File path to edit (required)."));
    p.insert(
        "tag".into(),
        prop(
            PropertyType::String,
            "Optional 4-hex content snapshot tag from the latest `read` of this file. \
             When provided it is verified against the live file and the edit is REJECTED \
             if the file changed since that read (stale anchor). Strongly recommended for \
             line-number edits.",
        ),
    );
    p.insert(
        "ops".into(),
        ToolInputProperty {
            property_type: PropertyType::Array,
            description: Some(
                "Array of edit operations. Each op is either line-based \
                 (start_line[, end_line][, delete|insert_after|insert_before], new_content) \
                 or text-based (old_text, new_text|delete). For line-based ops also pass \
                 `expect` — the exact text you believe occupies that range: it is verified, \
                 and if your line numbers are slightly off the tool relocates the edit \
                 automatically (unique match) instead of corrupting the wrong lines. \
                 Text-based ops must match exactly one location; if `old_text` occurs \
                 multiple times the edit is rejected — widen the context to make it unique, \
                 or pass `all: true` to intentionally replace every occurrence."
                    .into(),
            ),
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
        },
    );
    ToolInputSchema {
        schema_type: Default::default(),
        properties: p,
        required: Some(vec!["path".into(), "ops".into()]),
        additional_properties: None,
    }
}

fn resolve_path(path: &str, ctx: &ToolExecutionContext) -> PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Some(meta) = &ctx.metadata {
        if let Some(cwd) = meta.get("cwd").and_then(|v| v.as_str()) {
            return std::path::Path::new(cwd).join(path);
        }
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| p.to_path_buf())
}

fn split_content(content: &str) -> (Vec<&str>, bool) {
    let ends_with_newline = content.ends_with('\n');
    let lines: Vec<&str> = content.split('\n').collect();
    let effective = if ends_with_newline {
        &lines[..lines.len() - 1]
    } else {
        &lines[..]
    };
    (effective.to_vec(), ends_with_newline)
}

fn join_lines(lines: &[&str], ends_with_newline: bool) -> String {
    let mut result = lines.join("\n");
    if ends_with_newline {
        result.push('\n');
    }
    result
}

/// 校验 `expect` 与目标行区间的实际内容是否一致；不一致则尝试在全文
/// 唯一重定位，命中就订正行号（小错自愈），否则拒绝——**绝不**按错行号落笔。
///
/// 这是本工具此前最危险的缺口：行号操作零校验，模型行号偏 2 行就静默
/// 改错地方，且返回 success。
fn verify_or_relocate(
    lines: &[&str],
    start_line: usize,
    end_line: usize,
    expect: &str,
    warnings: &mut Vec<String>,
) -> Result<(usize, usize), ToolError> {
    let expect_lines: Vec<&str> = expect.split('\n').collect();
    // 允许 expect 末尾多一个空行（模型常把块尾换行也带进来）。
    let expect_lines: Vec<&str> = if expect_lines.len() > 1 && expect_lines.last() == Some(&"") {
        expect_lines[..expect_lines.len() - 1].to_vec()
    } else {
        expect_lines
    };

    let actual: Vec<&str> = lines
        .iter()
        .skip(start_line - 1)
        .take(end_line - start_line + 1)
        .copied()
        .collect();
    let same = actual.len() == expect_lines.len()
        && actual
            .iter()
            .zip(expect_lines.iter())
            .all(|(a, b)| a.trim_end() == b.trim_end());
    if same {
        return Ok((start_line, end_line));
    }

    // 行号对不上 → 全文唯一重定位。
    let (hit, mode) = locate_block_sloppy(lines, &expect_lines, "expect 内容").map_err(|e| {
        ToolError::other(format!(
            "expect 与 {start_line}-{end_line} 行实际内容不符，且{}。实际内容为：{:?}",
            e,
            actual.join("\n").chars().take(200).collect::<String>()
        ))
    })?;
    let new_start = hit + 1;
    let new_end = hit + expect_lines.len();
    let how = match mode {
        LineNorm::Exact => "精确",
        LineNorm::TrailingWs => "忽略行尾空白",
        LineNorm::Indent => "忽略缩进",
    };
    warnings.push(format!(
        "行号自愈：expect 内容不在 {start_line}-{end_line} 行，已按{how}匹配\
         重定位到 {new_start}-{new_end} 行"
    ));
    Ok((new_start, new_end))
}

fn apply_line_op(
    content: &str,
    op: &Value,
    warnings: &mut Vec<String>,
) -> Result<String, ToolError> {
    let start_line = op
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .ok_or_else(|| ToolError::other("start_line is required"))?;
    if start_line < 1 {
        return Err(ToolError::other("start_line must be >= 1"));
    }

    let delete = op.get("delete").and_then(|v| v.as_bool()).unwrap_or(false);
    let insert_after = op.get("insert_after").and_then(|v| v.as_bool()).unwrap_or(false);
    let insert_before = op.get("insert_before").and_then(|v| v.as_bool()).unwrap_or(false);

    let (lines, ends_with_newline) = split_content(content);
    let total_lines = lines.len();

    if start_line > total_lines {
        if insert_after {
            let new_content = op
                .get("new_content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::other("new_content is required"))?;
            let result = if content.ends_with('\n') {
                format!("{}{}\n", content, new_content)
            } else {
                format!("{}\n{}\n", content, new_content)
            };
            return Ok(result);
        }
        return Err(ToolError::other(format!(
            "start_line {} exceeds file length {}",
            start_line, total_lines
        )));
    }

    // expect（可选）：模型认为目标区间的现有内容。给了就校验；不符则
    // 唯一重定位或拒绝。不给 → 保持历史行为（向后兼容），但会在返回值里
    // 提示「本次编辑未校验」。
    let expect = op.get("expect").and_then(|v| v.as_str());

    if delete {
        let end_line = op
            .get("end_line")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(start_line);
        if end_line < start_line {
            return Err(ToolError::other("end_line must be >= start_line"));
        }
        if end_line > total_lines {
            return Err(ToolError::other(format!(
                "end_line {} exceeds file length {}",
                end_line, total_lines
            )));
        }
        let (start_line, end_line) = match expect {
            Some(e) => verify_or_relocate(&lines, start_line, end_line, e, warnings)?,
            None => (start_line, end_line),
        };
        let result: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(i, _)| { let n = i + 1; n < start_line || n > end_line })
            .map(|(_, l)| *l)
            .collect();
        return Ok(join_lines(&result, ends_with_newline));
    }

    if insert_after || insert_before {
        let new_content = op
            .get("new_content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::other("new_content is required"))?;
        // 插入锚点也校验：expect 指锚点行本身（单行）。
        let start_line = match expect {
            Some(e) => verify_or_relocate(&lines, start_line, start_line, e, warnings)?.0,
            None => start_line,
        };
        // 插入内容按行切分。**不能** filter 掉空行——此前用
        // `.filter(|l| !l.is_empty())` 会把插入内容里的空行全部吃掉
        // （插一段带空行分隔的代码，落盘后被挤成连续行）。
        // 只丢弃「末尾换行产生的那一个尾随空串」，其余空行都是有意义的内容。
        let mut insert_lines: Vec<&str> = new_content.split('\n').collect();
        if insert_lines.len() > 1 && insert_lines.last() == Some(&"") {
            insert_lines.pop();
        }
        let mut result: Vec<&str> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let line_num = i + 1;
            if insert_before && line_num == start_line {
                result.extend_from_slice(&insert_lines);
                result.push(line);
            } else {
                result.push(line);
            }
            if insert_after && line_num == start_line {
                result.extend_from_slice(&insert_lines);
            }
        }
        return Ok(join_lines(&result, ends_with_newline));
    }

    let end_line = op
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(start_line);
    if end_line < start_line {
        return Err(ToolError::other("end_line must be >= start_line"));
    }
    if end_line > total_lines {
        return Err(ToolError::other(format!(
            "end_line {} exceeds file length {}",
            end_line, total_lines
        )));
    }
    let new_content = op
        .get("new_content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::other("new_content is required"))?;
    let (start_line, end_line) = match expect {
        Some(e) => verify_or_relocate(&lines, start_line, end_line, e, warnings)?,
        None => (start_line, end_line),
    };
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let mut result: Vec<&str> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let line_num = i + 1;
        if line_num < start_line || line_num > end_line {
            result.push(line);
        } else if line_num == start_line {
            result.extend_from_slice(&new_lines);
        }
    }
    Ok(join_lines(&result, ends_with_newline))
}

fn apply_text_op(
    content: &str,
    op: &Value,
    warnings: &mut Vec<String>,
) -> Result<String, ToolError> {
    let old_text = op
        .get("old_text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::other("old_text is required"))?;
    let delete = op.get("delete").and_then(|v| v.as_bool()).unwrap_or(false);

    // 快路径：逐字节命中。
    if content.contains(old_text) {
        // 唯一性保护：`content.replace` 会替换**所有**出现处——模型想改
        // 一处却静默改了五处，是很难察觉的批量误改。多处命中时要求调用方
        // 明确表态：要么扩上下文让锚点唯一，要么显式声明 `all: true`。
        let hits = content.matches(old_text).count();
        let replace_all = op.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
        if hits > 1 && !replace_all {
            return Err(ToolError::other(format!(
                "old_text 在文件中出现 {hits} 次（歧义），拒绝猜测：请扩大上下文使其\
                 唯一，或显式传 \"all\": true 表示确实要改全部 {hits} 处"
            )));
        }
        if hits > 1 && replace_all {
            warnings.push(format!("old_text 命中 {hits} 处，按 all=true 全部替换"));
        }
        if delete {
            // 修复数据损坏 bug：此前这里做
            //   result.lines().filter(|l| !l.trim().is_empty())
            // 会把**整个文件的所有空行**都删掉（而不只是删除目标片段），
            // 且顺带吃掉行尾换行。现在只移除匹配片段，其余内容原样保留。
            return Ok(content.replace(old_text, ""));
        }
        let new_text = op
            .get("new_text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::other("new_text is required"))?;
        return Ok(content.replace(old_text, new_text));
    }

    // sloppy 回退：精确匹配失败不立刻判死——按行放宽（忽略行尾空白 →
    // 忽略缩进）唯一定位。命中即自愈并 warning；多处候选则拒绝不猜。
    let (lines, ends_with_newline) = split_content(content);
    let needle: Vec<&str> = old_text.split('\n').collect();
    // 允许 old_text 末尾多一个空行。
    let needle: Vec<&str> = if needle.len() > 1 && needle.last() == Some(&"") {
        needle[..needle.len() - 1].to_vec()
    } else {
        needle
    };
    let (hit, mode) = locate_block_sloppy(&lines, &needle, "old_text").map_err(|e| {
        // 保留原始错误措辞里的关键信息，便于模型自我纠正。
        ToolError::other(format!("old_text not found: {:?}（{}）", old_text, e))
    })?;
    let how = match mode {
        LineNorm::Exact => "精确",
        LineNorm::TrailingWs => "忽略行尾空白",
        LineNorm::Indent => "忽略缩进",
    };
    warnings.push(format!(
        "old_text 宽松匹配：逐字节未命中，已按{how}唯一匹配到第 {}-{} 行",
        hit + 1,
        hit + needle.len()
    ));

    let mut result: Vec<&str> = Vec::new();
    result.extend_from_slice(&lines[..hit]);
    if !delete {
        let new_text = op
            .get("new_text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::other("new_text is required"))?;
        result.extend(new_text.split('\n'));
    }
    result.extend_from_slice(&lines[hit + needle.len()..]);
    Ok(join_lines(&result, ends_with_newline))
}

fn file_edit_tool() -> Tool {
    let handler = |input: Value, ctx: ToolExecutionContext| {
        async move {
            let path = input.get("path").and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::other("path is required"))?;
            let ops = input.get("ops").and_then(|v| v.as_array())
                .ok_or_else(|| ToolError::other("ops must be an array"))?;
            if ops.is_empty() {
                return Err(ToolError::other("ops must not be empty"));
            }
            let resolved = resolve_path(path, &ctx);
            let resolved_str = resolved.to_string_lossy().to_string();
            let content = fs::read_to_string(&resolved).await
                .map_err(|e| ToolError::other(format!("failed to read: {}", e)))?;

            // 哈希快照锚点校验（stale anchor）：调用方带了 tag 就与当前
            // 文件比对。
            //
            // **过期 tag 不等于一律拒绝**（对齐 oh-my-pi 的
            // "stale tags attempt snapshot-based recovery"）：tag 只是
            // 文件级的「变过没变过」信号，而**内容级校验强于它**——
            //   · 每个 op 都可按内容自校验（行号 op 带 `expect`、或文本
            //     op 用 `old_text`）→ 降级为 warning 继续，由 op 级的
            //     校验/唯一重定位完成自愈；
            //   · 存在无法内容自校验的 op（行号 op 却没给 expect）→ 保持
            //     硬拒绝，因为此时没有任何依据保证不改错位置。
            // 否则 tag 会抢在重定位之前判死，「行号自愈」永远走不到。
            let mut warnings: Vec<String> = Vec::new();
            let live_tag = content_tag(&content);
            if let Some(want) = input.get("tag").and_then(|v| v.as_str()) {
                let want_norm = want.trim().trim_start_matches('#').to_uppercase();
                if !want_norm.is_empty() && want_norm != live_tag {
                    let all_verifiable = ops.iter().all(|op| {
                        op.get("expect").is_some()
                            || (op.get("old_text").is_some() && op.get("start_line").is_none())
                    });
                    if all_verifiable {
                        warnings.push(format!(
                            "快照已过期（编辑基于 #{want_norm}，当前 #{live_tag}）：\
                             各操作均可按内容自校验，转为内容级校验/重定位继续"
                        ));
                    } else {
                        return Err(ToolError::other(format!(
                            "stale tag：编辑基于快照 #{want_norm}，但文件当前快照是 \
                             #{live_tag}（文件已被改动），且存在无 `expect` 的行号操作\
                             （无法按内容校验）。请重新 read 取得最新 tag 与行号，或给\
                             每个行号操作补上 `expect`。"
                        )));
                    }
                }
            }

            let mut current = content;
            let mut changes: Vec<Value> = Vec::new();
            for (idx, op) in ops.iter().enumerate() {
                let before = current.clone();
                current = if op.get("old_text").is_some() && op.get("start_line").is_none() {
                    apply_text_op(&current, op, &mut warnings)
                        .map_err(|e| ToolError::other(format!("op[{}]: {}", idx, e)))?
                } else if op.get("start_line").is_some() {
                    apply_line_op(&current, op, &mut warnings)
                        .map_err(|e| ToolError::other(format!("op[{}]: {}", idx, e)))?
                } else {
                    return Err(ToolError::other(format!("op[{}]: specify start_line or old_text", idx)));
                };
                if current != before {
                    changes.push(json!({"op_index": idx}));
                }
            }
            fs::write(&resolved, &current).await
                .map_err(|e| ToolError::other(format!("failed to write: {}", e)))?;
            // 回传新 tag：后续对同一文件的编辑可直接带上它，形成
            // read → edit → edit 的锚点链，无需每次重读。
            let mut out = json!({
                "path": resolved_str,
                "success": true,
                "changes": changes,
                "tag": content_tag(&current),
            });
            if !warnings.is_empty() {
                out["warnings"] = json!(warnings);
            }
            Ok(out)
        }.boxed()
    };
    Tool::builder("edit", "Apply precise text edits to a file.", edit_schema(), std::sync::Arc::new(handler))
        .concurrency_safe(false)
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

pub struct EditToolsPackage;

impl EditToolsPackage {
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "edit".into(),
            version: Some("1.0.0".into()),
            namespace: None,
            description: Some("文件编辑工具".into()),
            dependencies: None,
            tools: vec![file_edit_tool()],
            on_init: None, on_destroy: None, before_execute: None, after_execute: None,
            metadata: Some(json!({"category": "file", "tags": ["edit", "file", "patch"]})),
        }
    }
}

impl Default for EditToolsPackage { fn default() -> Self { Self } }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::create_tool_manager;
    use crate::types::ToolManager;
    use serde_json::json;
    use tempfile::TempDir;

    async fn setup_test(content: &str) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, content).await.unwrap();
        (dir, path.to_string_lossy().to_string())
    }

    async fn run_edit(path: &str, ops: Value) -> Value {
        let m = create_tool_manager();
        m.register_package(EditToolsPackage::new()).await.unwrap();
        m.execute("edit", json!({"path": path, "ops": ops}), None).await.unwrap()
    }

    #[tokio::test]
    async fn test_edit_replace_single_line() {
        let (_dir, path) = setup_test("line1\nline2\nline3\n").await;
        let r = run_edit(&path, json!([{"start_line":2,"end_line":2,"new_content":"replaced"}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "line1\nreplaced\nline3\n");
    }

    #[tokio::test]
    async fn test_edit_delete_lines() {
        let (_dir, path) = setup_test("keep1\nremove\nremove\nkeep2\n").await;
        let r = run_edit(&path, json!([{"start_line":2,"end_line":3,"delete":true}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "keep1\nkeep2\n");
    }

    #[tokio::test]
    async fn test_edit_insert_after() {
        let (_dir, path) = setup_test("hello\nworld\n").await;
        let r = run_edit(&path, json!([{"start_line":1,"new_content":"inserted","insert_after":true}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "hello\ninserted\nworld\n");
    }

    #[tokio::test]
    async fn test_edit_text_replace() {
        let (_dir, path) = setup_test("fn old_name() {}\n").await;
        let r = run_edit(&path, json!([{"old_text":"old_name","new_text":"new_name"}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "fn new_name() {}\n");
    }

    // ---------------------------------------------------------------
    // 以下为移植 oh-my-pi「hashline 快照锚点 + sloppy 宽松恢复」后的
    // 回归测试。
    // ---------------------------------------------------------------

    async fn run_edit_raw(path: &str, input: Value) -> Result<Value, ToolError> {
        let m = create_tool_manager();
        m.register_package(EditToolsPackage::new()).await.unwrap();
        m.execute("edit", input, None).await
    }

    /// 数据损坏回归：text delete 此前用
    /// `result.lines().filter(|l| !l.trim().is_empty())`，会把**整个文件**
    /// 的空行都删掉。现在只移除匹配片段，其余（含空行）原样保留。
    #[tokio::test]
    async fn text_delete_preserves_unrelated_blank_lines() {
        let (_dir, path) = setup_test("a\n\nb\nDROP_ME\n\nc\n").await;
        let r = run_edit(&path, json!([{"old_text":"DROP_ME\n","delete":true}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            "a\n\nb\n\nc\n",
            "只删目标片段，其它空行必须保留"
        );
    }

    /// tag 一致 → 放行；并回传新 tag 供后续编辑串联。
    #[tokio::test]
    async fn matching_tag_is_accepted_and_new_tag_returned() {
        let (_dir, path) = setup_test("line1\nline2\n").await;
        let tag = content_tag("line1\nline2\n");
        let r = run_edit_raw(
            &path,
            json!({"path": path, "tag": tag, "ops":[{"start_line":1,"end_line":1,"new_content":"L1"}]}),
        )
        .await
        .expect("tag 一致应放行");
        assert!(r["success"].as_bool().unwrap());
        let new_tag = r["tag"].as_str().unwrap();
        assert_eq!(new_tag, content_tag("L1\nline2\n"), "应回传编辑后的新 tag");
    }

    /// stale tag → 拒绝执行（文件在 read 之后被改过），且**不写入**文件。
    #[tokio::test]
    async fn stale_tag_is_rejected_without_writing() {
        let (_dir, path) = setup_test("current\n").await;
        let err = run_edit_raw(
            &path,
            json!({"path": path, "tag": "0000", "ops":[{"start_line":1,"new_content":"X"}]}),
        )
        .await
        .expect_err("stale tag 必须拒绝");
        let msg = err.to_string();
        assert!(msg.contains("stale tag"), "错误应点明 stale tag: {msg}");
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            "current\n",
            "拒绝时不得改动文件"
        );
    }

    /// 核心自愈：模型行号偏了 2 行，但 expect 内容在全文唯一 →
    /// 自动重定位到正确行，不再静默改错位置。
    #[tokio::test]
    async fn expect_relocates_when_line_number_is_slightly_off() {
        let (_dir, path) = setup_test("h1\nh2\nh3\ntarget_line\ntail\n").await;
        // 模型以为 target_line 在第 2 行，实际在第 4 行。
        let r = run_edit(
            &path,
            json!([{"start_line":2,"end_line":2,"expect":"target_line","new_content":"FIXED"}]),
        )
        .await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            "h1\nh2\nh3\nFIXED\ntail\n",
            "必须改到 expect 实际所在行"
        );
        let warnings = r["warnings"].as_array().expect("应有自愈 warning");
        assert!(
            warnings[0].as_str().unwrap().contains("行号自愈"),
            "warning 应说明重定位: {warnings:?}"
        );
    }

    /// 防静默改错：expect 内容在文件里根本不存在 → 拒绝，不落笔。
    #[tokio::test]
    async fn expect_mismatch_without_candidate_is_rejected() {
        let (_dir, path) = setup_test("a\nb\nc\n").await;
        let err = run_edit_raw(
            &path,
            json!({"path": path, "ops":[{"start_line":1,"expect":"not_in_file","new_content":"X"}]}),
        )
        .await
        .expect_err("expect 找不到必须拒绝");
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "a\nb\nc\n", "拒绝时不得改动文件");
        assert!(err.to_string().contains("expect"), "错误应提到 expect: {err}");
    }

    /// 歧义保护：expect 内容在文件里出现多次 → 拒绝而不猜。
    #[tokio::test]
    async fn expect_ambiguous_match_is_rejected() {
        let (_dir, path) = setup_test("dup\nx\ndup\n").await;
        let err = run_edit_raw(
            &path,
            json!({"path": path, "ops":[{"start_line":2,"expect":"dup","new_content":"X"}]}),
        )
        .await
        .expect_err("多处候选必须拒绝");
        assert!(err.to_string().contains("歧义"), "错误应点明歧义: {err}");
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "dup\nx\ndup\n");
    }

    /// sloppy 宽松匹配：多行 old_text 的缩进与文件不一致（模型复述代码时
    /// 常丢缩进），逐字节子串匹配必然失败，按「忽略缩进」逐行唯一命中并自愈。
    /// 替换内容以 new_text 原文为准（缩进由调用方决定，工具不擅自重排）。
    #[tokio::test]
    async fn old_text_recovers_from_indentation_slip() {
        let (_dir, path) = setup_test("fn main() {\n    let x = 1;\n    let y = 2;\n}\n").await;
        // 模型给的两行 old_text 都没有前导缩进 → 整体子串在文件里不存在。
        let r = run_edit(
            &path,
            json!([{"old_text":"let x = 1;\nlet y = 2;","new_text":"    let z = 3;"}]),
        )
        .await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            "fn main() {\n    let z = 3;\n}\n"
        );
        let warnings = r["warnings"].as_array().expect("应有宽松匹配 warning");
        assert!(
            warnings[0].as_str().unwrap().contains("宽松匹配"),
            "warning 应说明宽松匹配: {warnings:?}"
        );
        assert!(
            warnings[0].as_str().unwrap().contains("忽略缩进"),
            "warning 应说明放宽到忽略缩进: {warnings:?}"
        );
    }

    /// 行尾空白差异（模型吞掉尾随空格）也应自愈。
    #[tokio::test]
    async fn old_text_recovers_from_trailing_whitespace_slip() {
        // 文件里 needle 行带尾随空格，多行使整体子串无法命中。
        let (_dir, path) = setup_test("head\nalpha   \nbeta\ntail\n").await;
        let r = run_edit(&path, json!([{"old_text":"alpha\nbeta","new_text":"merged"}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "head\nmerged\ntail\n");
        let warnings = r["warnings"].as_array().expect("应有宽松匹配 warning");
        assert!(
            warnings[0].as_str().unwrap().contains("忽略行尾空白"),
            "应先按忽略行尾空白命中: {warnings:?}"
        );
    }

    /// 宽松匹配的歧义保护：逐字节子串匹配不到，但忽略缩进后有多处候选
    /// → 拒绝不猜（宁可让模型给更精确的锚点）。
    #[tokio::test]
    async fn sloppy_ambiguous_old_text_is_rejected() {
        let original = "  alpha\n  beta\nmid\n    alpha\n    beta\n";
        let (_dir, path) = setup_test(original).await;
        let err = run_edit_raw(
            &path,
            json!({"path": path, "ops":[{"old_text":"alpha\nbeta","new_text":"x"}]}),
        )
        .await
        .expect_err("宽松匹配多处候选必须拒绝");
        assert!(err.to_string().contains("歧义"), "错误应点明歧义: {err}");
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            original,
            "拒绝时不得改动文件"
        );
    }

    /// old_text 完全找不到时仍然报错（宽松不等于瞎猜）。
    #[tokio::test]
    async fn old_text_absent_still_errors() {
        let (_dir, path) = setup_test("alpha\n").await;
        let err = run_edit_raw(
            &path,
            json!({"path": path, "ops":[{"old_text":"omega","new_text":"x"}]}),
        )
        .await
        .expect_err("完全不存在应报错");
        assert!(err.to_string().contains("old_text not found"), "{err}");
    }

    /// 精确命中时不产生 warning（不打扰正常路径）。
    #[tokio::test]
    async fn exact_match_emits_no_warnings() {
        let (_dir, path) = setup_test("keep\nexact\n").await;
        let r = run_edit(&path, json!([{"old_text":"exact","new_text":"done"}])).await;
        assert!(r.get("warnings").is_none(), "精确路径不应有 warning: {r}");
    }

    /// P2-1 回归：精确子串命中多处时，`content.replace` 会替换**全部**
    /// 出现处 —— 模型想改一处却静默改了 N 处。现在要求唯一，或显式 all。
    #[tokio::test]
    async fn exact_multi_occurrence_requires_explicit_all() {
        let original = "let x = f(1);\nlet y = f(1);\n";
        let (_dir, path) = setup_test(original).await;
        let err = run_edit_raw(
            &path,
            json!({"path": path, "ops":[{"old_text":"f(1)","new_text":"f(2)"}]}),
        )
        .await
        .expect_err("多处命中必须拒绝");
        assert!(err.to_string().contains("出现 2 次"), "应报命中次数: {err}");
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            original,
            "拒绝时不得改动文件"
        );

        // 显式 all=true → 全部替换，并给出 warning。
        let r = run_edit_raw(
            &path,
            json!({"path": path, "ops":[{"old_text":"f(1)","new_text":"f(2)","all":true}]}),
        )
        .await
        .expect("显式 all 应放行");
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            "let x = f(2);\nlet y = f(2);\n"
        );
        let warnings = r["warnings"].as_array().expect("应有 warning");
        assert!(
            warnings.iter().any(|w| w.as_str().unwrap_or("").contains("2 处")),
            "warning 应说明改了几处: {warnings:?}"
        );
    }

    /// 单处命中不受影响（唯一性保护不该妨碍正常路径）。
    #[tokio::test]
    async fn exact_single_occurrence_still_works_without_all() {
        let (_dir, path) = setup_test("only_one_here\n").await;
        let r = run_edit(&path, json!([{"old_text":"only_one_here","new_text":"done"}])).await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "done\n");
        assert!(r.get("warnings").is_none(), "单处命中不应有 warning: {r}");
    }

    /// P2-2 回归：插入内容里的空行必须保留。此前
    /// `.filter(|l| !l.is_empty())` 会把它们全部吃掉，插入的代码段被挤成连续行。
    #[tokio::test]
    async fn insert_preserves_blank_lines_in_new_content() {
        let (_dir, path) = setup_test("head\ntail\n").await;
        let r = run_edit(
            &path,
            json!([{"start_line":1,"insert_after":true,"new_content":"fn a() {}\n\nfn b() {}"}]),
        )
        .await;
        assert!(r["success"].as_bool().unwrap());
        assert_eq!(
            fs::read_to_string(&path).await.unwrap(),
            "head\nfn a() {}\n\nfn b() {}\ntail\n",
            "插入内容中间的空行必须保留"
        );
    }

    /// 插入内容以换行结尾时，只丢那个尾随空串，不产生多余空行。
    #[tokio::test]
    async fn insert_trailing_newline_does_not_add_blank_line() {
        let (_dir, path) = setup_test("a\nb\n").await;
        run_edit(
            &path,
            json!([{"start_line":1,"insert_after":true,"new_content":"mid\n"}]),
        )
        .await;
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "a\nmid\nb\n");
    }

    #[test]
    fn tag_is_stable_and_newline_normalized() {
        assert_eq!(content_tag("a\nb\n"), content_tag("a\r\nb\r\n"), "CRLF 不应改变 tag");
        assert_eq!(content_tag("x"), content_tag("x"), "同内容 tag 必须稳定");
        assert_ne!(content_tag("x"), content_tag("y"));
        assert_eq!(content_tag("x").len(), 4, "tag 是 4 位十六进制");
        assert!(content_tag("x").chars().all(|c| c.is_ascii_hexdigit() && !c.is_lowercase()));
    }
}