# latte-rs-agent-tools

Tool management implementation for AI agents — Rust port of
[`latte-ts-agent-tools`](../latte-ts-agent-tools/README.md).

## Features

- **Tool Registry** — register, unregister, and query tools with namespace support
- **Hook Manager** — lifecycle hooks (before/after execute, on error, on register, etc.)
- **Handler Resolvers** — builtin, HTTP, script, and composite (chain) resolvers
- **Config Validation** — validate serialized tool manager configurations
- **Built-in Tool Packages** — git, file system, and shell command tools
- **Factory** — JSON-driven create, diff, merge, and scope operations

## Architecture

```
ToolManager  (orchestrator)
    │
    ├── ToolRegistry    (storage / namespace / conflict resolution)
    ├── HookManager     (lifecycle events: before/after/error/...)
    ├── HandlerResolver (builtin / http / script / composite)
    └── ConfigValidator (JSON config validation)
```

## Quick Start

```rust
use latte_rs_agent_tools::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manager = create_tool_manager();

    // Register built-in packages
    manager.register_package(GitToolsPackage::new()).await?;
    manager.register_package(FileToolsPackage::new()).await?;

    // Execute a tool
    let result = manager
        .execute(
            "git.status",
            serde_json::json!({ "short": true }),
            None,
        )
        .await?;

    println!("{}", result);
    Ok(())
}
```

## Built-in Tool Packages

### HTTP (`HttpToolsPackage`)
- `http.fetch` — 发起 HTTP/HTTPS 请求，支持 method / headers / body / timeoutMs / maxSize / followRedirects。
  响应体按 Content-Type 自动选择文本或 base64 编码，超过 maxSize 会被流式截断。

### File (`FileToolsPackage`)
- `file.read` — 读取文件内容，支持行范围选择器（`:N-M`、`:N+count`、`:N`、`:raw`），也支持读取目录列表
- `file.write` — 写入文件内容
- `file.list` — 列出目录内容
- `file.delete` — 删除文件或目录
- `file.search` — 按正则搜索文件内容。支持多目标 (`paths` 数组 / glob 路径)、
  `i` 大小写不敏感、`gitignore` 尊重、`skip` 按文件分页、`limit` 单文件匹配上限。
  `path` 和 `ignoreCase` 是旧版字段的别名，向后兼容。
- `file.find` — 按 glob 模式在目录树下查找文件，支持 `**` 递归、`.gitignore` 尊重、
  mtime 倒序排序、limit 截断、超时控制。`paths` 是 glob 数组，每个元素可以是
  字面文件路径、字面目录路径，或带 `*` `?` `**` 的 glob。
### Git (`GitToolsPackage`)
- `git.status` — 获取 git 工作区状态
- `git.diff` — 查看 git 差异
- `git.log` — 查看 git 提交历史
- `git.branch` — 管理 git 分支
- `git.commit` — 创建 git 提交
- `git.add` — 添加文件到暂存区

### Shell (`ShellToolsPackage`)
- `shell.exec` — 执行 shell 命令并返回输出
- `shell.spawn` — 启动进程并流式处理输出

### Todo (`TodoToolsPackage`)
- `todo` — 管理 todo 列表（原子批量操作）。输入 `currentPhases`（可选）和 `ops`
  数组，每个 op 支持 `init` / `start` / `done` / `rm` / `drop` / `append` / `view`。
  整批原子应用，任一 op 报错则状态保持不变；应用后自动规范化
  （至多一个 in_progress，无 in_progress 时第一个 pending 自动升格）。

## License

MIT
