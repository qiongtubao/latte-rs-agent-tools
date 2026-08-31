//! Integration tests for the tool manager.

use std::sync::Arc;

use futures::FutureExt;

use crate::prelude::*;

#[tokio::test]
async fn smoke_create_manager() {
    let manager = create_tool_manager();
    let names = manager.get_tool_names();
    assert!(names.is_empty());
}

#[tokio::test]
async fn smoke_register_package() {
    let manager = create_tool_manager();
    manager
        .register_package(GitToolsPackage::new())
        .await
        .unwrap();
    assert!(manager.has("git_status"));
    assert!(manager.has("git_diff"));
    assert!(manager.has("git_log"));
    assert!(manager.has("git_branch"));
    assert!(manager.has("git_commit"));
    assert!(manager.has("git_add"));
    assert_eq!(manager.get_tool_names().len(), 6);
}

#[tokio::test]
async fn smoke_register_and_execute() {
    let manager = create_tool_manager();
    manager
        .register_package(FileToolsPackage::new())
        .await
        .unwrap();

    // read with invalid path
    let result = manager
        .execute("read", serde_json::json!({ "path": "/nonexistent" }), None)
        .await;
    assert!(result.is_err());

    // read with existing path (Cargo.toml)
    let result = manager
        .execute(
            "read",
            serde_json::json!({ "path": "Cargo.toml" }),
            None,
        )
        .await;
    assert!(result.is_ok());
    let v = result.unwrap();
    assert!(v["content"].as_str().unwrap().contains("[package]"));
}

#[tokio::test]
async fn smoke_hooks() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let manager = create_tool_manager();
    let called = Arc::new(AtomicBool::new(false));
    let called_clone = called.clone();

    manager.on(
        ToolHookEvent::BeforeExecute,
        Arc::new(move |_args| {
            let c = called_clone.clone();
            Box::pin(async move {
                c.store(true, Ordering::SeqCst);
                Ok(())
            })
        }),
    );

    manager
        .register_package(FileToolsPackage::new())
        .await
        .unwrap();

    manager
        .execute(
            "read",
            serde_json::json!({ "path": "Cargo.toml" }),
            None,
        )
        .await
        .unwrap();

    assert!(called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn smoke_config_export_import() {
    let manager = create_tool_manager();
    manager
        .register_package(GitToolsPackage::new())
        .await
        .unwrap();

    let config = manager.export_config();
    assert_eq!(config.version, "1.0");
    assert_eq!(config.packages.len(), 1);

    let resolver = Arc::new(
        crate::resolvers::CompositeHandlerResolver::new(std::time::Duration::from_secs(30)),
    );
    // Register handlers by tool names (exported config uses the registered names)
    for name in &["git_status", "git_diff", "git_log", "git_branch", "git_commit", "git_add"] {
        let n = *name;
        resolver.register(
            n,
            Arc::new(move |_input, _ctx| {
                async move { Ok(serde_json::Value::String(format!("mock-{}", n))) }.boxed()
            }),
        );
    }

    let opts = CreateFromConfigOptions {
        handler_resolver: resolver,
        validate: false,
        merge_into: None,
        config_overrides: None,
        skip_init_hooks: true,
    };

    let new_manager = ToolManagerFactory::create_from_config(config, opts)
        .await
        .unwrap();
    assert!(new_manager.has("git_status"));
}
