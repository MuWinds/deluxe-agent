//! Compatibility conversion from runtime events to the legacy UI protocol.

use crate::ipc::Event;

use super::events::AgentEvent;
impl From<AgentEvent> for Event {
    fn from(event: AgentEvent) -> Self {
        match event {
            AgentEvent::AssistantDelta { run_id, text } => Self::AssistantDelta { run_id, text },
            AgentEvent::AssistantTurnReset { run_id } => Self::AssistantTurnReset { run_id },
            AgentEvent::ReasoningDelta { run_id, text } => Self::ReasoningDelta { run_id, text },
            AgentEvent::AssistantDone { run_id, content } => {
                Self::AssistantDone { run_id, content }
            }
            AgentEvent::Notice { run_id, text } => Self::Notice { run_id, text },
            AgentEvent::ToolStarted {
                run_id,
                call_id,
                name,
                arguments,
                raw_arguments,
            } => Self::ToolStarted {
                run_id,
                call_id,
                name,
                arguments,
                raw_arguments,
            },
            AgentEvent::ToolFinished {
                run_id,
                call_id,
                outcome,
                output,
                images,
                hunks,
                duration_ms,
            } => Self::ToolFinished {
                run_id,
                call_id,
                outcome,
                output,
                images,
                hunks,
                duration_ms,
            },
            AgentEvent::UsageSampled {
                run_id,
                measurement,
                usage,
            } => Self::UsageSampled {
                run_id,
                measurement,
                usage,
            },
            AgentEvent::RunFinished {
                run_id,
                usage,
                measurement,
            } => Self::RunFinished {
                run_id,
                usage,
                measurement,
            },
            AgentEvent::CompactionStarted { run_id, dropping } => {
                Self::CompactionStarted { run_id, dropping }
            }
            AgentEvent::Compacted {
                run_id,
                summary,
                keep,
            } => Self::Compacted {
                run_id,
                summary,
                keep,
            },
            AgentEvent::RunFailed { run_id, message } => Self::RunFailed { run_id, message },
            AgentEvent::SubagentStarted {
                job_id,
                agent,
                prompt,
            } => Self::SubagentStarted {
                job_id,
                agent,
                prompt,
            },
            AgentEvent::Subagent { job_id, event } => Self::Subagent {
                job_id,
                event: Box::new((*event).into()),
            },
        }
    }
}
