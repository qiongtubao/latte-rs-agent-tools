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

### Git (`GitToolsPackage`)
- `git.status` — 获取 git 工作区状态
- `git.diff` — 查看 git 差异
- `git.log` — 查看 git 提交历史
- `git.branch` — 管理 git 分支
- `git.commit` — 创建 git 提交
- `git.add` — 添加文件到暂存区

### File (`FileToolsPackage`)
- `file.read` — 读取文件内容
- `file.write` — 写入文件内容
- `file.list` — 列出目录内容
- `file.delete` — 删除文件或目录
- `file.search` — 在文件中搜索内容

### Shell (`ShellToolsPackage`)
- `shell.exec` — 执行 shell 命令并返回输出
- `shell.spawn` — 启动进程并流式处理输出

## Configuration API

```rust
use latte_rs_agent_tools::prelude::*;

// Validate config
let config = ToolManagerSerializedConfig { /* ... */ };
let result = ConfigValidatorImpl::new().validate(&config);
assert!(result.valid);

// Create from config
let resolver = Arc::new(CompositeHandlerResolver::new(
    std::time::Duration::from_secs(30),
));
let opts = CreateFromConfigOptions {
    handler_resolver: resolver,
    validate: true,
    merge_into: None,
    config_overrides: None,
    skip_init_hooks: false,
};
let manager = ToolManagerFactory::create_from_config(config, opts).await?;

// Diff two configs
let diff = ToolManagerFactory::diff_configs(&config_a, &config_b);
```

## License

MIT
