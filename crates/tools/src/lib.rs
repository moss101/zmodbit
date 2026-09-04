//! modbit-tools — typed tool registry + direct fs/shell tools (M2.4,
//! docs/16 § Tool Registry, docs/17 § Direct tools: fs/git/shell/test).
//!
//! A tool is a typed handler registered under a unique name + version with
//! a declared effect class. Execution requires a policy Allow decision —
//! the registry is fail-closed and never bypasses the capability kernel.
//! Arguments are hashed for evidence.
//!
//! Canonical owner subsystem: tool-runtime (docs/81). Layout: docs/12.

use std::collections::BTreeMap;
use std::fmt;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use modbit_policy::{EffectClass, PolicyDecision};

/// A typed tool handler: JSON args in, JSON result out.
pub type ToolHandler = Arc<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;

#[derive(Clone)]
pub struct RegisteredTool {
    pub name: String,
    pub version: String,
    pub effect_class: EffectClass,
    pub handler: ToolHandler,
}

#[derive(Debug)]
pub enum ToolError {
    UnknownTool(String),
    DuplicateTool(String),
    PolicyDenied { reason: String },
    Handler { message: String },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::UnknownTool(name) => write!(f, "unknown tool {name:?}"),
            ToolError::DuplicateTool(name) => write!(f, "duplicate tool {name:?}"),
            ToolError::PolicyDenied { reason } => write!(f, "policy denied: {reason}"),
            ToolError::Handler { message } => write!(f, "tool handler error: {message}"),
        }
    }
}

impl std::error::Error for ToolError {}

/// The outcome envelope of an executed tool: typed result + evidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolExecution {
    pub tool: String,
    pub arguments_hash: String,
    pub result: Value,
    pub effect_class: EffectClass,
}

pub struct ToolRegistry {
    tools: Mutex<BTreeMap<String, RegisteredTool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Mutex::new(BTreeMap::new()),
        }
    }

    /// Registers a tool handler. Duplicate names are rejected — the registry
    /// guarantees one canonical implementation per tool name (docs/81).
    pub fn register(
        &self,
        name: &str,
        version: &str,
        effect_class: EffectClass,
        handler: ToolHandler,
    ) -> Result<(), ToolError> {
        let mut tools = self.tools.lock().expect("registry mutex poisoned");
        let key = name.to_string();
        if tools.contains_key(&key) {
            return Err(ToolError::DuplicateTool(key));
        }
        tools.insert(
            key,
            RegisteredTool {
                name: name.to_string(),
                version: version.to_string(),
                effect_class,
                handler,
            },
        );
        Ok(())
    }

    pub fn list(&self) -> Vec<(String, String, EffectClass)> {
        self.tools
            .lock()
            .expect("registry mutex poisoned")
            .values()
            .map(|t| (t.name.clone(), t.version.clone(), t.effect_class))
            .collect()
    }

    /// Executes a tool: builds the policy request, expects the caller to
    /// present the kernel's decision, hashes arguments for evidence, and
    /// invokes the typed handler. Fail-closed: no decision → deny.
    pub fn execute(
        &self,
        name: &str,
        arguments: &Value,
        decision: &PolicyDecision,
    ) -> Result<ToolExecution, ToolError> {
        if !decision.is_allow() {
            return Err(ToolError::PolicyDenied {
                reason: format!("kernel decision: {decision}"),
            });
        }
        let tool = self
            .tools
            .lock()
            .expect("registry mutex poisoned")
            .get(name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_string()))?
            .clone();

        let result = (tool.handler)(arguments).map_err(|message| ToolError::Handler { message })?;
        let mut hasher = Sha256::new();
        hasher.update(
            serde_json::to_vec(arguments).map_err(|e| ToolError::Handler {
                message: e.to_string(),
            })?,
        );
        let arguments_hash = format!("{:x}", hasher.finalize());

        Ok(ToolExecution {
            tool: name.to_string(),
            arguments_hash,
            result,
            effect_class: tool.effect_class,
        })
    }
}

/// Direct fs/shell tools (docs/17 § Direct tools: fs/git/shell/test).
pub mod media;
pub mod schema;

pub mod direct {
    use super::*;

    /// fs.list — real directory listing.
    pub fn fs_list(args: &Value) -> Result<Value, String> {
        let dir = args
            .get("dir")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing dir".to_string())?;
        let entries: Vec<String> = std::fs::read_dir(dir)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        Ok(serde_json::json!({ "entries": entries }))
    }

    /// shell.run — structured argv, captured output (no shell strings).
    pub fn shell_run(args: &Value) -> Result<Value, String> {
        let argv: Vec<String> = args
            .get("argv")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .ok_or_else(|| "missing argv".to_string())?;
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        let output = command.output().map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modbit_policy::{PolicyKernel, ToolCallRequest};

    fn kernel() -> PolicyKernel {
        PolicyKernel::new(vec!["/protected".into()])
    }

    fn allow(
        kernel: &PolicyKernel,
        tool: &str,
        class: EffectClass,
        args: &Value,
    ) -> PolicyDecision {
        let grants = vec![modbit_policy::CapabilityGrant {
            grant_id: format!("g-{tool}"),
            tool: tool.into(),
            effect_class: class,
        }];
        let request = ToolCallRequest {
            tool: tool.into(),
            effect_class: class,
            arguments: args.clone(),
        };
        kernel.check(&request, &grants)
    }

    #[test]
    fn register_list_and_duplicate_rejection() {
        let registry = ToolRegistry::new();
        registry
            .register(
                "fs.list",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(direct::fs_list),
            )
            .unwrap();
        let err = registry
            .register(
                "fs.list",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(direct::fs_list),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::DuplicateTool(_)));
        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "fs.list");
    }

    #[test]
    fn real_fs_tool_lists_a_real_directory() {
        let registry = ToolRegistry::new();
        registry
            .register(
                "fs.list",
                "1.0.0",
                EffectClass::ReadOnly,
                Arc::new(direct::fs_list),
            )
            .unwrap();

        let dir = std::env::temp_dir();
        let args = serde_json::json!({ "dir": dir });
        let kernel = kernel();
        let decision = allow(&kernel, "fs.list", EffectClass::ReadOnly, &args);
        let execution = registry.execute("fs.list", &args, &decision).unwrap();
        assert_eq!(execution.effect_class, EffectClass::ReadOnly);
        assert!(!execution.arguments_hash.is_empty());
    }

    #[test]
    fn shell_tool_runs_a_real_command() {
        let registry = ToolRegistry::new();
        registry
            .register(
                "shell.run",
                "1.0.0",
                EffectClass::External,
                Arc::new(direct::shell_run),
            )
            .unwrap();
        let args = serde_json::json!({ "argv": ["git", "--version"] });
        let kernel = kernel();
        let decision = allow(&kernel, "shell.run", EffectClass::External, &args);
        let execution = registry.execute("shell.run", &args, &decision).unwrap();
        let stdout = execution.result.get("stdout").unwrap().as_str().unwrap();
        assert!(stdout.contains("git version"), "{stdout}");
    }

    #[test]
    fn execution_fails_closed_without_a_policy_decision() {
        let registry = ToolRegistry::new();
        registry
            .register(
                "shell.run",
                "1.0.0",
                EffectClass::External,
                Arc::new(direct::shell_run),
            )
            .unwrap();
        let args = serde_json::json!({ "argv": ["git", "--version"] });
        let err = registry
            .execute(
                "shell.run",
                &args,
                &PolicyDecision::Deny {
                    reason: "no grant".into(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::PolicyDenied { .. }));
    }

    #[test]
    fn unknown_tool_is_an_error_even_with_allow() {
        let registry = ToolRegistry::new();
        let args = serde_json::json!({});
        let allow = PolicyDecision::Allow;
        let err = registry.execute("nope", &args, &allow).unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(_)));
    }
}
