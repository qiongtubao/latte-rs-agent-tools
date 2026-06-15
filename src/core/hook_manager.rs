//! Lifecycle hook manager. Mirrors `latte-ts-agent-tools/src/core/hook-manager.ts`.
//!
//! Hooks are registered against one of the `ToolHookEvent` variants and invoked
//! in priority order (highest first). A hook can be one-shot (`once: true`),
//! guarded by a `condition: Fn(&[Value]) -> bool`, or be a no-arg `on_destroy`
//! hook that always fires.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::types::{HookFn, HookRegistrationOptions, ToolHookEvent, ToolHookCallbacks};

/// Internal hook entry.
pub struct HookEntry {
    /// The user-supplied callback.
    pub callback: HookFn,
    /// Optional registration metadata.
    pub options: HookRegistrationOptions,
}

/// Thread-safe hook manager.
pub struct HookManagerImpl {
    hooks: RwLock<HashMap<ToolHookEvent, Vec<HookEntry>>>,
}

impl HookManagerImpl {
    /// Construct an empty hook manager.
    pub fn new() -> Self {
        let mut hooks = HashMap::new();
        for event in [
            ToolHookEvent::BeforeExecute,
            ToolHookEvent::AfterExecute,
            ToolHookEvent::OnError,
            ToolHookEvent::OnRetry,
            ToolHookEvent::OnTimeout,
            ToolHookEvent::OnRegister,
            ToolHookEvent::OnUnregister,
            ToolHookEvent::OnPackageRegister,
            ToolHookEvent::OnPackageUnregister,
            ToolHookEvent::OnConfigChange,
            ToolHookEvent::OnDestroy,
        ] {
            hooks.insert(event, Vec::new());
        }
        Self {
            hooks: RwLock::new(hooks),
        }
    }

    /// Register a hook for the given event.
    pub fn on(&self, event: ToolHookEvent, callback: HookFn) {
        self.on_with_options(event, callback, HookRegistrationOptions::default());
    }

    /// Register a hook with extra options (priority / once / condition).
    pub fn on_with_options(
        &self,
        event: ToolHookEvent,
        callback: HookFn,
        options: HookRegistrationOptions,
    ) {
        let mut hooks = self.hooks.write().expect("poisoned");
        let entries = hooks.entry(event).or_default();
        let priority = options.priority;
        // Insert sorted by priority (higher first).
        let pos = entries
            .iter()
            .position(|e| e.options.priority < priority)
            .unwrap_or(entries.len());
        entries.insert(
            pos,
            HookEntry {
                callback,
                options,
            },
        );
    }

    /// Unregister a specific callback (or all callbacks for the event when `None`).
    pub fn off(&self, event: ToolHookEvent, callback: Option<HookFn>) {
        let mut hooks = self.hooks.write().expect("poisoned");
        let entries = hooks.entry(event).or_default();
        match callback {
            None => entries.clear(),
            Some(cb) => {
                if let Some(pos) = entries.iter().position(|e| {
                    std::sync::Arc::ptr_eq(&e.callback, &cb)
                }) {
                    entries.remove(pos);
                }
            }
        }
    }

    /// Register a batch of callbacks at once.
    pub fn register_callbacks(&self, callbacks: ToolHookCallbacks) {
        if let Some(cb) = callbacks.before_execute {
            self.on(ToolHookEvent::BeforeExecute, cb);
        }
        if let Some(cb) = callbacks.after_execute {
            self.on(ToolHookEvent::AfterExecute, cb);
        }
        if let Some(cb) = callbacks.on_error {
            self.on(ToolHookEvent::OnError, cb);
        }
        if let Some(cb) = callbacks.on_retry {
            self.on(ToolHookEvent::OnRetry, cb);
        }
        if let Some(cb) = callbacks.on_timeout {
            self.on(ToolHookEvent::OnTimeout, cb);
        }
        if let Some(cb) = callbacks.on_register {
            self.on(ToolHookEvent::OnRegister, cb);
        }
        if let Some(cb) = callbacks.on_unregister {
            self.on(ToolHookEvent::OnUnregister, cb);
        }
        if let Some(cb) = callbacks.on_package_register {
            self.on(ToolHookEvent::OnPackageRegister, cb);
        }
        if let Some(cb) = callbacks.on_package_unregister {
            self.on(ToolHookEvent::OnPackageUnregister, cb);
        }
        if let Some(cb) = callbacks.on_config_change {
            self.on(ToolHookEvent::OnConfigChange, cb);
        }
        if let Some(cb) = callbacks.on_destroy {
            self.on(ToolHookEvent::OnDestroy, cb);
        }
    }

    /// Remove every hook for every event.
    pub fn clear_hooks(&self) {
        let mut hooks = self.hooks.write().expect("poisoned");
        for entries in hooks.values_mut() {
            entries.clear();
        }
    }

    /// Remove every hook for a single event.
    pub fn clear_hooks_for_event(&self, event: ToolHookEvent) {
        let mut hooks = self.hooks.write().expect("poisoned");
        if let Some(entries) = hooks.get_mut(&event) {
            entries.clear();
        }
    }

    /// List the raw callbacks for an event.
    pub fn get_hooks(&self, event: ToolHookEvent) -> Vec<HookFn> {
        let hooks = self.hooks.read().expect("poisoned");
        hooks
            .get(&event)
            .map(|v| v.iter().map(|e| e.callback.clone()).collect())
            .unwrap_or_default()
    }

    /// Whether any hook is registered for the event.
    pub fn has_hooks(&self, event: ToolHookEvent) -> bool {
        let hooks = self.hooks.read().expect("poisoned");
        hooks
            .get(&event)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// Asynchronously emit an event, awaiting every registered hook in order.
    pub async fn emit(&self, event: ToolHookEvent, args: Vec<serde_json::Value>) {
        let mut to_remove: Vec<ToolHookEvent> = Vec::new();
        let mut remove_at: Option<usize> = None;

        // Take a snapshot of entries to avoid holding the write lock across awaits.
        let snapshot: Vec<HookEntry> = {
            let hooks = self.hooks.read().expect("poisoned");
            hooks
                .get(&event)
                .map(|v| {
                    v.iter()
                        .map(|e| HookEntry {
                            callback: e.callback.clone(),
                            options: e.options.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        for (i, entry) in snapshot.iter().enumerate() {
            if let Some(cond) = &entry.options.condition {
                if !cond(&args) {
                    continue;
                }
            }
            if let Err(e) = (entry.callback)(&args).await {
                log::warn!("hook callback for {} failed: {}", event.as_str(), e);
            }
            if entry.options.once {
                // Mark for removal; we do it after the loop to avoid mutating
                // the lock while iterating.
                to_remove.push(event);
                remove_at = Some(i);
            }
        }

        if !to_remove.is_empty() {
            let mut hooks = self.hooks.write().expect("poisoned");
            if let Some(entries) = hooks.get_mut(&event) {
                if let Some(i) = remove_at {
                    if i < entries.len() {
                        entries.remove(i);
                    }
                }
            }
        }
    }

    /// Synchronously emit an event. Each callback must NOT block on async work
    /// (used for events like `on_register`).
    pub fn emit_sync(&self, event: ToolHookEvent, args: Vec<serde_json::Value>) {
        let snapshot: Vec<HookEntry> = {
            let hooks = self.hooks.read().expect("poisoned");
            hooks
                .get(&event)
                .map(|v| {
                    v.iter()
                        .map(|e| HookEntry {
                            callback: e.callback.clone(),
                            options: e.options.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut to_remove: Option<usize> = None;
        for (i, entry) in snapshot.iter().enumerate() {
            if let Some(cond) = &entry.options.condition {
                if !cond(&args) {
                    continue;
                }
            }
            // We can't await in a sync context; spawn as a detached future.
            let cb = entry.callback.clone();
            let args = args.clone();
            tokio::spawn(async move {
                if let Err(e) = cb(&args).await {
                    log::warn!("async hook callback in emit_sync failed: {}", e);
                }
            });
            if entry.options.once {
                to_remove = Some(i);
            }
        }
        if let Some(i) = to_remove {
            let mut hooks = self.hooks.write().expect("poisoned");
            if let Some(entries) = hooks.get_mut(&event) {
                if i < entries.len() {
                    entries.remove(i);
                }
            }
        }
    }
}

impl Default for HookManagerImpl {
    fn default() -> Self {
        Self::new()
    }
}
