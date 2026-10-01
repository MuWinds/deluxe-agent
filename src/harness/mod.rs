//! Neutral runtime contracts shared by the agent loop and native adapters.
//!
//! The harness deliberately does not know about the GUI, IPC, plugin
//! discovery, or MCP transport. Those concerns adapt to these contracts at
//! the composition boundary.

pub mod events;
mod ipc;
pub mod ports;
pub mod services;
pub mod types;

pub use events::AgentEvent;
pub use ports::{
    AgentEventSink, AgentServices, ConfigStore, HookContext, HookRuntime, JobRuntime, LlmProvider,
    LlmStreamEvent, LlmStreamSink, PluginManager, SecretStore, SessionStore, SubagentContext,
    SubagentRunner,
};
pub use services::{NativeConfigStore, NativeJobRuntime, NativePluginManager, NativeSecretStore};
pub use types::{
    AgentRole, AuditOutcome, HunkLines, ProjectInstruction, PromptAgent, PromptContext,
    PromptSkill, RunId, RunState,
};
