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
use super::types::{AgentRole, AuditOutcome, PromptContext};

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

    /// Builds the system prompt for one delegated role.
    fn build_role_prompt(&self, role: &AgentRole, tools: &[ToolDescriptor]) -> Result<String>;

    /// Converts descriptors into the provider-specific tool request schema.
    fn tool_schema(&self, tools: &[ToolDescriptor]) -> Value;
}

#[derive(Debug, Clone)]
pub struct HookContext {
    pub project: PathBuf,
    pub max_output_chars: usize,
}

#[async_trait]
pub trait HookRuntime: Send + Sync {
    /// Runs matching post-tool hooks and appends their visible output.
    async fn after_tool(
        &self,
        tool: &str,
        output: &mut String,
        context: &HookContext,
        cancel: &CancellationToken,
    ) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct SubagentContext {
    pub project: PathBuf,
    pub context_settings: crate::context::ContextSettings,
}

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    /// Runs one plugin role and streams its runtime events to `sink`.
    async fn run_role(
        &self,
        role: &AgentRole,
        prompt: String,
        context: SubagentContext,
        sink: Arc<dyn AgentEventSink>,
        cancel: CancellationToken,
    ) -> Result<String>;
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
    /// Discovers the catalogue for the supplied project and trust settings.
    async fn discover(
        &self,
        projects: Vec<PathBuf>,
        settings: PluginSettings,
    ) -> Result<Arc<PluginCatalogue>>;

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
    pub hooks: Arc<dyn HookRuntime>,
    pub jobs: Arc<dyn JobRuntime>,
}
