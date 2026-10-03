//! Stable identifiers and small domain values used by the runtime boundary.

use serde::{Deserialize, Serialize};

use crate::tools::ToolDescriptor;

pub type RunId = u64;
pub type CallId = String;
pub type ToolName = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditOutcome {
    Executed,
    Denied,
    Failed,
}

impl AuditOutcome {
    /// The Chinese label shown by the UI for a tool audit result.
    pub fn label(self) -> &'static str {
        match self {
            Self::Executed => "已执行",
            Self::Denied => "已拒绝",
            Self::Failed => "失败",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkLines {
    pub path: String,
    pub lines: Vec<Option<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstruction {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct PromptContext {
    pub tools: Vec<ToolDescriptor>,
    /// Prompt text contributed by Components, in the order they were asked.
    /// Each entry is appended to the system prompt verbatim: the host does not
    /// parse it, so it stays ignorant of what any plugin chose to say.
    pub plugin_sections: Vec<String>,
    pub project_instructions: Vec<ProjectInstruction>,
}
