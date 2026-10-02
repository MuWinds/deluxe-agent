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
        /// The model's own JSON text for the call, kept so the transcript can
        /// replay the exact bytes the run sent instead of re-serialising the
        /// parsed `arguments` and shifting the provider's prompt prefix.
        raw_arguments: String,
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
        /// The turn's provider usage, when it reported one. Carried so the
        /// cache counters can track a multi-turn run instead of jumping once
        /// at the end; the final turn's figure arrives via [`AgentEvent::RunFinished`].
        usage: Option<Usage>,
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
        /// How many trailing messages the summary kept verbatim. The app folds
        /// the marker in at that boundary so a later run replays the same
        /// `[summary, ...tail]` the compacting run continued from.
        keep: usize,
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
