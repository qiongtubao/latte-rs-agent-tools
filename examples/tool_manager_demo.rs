//! Demonstrates the tool manager with built-in packages.

use latte_rs_agent_tools::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manager = create_tool_manager();

    // Register built-in packages
    manager
        .register_package(GitToolsPackage::new())
        .await?;
    manager
        .register_package(FileToolsPackage::new())
        .await?;
    manager
        .register_package(HttpToolsPackage::new())
        .await?;
    manager
        .register_package(TodoToolsPackage::new())
        .await?;
    // List all tools
    let names = manager.get_tool_names();
    println!("Registered {} tools:", names.len());
    for n in &names {
        println!("  - {}", n);
    }

    // Get tool definitions for LLM
    let defs = manager.get_tool_definitions();
    println!(
        "\nTool definitions: {}",
        serde_json::to_string_pretty(&defs)?
    );

    // Execute git.status
    let result = manager
        .execute("git.status", serde_json::json!({ "short": true }), None)
        .await?;
    println!("\nGit status result: {}", result);

    // Export config
    let config = manager.export_config();
    println!(
        "\nExported config: {}",
        serde_json::to_string_pretty(&config)?
    );

    // Hooks
    manager.on(ToolHookEvent::BeforeExecute, std::sync::Arc::new(|args| {
        Box::pin(async move {
            println!(">>> Before execute: {:?}", args);
            Ok(())
        })
    }));

    // Execute again to trigger hook
    manager
        .execute("git.status", serde_json::json!({ "short": true }), None)
        .await?;

    // Clean up
    manager.destroy().await;
    println!("\nManager destroyed.");
    Ok(())
}
