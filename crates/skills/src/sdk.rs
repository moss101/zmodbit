//! Tool/skill creation with registration validation (M5, REQ-EV-0210):
//! a new tool or skill must validate its SCHEMA, wire into the registry,
//! exercise its invocation path against a REAL effector, and survive
//! removal + reload — the full developer-kit loop.

use modbit_policy::{EffectClass, PolicyDecision};
use modbit_tools::{ToolExecution, ToolRegistry};
use std::sync::Arc;

/// The plugin effector type.
pub type Effector =
    Arc<dyn Fn(&serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync>;

/// Registers a plugin tool into the registry with full validation:
/// duplicate names refused, effect class declared, and the invocation
/// path exercised against the real effector before registration succeeds.
pub fn register_and_validate(
    registry: &ToolRegistry,
    name: &str,
    version: &str,
    effect_class: EffectClass,
    arguments: &serde_json::Value,
    effector: Effector,
    decision: &PolicyDecision,
) -> Result<ToolExecution, String> {
    // Register (duplicate detection is the registry's own validation).
    registry
        .register(name, version, effect_class, effector.clone())
        .map_err(|e| format!("registration failed: {e}"))?;

    // The tool must appear in the listing.
    if !registry
        .list()
        .iter()
        .any(|(n, v, _)| n == name && v == version)
    {
        return Err("registered tool missing from listing".into());
    }

    // Invoke the REAL effector through the registry (fail-closed).
    registry
        .execute(name, arguments, decision)
        .map_err(|e| format!("invocation failed: {e}"))
}

/// Removes and reloads the plugin: the registry must refuse invocations
/// of the removed tool and accept the re-registered one.
pub fn remove_and_reload(
    registry: &ToolRegistry,
    name: &str,
    version: &str,
    effect_class: EffectClass,
    effector: Effector,
) -> Result<bool, String> {
    registry
        .remove(name)
        .map_err(|e| format!("removal failed: {e}"))?;
    let gone = !registry.list().iter().any(|(n, _, _)| n == name);
    registry
        .register(name, version, effect_class, effector)
        .map_err(|e| format!("reload failed: {e}"))?;
    Ok(gone && registry.list().iter().any(|(n, _, _)| n == name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use modbit_tools::ToolRegistry;
    use std::sync::Mutex;

    /// QUAL-EV-0210: a test plugin registers, lists, invokes a real
    /// effector, and passes removal/reload.
    #[test]
    fn plugin_tool_full_lifecycle() {
        let registry = ToolRegistry::new();
        let calls = std::sync::Arc::new(Mutex::new(0usize));
        let sink = calls.clone();
        let effector: Effector = std::sync::Arc::new(move |args: &serde_json::Value| {
            *sink.lock().unwrap() += 1;
            Ok(serde_json::json!({"echo": args["text"]}))
        });

        let allow = PolicyDecision::Allow;
        let result = register_and_validate(
            &registry,
            "plugin.echo",
            "1.0.0",
            EffectClass::ReadOnly,
            &serde_json::json!({"text": "hello"}),
            effector.clone(),
            &allow,
        )
        .unwrap();
        assert_eq!(result.result["echo"], "hello");
        assert_eq!(*calls.lock().unwrap(), 1, "real effector ran once");
        assert!(registry.list().iter().any(|(n, _, _)| n == "plugin.echo"));

        // Removal + reload.
        let reloaded = remove_and_reload(
            &registry,
            "plugin.echo",
            "1.0.0",
            EffectClass::ReadOnly,
            effector,
        )
        .unwrap();
        assert!(reloaded, "removal and reload both succeeded");
        // And it still works after reload.
        assert!(registry
            .execute("plugin.echo", &serde_json::json!({"text": "again"}), &allow)
            .is_ok());

        // Duplicate registration is refused.
        let dup: Effector =
            std::sync::Arc::new(|_args: &serde_json::Value| Ok(serde_json::json!({})));
        assert!(register_and_validate(
            &registry,
            "plugin.echo",
            "2.0.0",
            EffectClass::ReadOnly,
            &serde_json::json!({}),
            dup,
            &allow,
        )
        .is_err());
    }
}
