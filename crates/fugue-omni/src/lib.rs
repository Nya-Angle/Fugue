//! # fugue-omni
//! Omni runtime and `new_task` meta-tool factory conforming to AXIOM[02] & AXIOM[03].

pub mod system_prompt;
pub use system_prompt::OMNI_SYSTEM_PROMPT;

use std::path::PathBuf;
use fugue_core::{FugueError, Result};
use fugue_kernel::SharedMemory;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRole {
    TaskAgent,
    ExecutionAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSpec {
    pub system_template_id: String,
    pub custom_instructions: String,
    pub context_slices: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub workspace_root: PathBuf,
    pub source_read_only: bool,
    pub artifacts_write_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionScope {
    pub token_budget: usize,
    pub allow_sub_dispatch: bool,
    pub timeout_seconds: u64,
}

/// Arguments for the sole meta-tool `new_task`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTaskArgs {
    pub agent_role: AgentRole,
    pub task_goal: String,
    pub prompt_spec: PromptSpec,
    pub toolset_whitelist: Vec<String>,
    pub sandbox_config: SandboxConfig,
    pub permission_scope: PermissionScope,
}

/// Omni Tool Manifest strictly enforcing Card(ToolManifest) == 1 and Name == "new_task".
#[derive(Debug, Default)]
pub struct OmniToolManifest {
    registered_tools: Vec<String>,
}

impl OmniToolManifest {
    pub fn new() -> Self {
        Self {
            registered_tools: vec!["new_task".to_string()],
        }
    }

    /// Verifies and registers a tool, refusing any tool other than "new_task".
    pub fn register_tool(&mut self, tool_name: &str) -> Result<()> {
        if tool_name != "new_task" {
            return Err(FugueError::ForbiddenTool(format!(
                "AXIOM[03] VIOLATION: Omni cannot register tool '{}'. Manifest strictly limited to 'new_task'",
                tool_name
            )));
        }
        if !self.registered_tools.contains(&tool_name.to_string()) {
            self.registered_tools.push(tool_name.to_string());
        }
        Ok(())
    }

    pub fn tools(&self) -> &[String] {
        &self.registered_tools
    }
}

/// Omni Facade runtime (AXIOM[02]: Sole User-Facing Monopoly).
pub struct OmniNode {
    pub manifest: OmniToolManifest,
    pub memory: SharedMemory,
}

impl OmniNode {
    pub fn new(memory: SharedMemory) -> Self {
        Self {
            manifest: OmniToolManifest::new(),
            memory,
        }
    }

    /// Dispatches a new task via `new_task` meta-tool.
    pub fn compile_and_dispatch(&self, task_id: String, args: NewTaskArgs) -> Result<()> {
        self.memory.add_task(task_id);
        // Store task constraints into IntentStore
        self.memory
            .intent_store
            .write()
            .insert("task_goal".to_string(), args.task_goal);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_axiom_03_omni_tool_purity_enforcement() {
        let mut manifest = OmniToolManifest::new();
        assert_eq!(manifest.tools(), &["new_task".to_string()]);

        // Attempting to add any tool other than "new_task" must strictly fail
        let err = manifest.register_tool("run_bash").expect_err("Must refuse bash");
        match err {
            FugueError::ForbiddenTool(msg) => {
                assert!(msg.contains("AXIOM[03] VIOLATION"));
            }
            _ => panic!("Expected ForbiddenTool error"),
        }

        let err2 = manifest.register_tool("edit_file").expect_err("Must refuse edit_file");
        match err2 {
            FugueError::ForbiddenTool(msg) => {
                assert!(msg.contains("AXIOM[03] VIOLATION"));
            }
            _ => panic!("Expected ForbiddenTool error"),
        }
    }
}

