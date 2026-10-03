//! External capabilities consumed by the agent runtime.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::Result;
use crate::llm::{AssistantTurn, Message, ThinkingLevel, ToolCall};
use crate::plugins::{PluginCatalogue, PluginSettings, Scope};
use crate::session::Session;
use crate::tools::jobs::JobSnapshot;
use crate::tools::{ToolDescriptor, ToolOutput};

use super::events::AgentEvent;
use super::types::{AuditOutcome, PromptContext};

#[derive(Debug, Clone)]
pub enum LlmStreamEvent {
    Reset,
    Reasoning(String),
    Content(String),
}

pub trait LlmStreamSink: Send {
    /// Receives one provider stream event without performing blocking work.
    fn push(&mut self, event: LlmStreamEvent);
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Streams one assistant turn and forwards provider fragments to `sink`.
    async fn stream_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        thinking: Option<ThinkingLevel>,
        cancel: &CancellationToken,
        sink: &mut dyn LlmStreamSink,
    ) -> Result<AssistantTurn>;

    /// Completes one non-streaming assistant turn.
    ///
    /// `tools` is the conversation's own schema, sent so a summarisation
    /// request keeps the cached prefix it shares with the turns around it.
    async fn complete_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn>;
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub project: PathBuf,
    pub working_directory: PathBuf,
    pub timeout: Duration,
    pub max_output_chars: usize,
}

#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub output: ToolOutput,
    pub outcome: AuditOutcome,
}

#[async_trait]
pub trait ToolRuntime: Send + Sync {
    /// Returns the descriptors exposed to the prompt and model request.
    fn descriptors(&self) -> Vec<ToolDescriptor>;

    /// Returns the current host policy for a project.
    async fn context(&self, project: &Path) -> ToolContext;

    /// Validates and executes one model tool call.
    async fn execute(
        &self,
        call: &ToolCall,
        context: &ToolContext,
        cancel: &CancellationToken,
    ) -> Result<ToolExecution>;
}

#[async_trait]
pub trait ContextCompactor: Send + Sync {
    /// Summarises history, returning `None` when it contains no useful text.
    ///
    /// `tools` is passed through to the completion so the summarisation request
    /// reuses the conversation's cached prefix.
    async fn summarize(
        &self,
        history: &[Message],
        tools: &Value,
        cancel: &CancellationToken,
    ) -> Result<Option<String>>;
}

pub trait PromptProvider: Send + Sync {
    /// Builds the stable root-agent system prompt from a snapshot.
    fn build_system_prompt(&self, context: &PromptContext) -> Result<String>;

    /// Converts descriptors into the provider-specific tool request schema.
    fn tool_schema(&self, tools: &[ToolDescriptor]) -> Value;
}

/// The only event kind the agent loop emits today.
pub const EVENT_TOOL_FINISHED: &str = "tool.finished";

/// A host event delivered to every subscribed plugin.
#[derive(Debug, Clone)]
pub struct PluginEvent {
    pub kind: &'static str,
    pub project: PathBuf,
    pub max_output_chars: usize,
    /// Kind-specific fields, serialised to the plugin as-is.
    pub payload: Value,
}

#[async_trait]
pub trait PluginEventRuntime: Send + Sync {
    /// Delivers `event` to every subscribed handler and returns the text they
    /// contribute, in stable plugin and handler order. An empty string means no
    /// handler contributed.
    async fn dispatch(&self, event: &PluginEvent, cancel: &CancellationToken) -> Result<String>;
}

/// Runs one nested agent loop on a Component's behalf.
///
/// The host owns the model client, the tool set, and the event stream, so a
/// Component that delegates work only hands over instructions and a prompt.
#[async_trait]
pub trait NestedAgentRuntime: Send + Sync {
    /// Runs one nested agent and returns its answer as a JSON object.
    ///
    /// `request_json` carries the role's name and instructions, the prompt, and
    /// whether the run is detached. The reply is `{"answer": ...}` for a
    /// foreground run and `{"jobId": ...}` for a detached one.
    async fn run(&self, request_json: &str, cancel: &CancellationToken) -> Result<String>;
}

pub type JobFuture = Pin<Box<dyn Future<Output = Result<String>> + Send + 'static>>;
pub type JobFactory = Box<dyn FnOnce(String, CancellationToken) -> JobFuture + Send + 'static>;

#[async_trait]
pub trait JobRuntime: Send + Sync {
    /// Lists jobs in their registration order.
    fn list(&self) -> Vec<JobSnapshot>;

    /// Requests cancellation of one job.
    async fn kill(&self, id: &str, reason: Option<&str>) -> Result<()>;

    /// Starts a result-producing job and returns its stable id.
    fn start_result(&self, kind: &'static str, label: String, make: JobFactory) -> String;

    /// Removes completion notices already observed by the agent loop.
    fn drain_notifications(&self) -> Vec<String>;
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Loads persisted sessions, returning an empty list when no store exists.
    async fn load(&self) -> Result<Vec<Session>>;

    /// Persists a snapshot of sessions without blocking the caller's runtime.
    async fn save(&self, sessions: &[Session]) -> Result<()>;
}

#[async_trait]
pub trait ConfigStore: Send + Sync {
    /// Persists the application configuration.
    async fn save(&self, config: &Config) -> Result<()>;
}

#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Persists the model credential in the operating system's secret store.
    async fn save_api_key(&self, api_key: &str) -> Result<()>;
}

#[async_trait]
pub trait PluginManager: Send + Sync {
    /// Discovers the catalogue for the supplied trust settings.
    async fn discover(&self, settings: PluginSettings) -> Result<Arc<PluginCatalogue>>;

    /// Installs embedded defaults and returns the resulting plugin settings.
    async fn ensure_bundled_defaults(&self, settings: PluginSettings) -> Result<PluginSettings>;

    /// Imports a Wasmtime component or its plugin root into the managed cache.
    async fn install_local(&self, component_path: PathBuf) -> Result<InstalledPlugin>;

    /// Removes a cache directory created by an import that failed afterward.
    async fn discard_install(&self, root: PathBuf) -> Result<()>;

    /// Removes a cached plugin copy, leaving local and bundled sources intact.
    async fn uninstall(
        &self,
        id: &str,
        scope: &Scope,
        catalogue: Arc<PluginCatalogue>,
    ) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub id: String,
    pub root: PathBuf,
    pub copied: bool,
}

pub trait AgentEventSink: Send + Sync {
    /// Publishes a runtime event without blocking the worker.
    fn emit(&self, event: AgentEvent);
}

pub struct AgentServices {
    pub llm: Arc<dyn LlmProvider>,
    pub tools: Arc<dyn ToolRuntime>,
    pub context: Arc<dyn ContextCompactor>,
    pub prompts: Arc<dyn PromptProvider>,
    pub events: Arc<dyn PluginEventRuntime>,
    pub jobs: Arc<dyn JobRuntime>,
}
