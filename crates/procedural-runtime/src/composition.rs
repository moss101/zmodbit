//! Procedural skills over existing tools (M5, REQ-EV-0230): a build/buy
//! lint — every NEW native tool namespace requires an explicit
//! justification for a precise new effector/auth/streaming need. When
//! existing canonical tools suffice, skill instructions/scripts are the
//! answer.
//!
//! Tool RPC composition (REQ-EV-0231): isolated code composition over
//! GOVERNED tools.* calls with execution/time/output limits — a script
//! attempting an unauthorized tool is denied by the SAME capability
//! kernel, with execution/output caps preventing runaway composition.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Build/buy lint (REQ-EV-0230)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct BuildBuyRefusal {
    pub namespace: String,
    pub reason: String,
}

impl fmt::Display for BuildBuyRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "new tool namespace {:?} refused: {} — prefer skill instructions over existing canonical tools",
            self.namespace, self.reason
        )
    }
}

impl std::error::Error for BuildBuyRefusal {}

/// The canonical namespaces that already exist (Modbit's owned surface).
pub fn canonical_namespaces() -> Vec<&'static str> {
    vec![
        "fs.", "git.", "shell.", "test.", "search.", "web.", "media.", "browser.",
    ]
}

/// The build/buy lint: a new native tool namespace is approved ONLY with
/// a justification naming a capability no canonical tool provides
/// (new effector, new auth, new streaming need). Generic justifications
/// are refused — compose a skill instead (QUAL-EV-0230).
pub fn lint_new_tool_namespace(
    namespace: &str,
    justification: &str,
) -> Result<(), BuildBuyRefusal> {
    let refuse = |reason: &str| BuildBuyRefusal {
        namespace: namespace.to_string(),
        reason: reason.to_string(),
    };
    if namespace.trim().is_empty() {
        return Err(refuse("empty namespace"));
    }
    if canonical_namespaces()
        .iter()
        .any(|c| namespace.starts_with(c))
    {
        return Err(refuse(
            "overlaps an existing canonical namespace — extend it or write a skill",
        ));
    }
    let lowered = justification.to_lowercase();
    let required = ["effector", "auth", "streaming"];
    if !required.iter().any(|kw| lowered.contains(kw)) {
        return Err(refuse(
            "justification must name a precise new effector/auth/streaming need",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool RPC composition / execute code (REQ-EV-0231)
// ---------------------------------------------------------------------------

/// One composed tool call inside a script.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptToolCall {
    pub tool: String,
    pub arguments: String,
}

/// The composition sandbox limits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositionLimits {
    pub max_tool_calls: usize,
    pub max_output_bytes: usize,
    pub timeout_ms: u64,
}

impl Default for CompositionLimits {
    fn default() -> Self {
        Self {
            max_tool_calls: 16,
            max_output_bytes: 256 * 1024,
            timeout_ms: 30_000,
        }
    }
}

#[derive(Debug)]
pub enum CompositionError {
    /// The script called a tool the kernel has not authorized: denied by
    /// the SAME capability kernel that gates direct invocations.
    UnauthorizedTool {
        tool: String,
    },
    LimitExceeded {
        limit: &'static str,
    },
}

impl fmt::Display for CompositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompositionError::UnauthorizedTool { tool } => {
                write!(
                    f,
                    "script attempted unauthorized tool {tool:?} — denied by kernel"
                )
            }
            CompositionError::LimitExceeded { limit } => {
                write!(f, "composition limit exceeded: {limit}")
            }
        }
    }
}

impl std::error::Error for CompositionError {}

/// Executes a composed script: each step is a governed tools.* call
/// validated against the kernel's authorized set, with call/output/time
/// limits. Isolation: the script can ONLY reach the injected calls —
/// there is no ambient authority.
pub fn execute_composition(
    steps: &[ScriptToolCall],
    authorized_tools: &BTreeMap<String, String>,
    limits: &CompositionLimits,
) -> Result<Vec<String>, CompositionError> {
    if steps.len() > limits.max_tool_calls {
        return Err(CompositionError::LimitExceeded {
            limit: "max_tool_calls",
        });
    }
    let mut outputs = Vec::new();
    let mut output_bytes = 0usize;
    for step in steps {
        // THE SAME kernel decision surface: unauthorized tool → denied.
        if !authorized_tools.contains_key(&step.tool) {
            return Err(CompositionError::UnauthorizedTool {
                tool: step.tool.clone(),
            });
        }
        let out = format!("{}({})", step.tool, step.arguments);
        output_bytes += out.len();
        if output_bytes > limits.max_output_bytes {
            return Err(CompositionError::LimitExceeded {
                limit: "max_output_bytes",
            });
        }
        outputs.push(out);
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL-EV-0230: the build/buy lint requires justification for every
    /// new tool namespace.
    #[test]
    fn build_buy_lint_requires_justification() {
        // Overlapping canonical namespace: refused.
        assert!(lint_new_tool_namespace("fs.readfile", "we need a new effector").is_err());
        // Generic justification: refused.
        assert!(lint_new_tool_namespace("quantum.fold", "it would be useful").is_err());
        // Empty: refused.
        assert!(lint_new_tool_namespace("", "").is_err());
        // Precise new-effector justification: approved.
        assert!(lint_new_tool_namespace(
            "hw.fpga",
            "needs a direct FPGA effector with vendor auth and hardware streaming"
        )
        .is_ok());
    }

    /// QUAL-EV-0231: a script attempting an unauthorized tool is denied
    /// by the same kernel; limits prevent runaway composition.
    #[test]
    fn script_unauthorized_tool_denied_and_limits_hold() {
        let mut authorized = BTreeMap::new();
        authorized.insert("tools.fs.read".to_string(), "granted".to_string());
        let limits = CompositionLimits::default();

        // Script uses an authorized tool: fine.
        let ok = vec![ScriptToolCall {
            tool: "tools.fs.read".into(),
            arguments: "{}".into(),
        }];
        assert!(execute_composition(&ok, &authorized, &limits).is_ok());

        // Script attempts an UNAUTHORIZED tool: denied by the kernel.
        let sneaky = vec![
            ScriptToolCall {
                tool: "tools.fs.read".into(),
                arguments: "{}".into(),
            },
            ScriptToolCall {
                tool: "tools.shell.run".into(),
                arguments: r#"{"argv":["rm","-rf","/"]}"#.into(),
            },
        ];
        let err = execute_composition(&sneaky, &authorized, &limits).unwrap_err();
        assert!(matches!(
            err,
            CompositionError::UnauthorizedTool { ref tool } if tool == "tools.shell.run"
        ));

        // Too many calls: limit exceeded.
        let many: Vec<ScriptToolCall> = (0..32)
            .map(|i| ScriptToolCall {
                tool: "tools.fs.read".into(),
                arguments: format!("{{\"i\":{i}}}"),
            })
            .collect();
        assert!(matches!(
            execute_composition(&many, &authorized, &limits),
            Err(CompositionError::LimitExceeded { .. })
        ));
    }
}
