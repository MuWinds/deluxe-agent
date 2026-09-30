//! Native implementations of the harness ports.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::{self, Config};
use crate::context::summarize;
use crate::error::{AgentError, Result};
use crate::llm::{AssistantTurn, LlmClient, Message, StreamFragment, ThinkingLevel, ToolCall};
use crate::plugins::hooks::Hook;
use crate::plugins::{self, PluginCatalogue, PluginSettings};
use crate::tools::jobs::{JobRegistry, JobSnapshot};
use crate::tools::{validate_arguments, ToolDescriptor, ToolRegistry, ToolSettings};

use super::ports::{
    ConfigStore, ContextCompactor, HookContext, HookRuntime, JobFactory, JobRuntime, LlmProvider,
    LlmStreamEvent, LlmStreamSink, PluginManager, PromptProvider, SecretStore, ToolContext,
    ToolExecution, ToolRuntime,
};
use super::types::AuditOutcome;

#[derive(Clone)]
pub struct NativeLlmProvider {
    client: LlmClient,
}

impl NativeLlmProvider {
    /// Wraps the existing OpenAI-compatible client as a runtime provider.
    pub fn new(client: LlmClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl LlmProvider for NativeLlmProvider {
    async fn stream_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        thinking: Option<ThinkingLevel>,
        cancel: &CancellationToken,
        sink: &mut dyn LlmStreamSink,
    ) -> Result<AssistantTurn> {
        self.client
            .stream_turn(messages, tools, thinking, cancel, |fragment| {
                let event = match fragment {
                    StreamFragment::Reset => LlmStreamEvent::Reset,
                    StreamFragment::Reasoning(text) => LlmStreamEvent::Reasoning(text.to_string()),
                    StreamFragment::Content(text) => LlmStreamEvent::Content(text.to_string()),
                };
                sink.push(event);
            })
            .await
    }

    async fn complete_turn(
        &self,
        messages: &[Message],
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn> {
        self.client.complete_turn(messages, cancel).await
    }
}

pub struct NativeContextCompactor {
    llm: Arc<dyn LlmProvider>,
}

impl NativeContextCompactor {
    /// Creates a compactor backed by the same provider as the agent loop.
    pub fn new(llm: Arc<dyn LlmProvider>) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl ContextCompactor for NativeContextCompactor {
    async fn summarize(
        &self,
        history: &[Message],
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        summarize(history, self.llm.as_ref(), cancel).await
    }
}

pub struct NativeJobRuntime {
    jobs: Arc<JobRegistry>,
}

impl NativeJobRuntime {
    /// Adapts the registry shared by the built-in job tools.
    pub fn new(jobs: Arc<JobRegistry>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl JobRuntime for NativeJobRuntime {
    fn list(&self) -> Vec<JobSnapshot> {
        self.jobs.list()
    }

    async fn kill(&self, id: &str, reason: Option<&str>) -> Result<()> {
        self.jobs.kill(id, reason).map(|_| ())
    }

    fn start_result(&self, kind: &'static str, label: String, make: JobFactory) -> String {
        self.jobs.start_result(kind, label, make)
    }

    fn drain_notifications(&self) -> Vec<String> {
        self.jobs.drain_notifications()
    }
}

pub struct NativeConfigStore {
    path: Option<std::path::PathBuf>,
}

impl NativeConfigStore {
    /// Creates a TOML store targeting the config path resolved at startup.
    pub fn new(path: Option<std::path::PathBuf>) -> Self {
        Self { path }
    }
}

#[async_trait]
impl ConfigStore for NativeConfigStore {
    async fn save(&self, config: &Config) -> Result<()> {
        let Some(path) = self.path.clone() else {
            return Err(AgentError::internal(
                "No config directory is available on this system",
            ));
        };
        let config = config.clone();
        tokio::task::spawn_blocking(move || config::save_to(&path, &config))
            .await
            .map_err(|error| AgentError::internal(format!("Config store worker failed: {error}")))?
    }
}

pub struct NativeSecretStore;

impl NativeSecretStore {
    /// Creates a keyring-backed store for model credentials.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SecretStore for NativeSecretStore {
    async fn save_api_key(&self, api_key: &str) -> Result<()> {
        let api_key = api_key.to_string();
        tokio::task::spawn_blocking(move || config::store_api_key(&api_key))
            .await
            .map_err(|error| AgentError::internal(format!("Secret store worker failed: {error}")))?
    }
}

pub struct NativePluginManager {
    home: std::path::PathBuf,
}

impl NativePluginManager {
    /// Creates a manager rooted at the user's plugin installation directory.
    pub fn new(home: std::path::PathBuf) -> Self {
        Self { home }
    }
}

#[async_trait]
impl PluginManager for NativePluginManager {
    async fn discover(
        &self,
        projects: Vec<std::path::PathBuf>,
        settings: PluginSettings,
    ) -> Result<Arc<PluginCatalogue>> {
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || {
            Arc::new(plugins::discover(&home, &projects, &settings))
        })
        .await
        .map_err(|error| AgentError::internal(format!("Plugin discovery worker failed: {error}")))
    }

    async fn uninstall(&self, id: &str, catalogue: Arc<PluginCatalogue>) -> Result<()> {
        let Some(root) = catalogue
            .global()
            .iter()
            .chain(catalogue.disabled())
            .find(|plugin| plugin.id == id)
            .map(|plugin| plugin.root.clone())
        else {
            return Err(AgentError::invalid_params(format!(
                "Plugin `{id}` has no installed location"
            )));
        };

        let cache = self.home.join(".codex").join("plugins").join("cache");
        if !root.starts_with(&cache) {
            return Ok(());
        }

        tokio::task::spawn_blocking(move || {
            std::fs::remove_dir_all(&root)
                .map_err(|error| AgentError::from_io("Failed to uninstall the plugin", error))
        })
        .await
        .map_err(|error| AgentError::internal(format!("Plugin uninstall worker failed: {error}")))?
    }
}

pub struct RegistryToolRuntime {
    registry: Arc<ToolRegistry>,
    settings: Arc<RwLock<ToolSettings>>,
}

impl RegistryToolRuntime {
    /// Moves registry dispatch policy behind the tool runtime port.
    pub fn new(registry: Arc<ToolRegistry>, settings: Arc<RwLock<ToolSettings>>) -> Self {
        Self { registry, settings }
    }

    async fn current_settings(&self, project: &Path) -> ToolSettings {
        let mut settings = self.settings.read().await.clone();
        settings.working_directory = project.to_path_buf();
        settings
    }
}

#[async_trait]
impl ToolRuntime for RegistryToolRuntime {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.registry.descriptors()
    }

    async fn context(&self, project: &Path) -> ToolContext {
        let settings = self.current_settings(project).await;
        ToolContext {
            project: project.to_path_buf(),
            working_directory: settings.working_directory,
            timeout: Duration::from_millis(settings.default_timeout_ms),
            max_output_chars: settings.max_output_chars,
            block_destructive_commands: settings.block_destructive_commands,
        }
    }

    async fn execute(
        &self,
        call: &ToolCall,
        context: &ToolContext,
        cancel: &CancellationToken,
    ) -> Result<ToolExecution> {
        let name = call.function.name.as_str();
        let tool = self.registry.require(name)?;
        let descriptor = tool.descriptor();
        let arguments: Value = serde_json::from_str(&call.function.arguments).map_err(|error| {
            AgentError::invalid_params(format!("Could not parse tool arguments as JSON: {error}"))
        })?;

        if descriptor.host_validates_arguments {
            validate_arguments(&descriptor.input_schema, &arguments)?;
        }

        let mut settings = self.current_settings(&context.project).await;
        settings.working_directory = context.working_directory.clone();
        let timeout = if tool.bounds_own_timeout() {
            const HOST_TIMEOUT_CEILING_MS: u64 = 605_000;
            Duration::from_millis(settings.default_timeout_ms.max(HOST_TIMEOUT_CEILING_MS))
        } else {
            context.timeout
        };

        let output = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AgentError::cancelled()),
            result = tokio::time::timeout(timeout, tool.execute(arguments, &settings)) => {
                match result {
                    Ok(Ok(output)) => output,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(AgentError::timeout(format!(
                            "`{name}` exceeded the {timeout:?} host limit"
                        )));
                    }
                }
            }
        };

        let outcome = if output.is_error {
            AuditOutcome::Failed
        } else {
            AuditOutcome::Executed
        };

        Ok(ToolExecution {
            output: output.truncate_to(context.max_output_chars),
            outcome,
        })
    }
}

pub struct NativeHookRuntime {
    hooks: Vec<Hook>,
    tools: Arc<RegistryToolRuntime>,
}

impl NativeHookRuntime {
    /// Adapts native plugin hooks to the runtime hook port.
    pub fn new(hooks: Vec<Hook>, tools: Arc<RegistryToolRuntime>) -> Self {
        Self { hooks, tools }
    }
}

#[async_trait]
impl HookRuntime for NativeHookRuntime {
    async fn after_tool(
        &self,
        tool: &str,
        output: &mut String,
        context: &HookContext,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let settings = self.tools.context(Path::new(&context.project)).await;
        let hooks: Vec<&Hook> = self
            .hooks
            .iter()
            .filter(|hook| hook.matches(tool))
            .collect();
        for hook in hooks {
            if cancel.is_cancelled() {
                return Ok(());
            }

            let call = ToolCall {
                id: format!("hook-{}", uuid::Uuid::new_v4()),
                call_type: "function".into(),
                function: crate::llm::FunctionCall {
                    name: "exec".into(),
                    arguments: serde_json::json!({
                        "command": hook.command,
                        "cwd": hook.root,
                    })
                    .to_string(),
                },
            };
            let hook_context = ToolContext {
                project: context.project.clone(),
                working_directory: hook.root.clone(),
                timeout: settings.timeout,
                max_output_chars: context.max_output_chars,
                block_destructive_commands: settings.block_destructive_commands,
            };
            let (body, failed) = match self.tools.execute(&call, &hook_context, cancel).await {
                Ok(execution) => (
                    execution.output.as_text(),
                    execution.output.is_error
                        || matches!(
                            execution.outcome,
                            AuditOutcome::Failed | AuditOutcome::Denied
                        ),
                ),
                Err(error) => (format!("could not run: {error}"), true),
            };

            output.push_str("\n\n");
            output.push_str(&format!(
                "PostToolUse hook `{}` (plugin {}){}:\n{}",
                hook.command,
                hook.plugin,
                if failed { " failed" } else { "" },
                body.trim()
            ));
        }
        Ok(())
    }
}

/// Builds native services for a registry and its model provider.
pub fn native_services(
    client: LlmClient,
    registry: Arc<ToolRegistry>,
    settings: Arc<RwLock<ToolSettings>>,
    hooks: Vec<Hook>,
    prompts: Arc<dyn PromptProvider>,
) -> super::ports::AgentServices {
    let llm: Arc<dyn LlmProvider> = Arc::new(NativeLlmProvider::new(client));
    let native_tools = Arc::new(RegistryToolRuntime::new(registry.clone(), settings));
    let tools: Arc<dyn ToolRuntime> = native_tools.clone();
    let jobs: Arc<dyn JobRuntime> = Arc::new(NativeJobRuntime::new(registry.jobs().clone()));
    let context: Arc<dyn ContextCompactor> = Arc::new(NativeContextCompactor::new(llm.clone()));
    let hooks: Arc<dyn HookRuntime> = Arc::new(NativeHookRuntime::new(hooks, native_tools));
    super::ports::AgentServices {
        llm,
        tools,
        context,
        prompts,
        hooks,
        jobs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    #[tokio::test]
    async fn native_config_store_writes_to_its_injected_path() {
        let dir = tempfile::tempdir().expect("a temp directory is available");
        let path = dir.path().join("nested").join("config.toml");
        let store = NativeConfigStore::new(Some(path.clone()));
        let mut config = Config::default();
        config.llm.retry_count = 7;

        store.save(&config).await.expect("the config is saved");

        let text = fs::read_to_string(path).expect("the config file exists");
        let saved: Config = toml::from_str(&text).expect("the config is valid TOML");
        assert_eq!(saved.llm.retry_count, 7);
    }

    #[tokio::test]
    async fn native_config_store_reports_an_unavailable_path() {
        let store = NativeConfigStore::new(None);

        assert!(
            store.save(&Config::default()).await.is_err(),
            "a missing config directory must not be reported as a successful save"
        );
    }

    #[tokio::test]
    async fn native_plugin_manager_only_deletes_cached_plugin_copies() {
        let home = tempfile::tempdir().expect("a temp directory is available");
        let cached = home.path().join(".codex/plugins/cache/test/thing/1.0.0");
        fs::create_dir_all(cached.join(".codex-plugin")).expect("the cache is created");
        fs::write(
            cached.join(".codex-plugin/plugin.json"),
            r#"{"name":"thing","version":"1.0.0"}"#,
        )
        .expect("the manifest is written");
        let mut settings = PluginSettings::default();
        settings.set_enabled("thing@test", true);
        let catalogue = Arc::new(plugins::discover(home.path(), &[], &settings));
        let manager = NativePluginManager::new(home.path().to_path_buf());

        manager
            .uninstall("thing@test", catalogue)
            .await
            .expect("the cached plugin is removed");

        assert!(!cached.exists(), "the Codex cache copy is deleted");
    }

    #[tokio::test]
    async fn native_plugin_manager_preserves_local_working_copies() {
        let home = tempfile::tempdir().expect("a temp directory is available");
        let marketplace = home.path().join(".agents/plugins");
        fs::create_dir_all(&marketplace).expect("the marketplace directory is created");
        fs::write(
            marketplace.join("marketplace.json"),
            r#"{"name":"test","plugins":[{"name":"thing",
                "source":{"source":"local","path":"./plugins/thing"}}]}"#,
        )
        .expect("the marketplace is written");
        let working_copy = home.path().join("plugins/thing");
        fs::create_dir_all(working_copy.join(".codex-plugin"))
            .expect("the working copy is created");
        fs::write(
            working_copy.join(".codex-plugin/plugin.json"),
            r#"{"name":"thing","version":"1.0.0"}"#,
        )
        .expect("the manifest is written");
        let mut settings = PluginSettings::default();
        settings.set_enabled("thing@test", true);
        let catalogue = Arc::new(plugins::discover(home.path(), &[], &settings));
        let manager = NativePluginManager::new(home.path().to_path_buf());

        manager
            .uninstall("thing@test", catalogue)
            .await
            .expect("the working copy is simply disabled by its caller");

        assert!(working_copy.is_dir(), "developer files are left untouched");
    }
}
