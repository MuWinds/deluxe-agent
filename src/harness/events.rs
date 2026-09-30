//! Events emitted by the agent runtime before UI-specific adaptation.

use serde_json::Value;

use crate::attachments::ImageRef;
use crate::llm::Usage;

use super::types::{AuditOutcome, CallId, HunkLines, RunId, ToolName};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    AssistantDelta {
        run_id: RunId,
        text: String,
    },
    AssistantTurnReset {
        run_id: RunId,
    },
    ReasoningDelta {
        run_id: RunId,
        text: String,
    },
    AssistantDone {
        run_id: RunId,
        content: String,
    },
    Notice {
        run_id: RunId,
        text: String,
    },
    ToolStarted {
        run_id: RunId,
        call_id: CallId,
        name: ToolName,
        arguments: Value,
    },
    ToolFinished {
        run_id: RunId,
        call_id: String,
        outcome: AuditOutcome,
        output: String,
        images: Vec<ImageRef>,
        hunks: Vec<HunkLines>,
        duration_ms: u64,
    },
    UsageSampled {
        run_id: RunId,
        measurement: Option<(u64, usize)>,
    },
    RunFinished {
        run_id: RunId,
        usage: Option<Usage>,
        measurement: Option<(u64, usize)>,
    },
    CompactionStarted {
        run_id: RunId,
        dropping: usize,
    },
    Compacted {
        run_id: RunId,
        summary: String,
    },
    RunFailed {
        run_id: RunId,
        message: String,
    },
    SubagentStarted {
        job_id: String,
        agent: String,
        prompt: String,
    },
    Subagent {
        job_id: String,
        event: Box<AgentEvent>,
    },
}
