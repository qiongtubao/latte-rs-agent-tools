//! Todo list tool. Mirrors the `todo` tool in oh-my-pi/coding-agent.
//!
//! 提供 `todo` 工具：管理一个分阶段的 todo 列表，支持原子批量操作。
//! 与 oh-my-pi 完整版相比，本工具是「纯函数」版——
//! 调用方传入 `currentPhases`（可选），工具返回新的 `phases`，由调用方自行持久化。
//! 这样可以避免在工具里持有进程级状态，测试也容易。
//!
//! ## 输入
//!
//! - `currentPhases` (array, 可选) — 当前 todo 列表。首调用可不传（默认空）。
//!   元素结构：`{"name": "阶段名", "tasks": [{"content": "...", "status": "..."}]}`。
//!   状态取值：`"pending" | "in_progress" | "completed" | "abandoned"`。
//! - `ops` (array, 必填) — 原子操作序列，按顺序应用。任一 op 失败则整批丢弃。
//!   每个 op 结构：`{"op": "...", ...}`，op 取值见下表。
//!
//! ### op 类型
//!
//! | op        | 必填字段            | 行为                                                  |
//! |-----------|--------------------|------------------------------------------------------|
//! | `init`    | `list`             | 用 `list` 替换整个 phases 列表（list 是 `[{phase, items}]`） |
//! | `start`   | `task`             | 把任务状态置为 `in_progress`（自动取消其他 in_progress） |
//! | `done`    | `task`             | 把任务状态置为 `completed`                              |
//! | `rm`      | `task`             | 删除任务（不留空 phase）                                |
//! | `drop`    | `task`             | 把任务状态置为 `abandoned`                             |
//! | `append`  | `phase`, `items`   | 在 `phase` 末尾追加 `items`（新 phase 不存在则创建）        |
//! | `view`    | (无)               | 只读——返回当前 phases，不应用任何修改                      |
//!
//! ## 输出
//!
//! ```jsonc
//! {
//!   "phases": [                        // 操作后的 phases
//!     {
//!       "name": "Phase 1",
//!       "tasks": [
//!         { "content": "task A", "status": "completed" },
//!         { "content": "task B", "status": "in_progress" }
//!       ]
//!     }
//!   ],
//!   "completedTasks": [                 // 本次 op 中「从非 completed 变 completed」的任务
//!     { "phase": "Phase 1", "content": "task A" }
//!   ],
//!   "errors": []                        // op 错误信息（任一 op 报错时整批不应用）
//! }
//! ```
//!
//! ## 原子性 + 规范化
//!
//! - 原子性：op 按顺序应用；任一 op 报错则整批丢弃，状态保持不变。
//! - 规范化：应用完成后，「至多一个 `in_progress`」——多余的会回退成 `pending`；
//!   若没有 `in_progress` 且有 `pending`，第一个 `pending` 自动升格为 `in_progress`。
//! - 这两个不变量在「首个 `init` 创建任务时」也成立，所以空 todo 调用 `view` 之后
//!   也能拿到一个「第一个 task 自动 in_progress」的列表。

use std::collections::HashMap;
use std::sync::Arc;

use futures::FutureExt;
use serde_json::{json, Value};

use crate::types::{PropertyType, Tool, ToolExecutionContext, ToolInputProperty, ToolInputSchema, ToolPackage};

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// 未开始（默认）。
    Pending,
    /// 进行中——一个 phase 内至多一个 in_progress。
    InProgress,
    /// 已完成——驱动 `completedTasks` 转换。
    Completed,
    /// 已放弃（不再做，但仍保留在列表里）。
    Abandoned,
}

/// 一条 todo。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    /// 任务描述。
    pub content: String,
    /// 状态。
    pub status: TodoStatus,
}

/// 一个 phase（一组有序的 todo）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoPhase {
    /// 阶段名。
    pub name: String,
    /// 阶段内的 task 列表（保序）。
    pub tasks: Vec<TodoItem>,
}

/// 本次 op 触发的「从非 completed 变 completed」转换。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoCompletion {
    /// 阶段名。
    pub phase: String,
    /// 任务描述。
    pub content: String,
}

/// 工具输出。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoResult {
    /// 操作后的 phases（apply 失败时退回到调用前的 prev）。
    pub phases: Vec<TodoPhase>,
    /// 本次 batch 中「从非 completed 变 completed」的 task。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub completed_tasks: Vec<TodoCompletion>,
    /// 错误信息（apply 失败时填；非空时整批不应用）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}
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

/// 构造一个 schema：所有属性都出现，但只有 `ops` 在 `required` 列表里。
fn optional(props: Vec<(&str, PropertyType, &str)>, required: &[&str]) -> ToolInputSchema {
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

use std::collections::BTreeMap;

/// 解析 `currentPhases` 字段。允许缺省 / 非数组 / 缺字段。
fn parse_current_phases(input: &Value) -> Vec<TodoPhase> {
    let Some(arr) = input.get("currentPhases").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut phases: Vec<TodoPhase> = Vec::new();
    for ph in arr {
        let name = ph
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let mut tasks: Vec<TodoItem> = Vec::new();
        if let Some(tasks_arr) = ph.get("tasks").and_then(|v| v.as_array()) {
            for t in tasks_arr {
                let content = t
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if content.is_empty() {
                    continue;
                }
                let status = match t.get("status").and_then(|v| v.as_str()) {
                    Some("in_progress") => TodoStatus::InProgress,
                    Some("completed") => TodoStatus::Completed,
                    Some("abandoned") => TodoStatus::Abandoned,
                    _ => TodoStatus::Pending,
                };
                tasks.push(TodoItem { content, status });
            }
        }
        phases.push(TodoPhase { name, tasks });
    }
    phases
}

/// 找到指定 phase 中的 task。返回 `(phase_index, task_index)`。
fn find_task(phases: &[TodoPhase], content: &str) -> Option<(usize, usize)> {
    for (pi, ph) in phases.iter().enumerate() {
        if let Some(ti) = ph.tasks.iter().position(|t| t.content == content) {
            return Some((pi, ti));
        }
    }
    None
}

/// 找 phase，按 name。
fn find_phase(phases: &[TodoPhase], name: &str) -> Option<usize> {
    phases.iter().position(|p| p.name == name)
}

/// 规范化：只保留第一个 `in_progress`，把多余的回退成 `pending`；
/// 若没有任何 `in_progress` 且有 `pending`，把第一个 `pending` 升格。
fn normalize(phases: &mut [TodoPhase]) {
    // 收集所有 task 的引用，避免借用冲突
    let mut all_tasks: Vec<&mut TodoItem> =
        phases.iter_mut().flat_map(|p| p.tasks.iter_mut()).collect();
    if all_tasks.is_empty() {
        return;
    }

    // 1. 多个 in_progress 时，只保留第一个
    let mut first_ip = true;
    for t in all_tasks.iter_mut() {
        if t.status == TodoStatus::InProgress {
            if first_ip {
                first_ip = false;
            } else {
                t.status = TodoStatus::Pending;
            }
        }
    }

    // 2. 没有 in_progress 且有 pending 时，第一个 pending 升格
    let has_ip = all_tasks.iter().any(|t| t.status == TodoStatus::InProgress);
    if !has_ip {
        if let Some(t) = all_tasks
            .iter_mut()
            .find(|t| t.status == TodoStatus::Pending)
        {
            t.status = TodoStatus::InProgress;
        }
    }
}

/// 找出本次 op 中「从非 completed 变 completed」的任务。
fn detect_completions(prev: &[TodoPhase], updated: &[TodoPhase]) -> Vec<TodoCompletion> {
    // 索引化 prev
    let mut prev_map: HashMap<(String, String), TodoStatus> = HashMap::new();
    for ph in prev {
        for t in &ph.tasks {
            prev_map.insert((ph.name.clone(), t.content.clone()), t.status);
        }
    }
    let mut completions = Vec::new();
    for ph in updated {
        for t in &ph.tasks {
            if t.status == TodoStatus::Completed {
                let prev_status = prev_map.get(&(ph.name.clone(), t.content.clone())).copied();
                if !matches!(prev_status, Some(TodoStatus::Completed)) {
                    completions.push(TodoCompletion {
                        phase: ph.name.clone(),
                        content: t.content.clone(),
                    });
                }
            }
        }
    }
    completions
}

/// 应用单个 op 到 `phases`，原地修改；返回 `Ok(())` 或错误消息。
fn apply_op(phases: &mut Vec<TodoPhase>, op: &Value) -> Result<(), String> {
    let op_name = op
        .get("op")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing op field".to_string())?;

    match op_name {
        "init" => {
            // 用 list 替换整个 phases
            let list = op
                .get("list")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "init: list is required".to_string())?;
            let mut new_phases: Vec<TodoPhase> = Vec::new();
            for entry in list {
                let name = entry
                    .get("phase")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "init.list: phase is required".to_string())?
                    .to_string();
                if name.is_empty() {
                    return Err("init.list: phase name must not be empty".into());
                }
                let items = entry
                    .get("items")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| format!("init.list: items required for phase '{}'", name))?;
                if items.is_empty() {
                    return Err(format!("init.list: items for phase '{}' must be non-empty", name));
                }
                let mut tasks: Vec<TodoItem> = Vec::new();
                for it in items {
                    let content = it
                        .as_str()
                        .ok_or_else(|| {
                            format!("init.list: items must be strings (phase '{}')", name)
                        })?
                        .to_string();
                    if content.is_empty() {
                        return Err(format!(
                            "init.list: task content must not be empty (phase '{}')",
                            name
                        ));
                    }
                    tasks.push(TodoItem {
                        content,
                        status: TodoStatus::Pending,
                    });
                }
                new_phases.push(TodoPhase { name, tasks });
            }
            *phases = new_phases;
            Ok(())
        }
        "start" => {
            let task = op
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "start: task is required".to_string())?;
            let (pi, ti) = find_task(phases, task)
                .ok_or_else(|| format!("start: task '{}' not found", task))?;
            phases[pi].tasks[ti].status = TodoStatus::InProgress;
            Ok(())
        }
        "done" => {
            let task = op
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "done: task is required".to_string())?;
            let (pi, ti) = find_task(phases, task)
                .ok_or_else(|| format!("done: task '{}' not found", task))?;
            phases[pi].tasks[ti].status = TodoStatus::Completed;
            Ok(())
        }
        "rm" => {
            let task = op
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "rm: task is required".to_string())?;
            let (pi, ti) = find_task(phases, task)
                .ok_or_else(|| format!("rm: task '{}' not found", task))?;
            phases[pi].tasks.remove(ti);
            // phase 删空 → 整个 phase 也删
            if phases[pi].tasks.is_empty() {
                phases.remove(pi);
            }
            Ok(())
        }
        "drop" => {
            let task = op
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "drop: task is required".to_string())?;
            let (pi, ti) = find_task(phases, task)
                .ok_or_else(|| format!("drop: task '{}' not found", task))?;
            phases[pi].tasks[ti].status = TodoStatus::Abandoned;
            Ok(())
        }
        "append" => {
            let phase_name = op
                .get("phase")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "append: phase is required".to_string())?;
            let items = op
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| format!("append: items required for phase '{}'", phase_name))?;
            if items.is_empty() {
                return Err(format!(
                    "append: items for phase '{}' must be non-empty",
                    phase_name
                ));
            }
            let pi = match find_phase(phases, phase_name) {
                Some(idx) => idx,
                None => {
                    phases.push(TodoPhase {
                        name: phase_name.to_string(),
                        tasks: Vec::new(),
                    });
                    phases.len() - 1
                }
            };
            for it in items {
                let content = it
                    .as_str()
                    .ok_or_else(|| {
                        format!("append: items must be strings (phase '{}')", phase_name)
                    })?
                    .to_string();
                if content.is_empty() {
                    return Err(format!(
                        "append: task content must not be empty (phase '{}')",
                        phase_name
                    ));
                }
                phases[pi].tasks.push(TodoItem {
                    content,
                    status: TodoStatus::Pending,
                });
            }
            Ok(())
        }
        "view" => {
            // 只读，不修改
            Ok(())
        }
        other => Err(format!("unknown op: '{}'", other)),
    }
}

/// 构造 `todo` 工具定义。
pub fn todo_tool() -> Tool {
    let handler = |input: Value, _ctx: ToolExecutionContext| {
        async move {
            // --- 1. 解析 currentPhases -------------------------------------
            let mut phases = parse_current_phases(&input);

            // --- 2. 解析 ops ------------------------------------------------
            let ops = input
                .get("ops")
                .and_then(|v| v.as_array())
                .ok_or_else(|| crate::error::ToolError::other("ops is required (array)"))?;
            if ops.is_empty() {
                return Err(crate::error::ToolError::other("ops must not be empty"));
            }

            // --- 3. 是否 read-only（全是 view）-----------------------------
            let read_only = ops
                .iter()
                .all(|op| op.get("op").and_then(|v| v.as_str()) == Some("view"));

            // --- 4. 快照 prev 用来算 completion transitions ----------------
            let prev = phases.clone();

            // --- 5. 应用 ops（原子：任一错则丢弃整个 batch）---------------
            let mut errors: Vec<String> = Vec::new();
            if !read_only {
                for op in ops {
                    if let Err(msg) = apply_op(&mut phases, op) {
                        errors.push(msg);
                    }
                }
            }
            let failed = !errors.is_empty();
            let effective_phases = if failed { prev.clone() } else { phases.clone() };

            // --- 6. 规范化（只对非 read-only 的成功 batch）-----------------
            let mut normalized = effective_phases.clone();
            if !read_only && !failed {
                normalize(&mut normalized);
            }

            // --- 7. 算 completions（只在成功 batch 算）---------------------
            let completed_tasks = if read_only || failed {
                Vec::new()
            } else {
                detect_completions(&prev, &normalized)
            };

            // --- 8. 组装结果 -----------------------------------------------
            let result = TodoResult {
                phases: normalized,
                completed_tasks,
                errors,
            };
            // 序列化时用 serde_json 转 Value，保持跟其它工具一致。
            let value = serde_json::to_value(&result).map_err(|e| {
                crate::error::ToolError::other(format!("serialize result: {}", e))
            })?;
            // 失败时返回的 phases 应当是 prev（未修改），上面的代码已经处理。
            // 错误时也以错误形式抛出，但保留 phases 让上层能看到上一次状态。
            if failed {
                // 整体 batch 失败 → 把所有错误信息合并到错误消息里；
                // 同时把 prev 状态附在 result 里（虽然 errors 非空，但调用方仍可看）。
                let mut v = value;
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("phases".to_string(), serde_json::to_value(&prev).unwrap());
                }
                return Ok(v);
            }
            Ok(value)
        }
        .boxed()
    };

    Tool::builder(
        "todo",
        "管理 todo 列表（原子批量操作）",
        optional(
            vec![
                (
                    "currentPhases",
                    PropertyType::String,
                    "JSON 序列化的 phases 数组（或直接传数组）",
                ),
                (
                    "ops",
                    PropertyType::String,
                    "原子操作序列，JSON 序列化数组",
                ),
            ],
            &["ops"],
        ),
        Arc::new(handler),
    )
    .concurrency_safe(false) // ops 通常要串行应用
    .timeout(std::time::Duration::from_secs(10))
    .build()
}

/// `todo` 工具包：目前只包含 `todo` 一个工具。
pub struct TodoToolsPackage;

impl TodoToolsPackage {
    /// 构造包（单个 `todo` 工具）。
    pub fn new() -> ToolPackage {
        ToolPackage {
            name: "todo".into(),
            version: Some("1.0.0".into()),
            namespace: None,
            description: Some("Todo 列表管理（原子批量操作）".into()),
            dependencies: None,
            tools: vec![todo_tool()],
            on_init: None,
            on_destroy: None,
            before_execute: None,
            after_execute: None,
            metadata: Some(json!({"category": "session", "tags": ["todo", "task"]})),
        }
    }
}

impl Default for TodoToolsPackage {
    fn default() -> Self {
        Self
    }
}

// =============================================================================
// 单测
// =============================================================================
//
// 测试 todo 状态机的核心语义：原子性、规范化、completion 检测。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn run(input: Value) -> Result<Value, crate::error::ToolError> {
        let tool = todo_tool();
        let ctx = ToolExecutionContext::fresh("todo", 0);
        (tool.handler)(input, ctx).await
    }

    /// 辅助：从 output 取出 phases 列表。
    fn phases_of(out: &Value) -> Vec<TodoPhase> {
        serde_json::from_value(out["phases"].clone()).unwrap()
    }

    /// 测试：init 创建一个 phase + 3 个 tasks，第一个自动 in_progress。
    #[tokio::test]
    async fn todo_init_creates_phase_and_normalizes() {
        let out = run(json!({
            "ops": [{
                "op": "init",
                "list": [{
                    "phase": "Phase 1",
                    "items": ["A", "B", "C"]
                }]
            }]
        }))
        .await
        .unwrap();
        let phases = phases_of(&out);
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].name, "Phase 1");
        assert_eq!(phases[0].tasks.len(), 3);
        assert_eq!(phases[0].tasks[0].content, "A");
        assert_eq!(phases[0].tasks[0].status, TodoStatus::InProgress); // 自动升格
        assert_eq!(phases[0].tasks[1].status, TodoStatus::Pending);
        assert_eq!(phases[0].tasks[2].status, TodoStatus::Pending);
    }

    /// 测试：start 触发规范化——把另一个 in_progress 任务回退成 pending。
    #[tokio::test]
    async fn todo_start_normalizes_existing_in_progress() {
        let out = run(json!({
            "currentPhases": [
                { "name": "P", "tasks": [
                    { "content": "A", "status": "in_progress" },
                    { "content": "B", "status": "in_progress" }
                ]}
            ],
            "ops": [
                { "op": "done", "task": "A" }
            ]
        }))
        .await
        .unwrap();
        let phases = phases_of(&out);
        // A: completed, B: 被规范化为 in_progress
        let p = &phases[0];
        assert_eq!(p.tasks[0].content, "A");
        assert_eq!(p.tasks[0].status, TodoStatus::Completed);
        assert_eq!(p.tasks[1].content, "B");
        assert_eq!(p.tasks[1].status, TodoStatus::InProgress);
    }

    /// 测试：done 不会自动推进到下一个 pending 任务——但 normalize 会在 batch 结束时做这件事。
    /// done A 之后：剩下 B 仍 pending，normalize 后 B → in_progress。
    #[tokio::test]
    async fn todo_done_triggers_completion_and_advances() {
        let out = run(json!({
            "currentPhases": [
                { "name": "P", "tasks": [
                    { "content": "A", "status": "in_progress" },
                    { "content": "B", "status": "pending" }
                ]}
            ],
            "ops": [
                { "op": "done", "task": "A" }
            ]
        }))
        .await
        .unwrap();
        let phases = phases_of(&out);
        assert_eq!(phases[0].tasks[0].status, TodoStatus::Completed);
        assert_eq!(phases[0].tasks[1].status, TodoStatus::InProgress);
        // completedTasks 应记录 A
        let completions: Vec<TodoCompletion> = serde_json::from_value(out["completedTasks"].clone()).unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].content, "A");
    }

    /// 测试：原子性——任一 op 报错，整批不应用，状态保持 prev。
    #[tokio::test]
    async fn todo_atomic_failure_keeps_prev_state() {
        let out = run(json!({
            "currentPhases": [
                { "name": "P", "tasks": [
                    { "content": "A", "status": "pending" }
                ]}
            ],
            "ops": [
                { "op": "done", "task": "A" },   // 成功
                { "op": "start", "task": "nonexistent" } // 失败
            ]
        }))
        .await
        .unwrap();
        // 应该报错（虽然不抛 ToolError，但 errors 数组里要有信息）
        let errors: Vec<String> = serde_json::from_value(out["errors"].clone()).unwrap();
        assert!(!errors.is_empty());
        // 状态应保持 prev：A 仍 pending
        let phases = phases_of(&out);
        assert_eq!(phases[0].tasks[0].status, TodoStatus::Pending);
    }

    /// 测试：rm 删除空 phase——phase 自身也被删。
    #[tokio::test]
    async fn todo_rm_removes_empty_phase() {
        let out = run(json!({
            "currentPhases": [
                { "name": "P1", "tasks": [{"content": "A", "status": "pending"}] },
                { "name": "P2", "tasks": [{"content": "B", "status": "pending"}] }
            ],
            "ops": [
                { "op": "rm", "task": "A" }
            ]
        }))
        .await
        .unwrap();
        let phases = phases_of(&out);
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].name, "P2");
    }

    /// 测试：append 到已存在 phase + append 到新 phase。
    #[tokio::test]
    async fn todo_append_to_existing_and_new_phase() {
        let out = run(json!({
            "currentPhases": [
                { "name": "P1", "tasks": [{"content": "A", "status": "pending"}] }
            ],
            "ops": [
                { "op": "append", "phase": "P1", "items": ["B", "C"] },
                { "op": "append", "phase": "P2", "items": ["D"] }
            ]
        }))
        .await
        .unwrap();
        let phases = phases_of(&out);
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].tasks.len(), 3); // A + B + C
        assert_eq!(phases[1].name, "P2");
        assert_eq!(phases[1].tasks.len(), 1);
    }

    /// 测试：drop 把任务标记为 abandoned。
    #[tokio::test]
    async fn todo_drop_marks_abandoned() {
        let out = run(json!({
            "currentPhases": [
                { "name": "P", "tasks": [{"content": "A", "status": "pending"}] }
            ],
            "ops": [
                { "op": "drop", "task": "A" }
            ]
        }))
        .await
        .unwrap();
        let phases = phases_of(&out);
        assert_eq!(phases[0].tasks[0].status, TodoStatus::Abandoned);
    }

    /// 测试：view 是只读的——不修改任何状态。
    #[tokio::test]
    async fn todo_view_is_readonly() {
        let out = run(json!({
            "currentPhases": [
                { "name": "P", "tasks": [
                    { "content": "A", "status": "in_progress" },
                    { "content": "B", "status": "in_progress" }
                ]}
            ],
            "ops": [
                { "op": "view" }
            ]
        }))
        .await
        .unwrap();
        let phases = phases_of(&out);
        // view 不应用 normalize，所以两个都还是 in_progress
        assert_eq!(phases[0].tasks[0].status, TodoStatus::InProgress);
        assert_eq!(phases[0].tasks[1].status, TodoStatus::InProgress);
    }

    /// 测试：init 后再 done → completedTasks 应记录 done 的那个。
    #[tokio::test]
    async fn todo_completion_detection() {
        let out = run(json!({
            "ops": [
                { "op": "init", "list": [{ "phase": "P", "items": ["A", "B"] }] },
                { "op": "done", "task": "A" }
            ]
        }))
        .await
        .unwrap();
        let completions: Vec<TodoCompletion> = serde_json::from_value(out["completedTasks"].clone()).unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].content, "A");
        // 第一个 init 之后：A 是 in_progress（normalize 把它升格）
        // 但 done 不会取消 in_progress——所以 done A 时 A 还是 in_progress，
        // 触发了「非 completed → completed」转换。
        let phases = phases_of(&out);
        assert_eq!(phases[0].tasks[0].status, TodoStatus::Completed);
        // B 升格成 in_progress
        assert_eq!(phases[0].tasks[1].status, TodoStatus::InProgress);
    }

    /// 测试：空 ops → 报错。
    #[tokio::test]
    async fn todo_empty_ops_errors() {
        let err = run(json!({ "ops": [] })).await.unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    /// 测试：未知 op → 报错，prev 状态保留。
    #[tokio::test]
    async fn todo_unknown_op_does_not_corrupt_state() {
        let out = run(json!({
            "currentPhases": [
                { "name": "P", "tasks": [{"content": "A", "status": "pending"}] }
            ],
            "ops": [
                { "op": "frobnicate" }
            ]
        }))
        .await
        .unwrap();
        let errors: Vec<String> = serde_json::from_value(out["errors"].clone()).unwrap();
        assert!(errors.iter().any(|e| e.contains("unknown op")));
        let phases = phases_of(&out);
        assert_eq!(phases[0].tasks[0].status, TodoStatus::Pending);
    }
}
