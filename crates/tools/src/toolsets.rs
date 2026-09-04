//! Toolsets / capability grouping (M2, REQ-EV-0229, ADAPT): toolsets group
//! discoverable tools for PROJECTION convenience, but authority is always
//! resolved in the Capability Kernel. Enabling a toolset can never expose
//! a tool the kernel denies: the projection is the INTERSECTION of the
//! toolset with the session's authorized surface.

use serde::{Deserialize, Serialize};

/// A named grouping of tool names (e.g. "rust-dev", "web-research").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Toolset {
    pub name: String,
    pub tool_names: Vec<String>,
    pub description: String,
}

impl Toolset {
    pub fn new(name: &str, description: &str, tool_names: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            tool_names: tool_names.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[derive(Debug)]
pub enum ToolsetError {
    /// A toolset tried to include a tool that is not registered at all.
    UnknownTool { tool: String },
}

impl std::fmt::Display for ToolsetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolsetError::UnknownTool { tool } => {
                write!(f, "toolset references unregistered tool {tool:?}")
            }
        }
    }
}

impl std::error::Error for ToolsetError {}

/// Validates a toolset against the registered tool names.
pub fn validate_toolset(
    toolset: &Toolset,
    registered: &[String],
) -> Result<(), ToolsetError> {
    for tool in &toolset.tool_names {
        if !registered.contains(tool) {
            return Err(ToolsetError::UnknownTool { tool: tool.clone() });
        }
    }
    Ok(())
}

/// Projects the toolset's visible surface for a session: the INTERSECTION
/// of the toolset with the kernel-authorized tool list. Enablement adds
/// nothing the kernel has not granted (QUAL-EV-0229).
pub fn project_toolset(
    toolset: &Toolset,
    authorized_tools: &[String],
) -> Vec<String> {
    toolset
        .tool_names
        .iter()
        .filter(|t| authorized_tools.contains(t))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered() -> Vec<String> {
        vec![
            "modbit.file.read".into(),
            "modbit.file.write".into(),
            "modbit.shell.run".into(),
            "modbit.web.fetch".into(),
        ]
    }

    /// QUAL-EV-0229: toolset enablement cannot expose a denied tool.
    #[test]
    fn toolset_enablement_cannot_expose_denied_tool() {
        let rust_dev = Toolset::new(
            "rust-dev",
            "everything a rust task needs",
            &[
                "modbit.file.read",
                "modbit.file.write",
                "modbit.shell.run",
            ],
        );
        validate_toolset(&rust_dev, &registered()).unwrap();

        // The session's kernel authorization: read-only + web fetch.
        // NOTE: shell.run and file.write are DENIED for this session.
        let authorized = vec![
            "modbit.file.read".to_string(),
            "modbit.web.fetch".to_string(),
        ];

        let visible = project_toolset(&rust_dev, &authorized);
        assert_eq!(visible, vec!["modbit.file.read".to_string()]);
        assert!(!visible.contains(&"modbit.shell.run".to_string()));
        assert!(!visible.contains(&"modbit.file.write".to_string()));

        // Enabling MORE toolsets still cannot leak denied tools: the
        // intersection stays the intersection.
        let shell_happy = Toolset::new("shell", "shell tools", &["modbit.shell.run"]);
        let both = project_toolset(&rust_dev, &authorized)
            .into_iter()
            .chain(project_toolset(&shell_happy, &authorized));
        assert!(both.all(|t| authorized.contains(&t)));
    }

    /// A toolset referencing an unregistered tool is refused at
    /// validation — the projection never invents tools.
    #[test]
    fn unknown_tool_in_toolset_is_refused() {
        let broken = Toolset::new("broken", "", &["modbit.file.read", "modbit.quantum.fold"]);
        let err = validate_toolset(&broken, &registered()).unwrap_err();
        assert!(matches!(err, ToolsetError::UnknownTool { .. }));
    }
}
