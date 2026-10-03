//! Project-scoped runtime construction.
//!
//! The worker owns lifecycle and caching; this factory owns the concrete
//! assembly of a model provider, Wasmtime Components, delegation, and an agent.
//! Keeping that composition here prevents the IPC loop from becoming another
//! place where runtime dependencies are wired by hand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::RwLock;

use crate::agent::Agent;
use crate::context::ContextSettings;
use crate::error::Result;
use crate::harness::services::native_services;
use crate::harness::{
    AgentEventSink, JobRuntime, NativeJobRuntime, NestedAgentRuntime, PromptContext,
};
use crate::llm::LlmClient;
use crate::plugins::capabilities::CapabilityHub;
use crate::plugins::providers::WasmEventRuntime;
use crate::plugins::wasm::tools;
use crate::plugins::wasm_manifest::Permissions;
use crate::plugins::wasm_runtime::{ComponentActor, Operation};
use crate::plugins::{PluginCatalogue, HOME_DIR};
use crate::tools::{JobRegistry, ToolRegistry, ToolSettings};

use super::nested_agent::NativeNestedAgent;

#[derive(Clone)]
pub struct RuntimeModelSettings {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub context: ContextSettings,
    pub max_output_tokens: Option<u32>,
    pub retry_count: Option<u32>,
    pub supports_images: bool,
}

#[derive(Clone)]
pub struct ProjectRuntime {
    pub agent: Arc<Agent>,
    pub jobs: Arc<dyn JobRuntime>,
    pub host_tools: Arc<dyn crate::harness::ports::ToolRuntime>,
    components: BTreeMap<String, Arc<ComponentActor>>,
}

impl ProjectRuntime {
    /// Stops plugin calls and project jobs before invalidating the cached runtime.
    ///
    /// Individual job cancellation errors are logged so cleanup continues.
    pub async fn shutdown(&self) {
        for component in self.components.values() {
            component.shutdown();
        }
        for job in self
            .jobs
            .list()
            .into_iter()
            .filter(|job| !job.status.is_settled())
        {
            if let Err(error) = self
                .jobs
                .kill(&job.id, Some("project runtime invalidated"))
                .await
            {
                tracing::warn!(%error, "failed to stop a stale runtime job");
            }
        }
    }
}

#[derive(Clone)]
pub struct HostCapabilities {
    pub runtime: Arc<dyn crate::harness::ports::ToolRuntime>,
    pub jobs: Arc<JobRegistry>,
}

/// Live component actors, keyed by project root and plugin id.
type ComponentActors = BTreeMap<(PathBuf, String), Arc<ComponentActor>>;

#[derive(Clone)]
pub struct ProjectRuntimeFactory {
    settings: Arc<RwLock<ToolSettings>>,
    sink: Arc<dyn AgentEventSink>,
    global_configuration_root: PathBuf,
    host_capabilities: Arc<Mutex<BTreeMap<(std::path::PathBuf, bool), HostCapabilities>>>,
    /// One live actor per project and plugin, shared by the agent's tools and
    /// the plugin's UI surface. Without this, a surface opened before the first
    /// run would load a second actor with its own process registry, so a
    /// surface action could not reach the server the agent is using.
    components: Arc<Mutex<ComponentActors>>,
}

impl ProjectRuntimeFactory {
    /// Creates a factory that shares worker tool policy and UI event delivery.
    pub fn new(
        settings: Arc<RwLock<ToolSettings>>,
        sink: Arc<dyn AgentEventSink>,
        global_configuration_root: PathBuf,
    ) -> Self {
        Self {
            settings,
            sink,
            global_configuration_root,
            host_capabilities: Arc::new(Mutex::new(BTreeMap::new())),
            components: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Returns the live component actor for one enabled plugin in a project.
    pub fn component(&self, project: &Path, plugin_id: &str) -> Option<Arc<ComponentActor>> {
        let components = match self.components.lock() {
            Ok(components) => components,
            Err(poisoned) => poisoned.into_inner(),
        };
        components
            .get(&(project.to_path_buf(), plugin_id.to_string()))
            .cloned()
    }

    /// Registers an actor so later runtime builds and surfaces reuse it.
    pub fn cache_component(&self, project: &Path, plugin_id: &str, actor: Arc<ComponentActor>) {
        let mut components = match self.components.lock() {
            Ok(components) => components,
            Err(poisoned) => poisoned.into_inner(),
        };
        components.insert((project.to_path_buf(), plugin_id.to_string()), actor);
    }

    /// Drops every cached actor after their runtimes were shut down.
    pub fn clear_components(&self) {
        let mut components = match self.components.lock() {
            Ok(components) => components,
            Err(poisoned) => poisoned.into_inner(),
        };
        components.clear();
    }

    /// Resolves the global/project configuration root exposed to a Component's
    /// generic file capability.
    pub fn configuration_root(&self, scope: &crate::plugins::Scope, project: &Path) -> PathBuf {
        match scope {
            crate::plugins::Scope::Global => self.global_configuration_root.clone(),
            crate::plugins::Scope::Project(root) if !root.as_os_str().is_empty() => root.clone(),
            crate::plugins::Scope::Project(_) => project.to_path_buf(),
        }
    }

    /// Returns the project-scoped native capabilities used by Components.
    ///
    /// The returned host registry is cached so a UI surface opened before the
    /// agent shares the same job runtime with the later project runtime.
    pub fn host_capabilities(&self, project: &Path, supports_images: bool) -> HostCapabilities {
        let key = (project.to_path_buf(), supports_images);
        let mut cached = match self.host_capabilities.lock() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(capabilities) = cached.get(&key) {
            return capabilities.clone();
        }

        let registry = if supports_images {
            ToolRegistry::with_image_input()
        } else {
            ToolRegistry::with_builtins()
        };
        let jobs = registry.jobs().clone();
        let runtime: Arc<dyn crate::harness::ports::ToolRuntime> =
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(registry),
                self.settings.clone(),
            ));
        let capabilities = HostCapabilities { runtime, jobs };
        cached.insert(key, capabilities.clone());
        capabilities
    }

    /// Builds the worker runtime for one project and model setting.
    ///
    /// A broken Component is logged and skipped; one invalid plugin must
    /// not make the rest of the project's agent unusable.
    pub async fn build(
        &self,
        project: &Path,
        model: &RuntimeModelSettings,
        catalogue: Arc<PluginCatalogue>,
    ) -> Result<Arc<ProjectRuntime>> {
        let client = LlmClient::new(
            &model.base_url,
            &model.model,
            &model.api_key,
            model.max_output_tokens,
            model.retry_count,
        )?;

        let host = self.host_capabilities(project, model.supports_images);
        // The built-ins are native: the agent dispatches `read_file`,
        // `apply_patch` and the rest directly. They share the host job registry
        // so the jobs the agent starts are the ones the UI and a Component's
        // `invoke_tool` observe.
        let mut registry =
            ToolRegistry::with_builtins_sharing(host.jobs.clone(), model.supports_images);

        let plugins = catalogue.for_project(project);
        let capabilities = host.runtime.clone();
        let mut components = BTreeMap::new();
        let mut provider_actors = Vec::new();
        for plugin in &plugins {
            let Some(manifest) = plugin.manifest.wasm_runtime() else {
                continue;
            };
            let loaded = match self.component(project, &plugin.id) {
                Some(actor) => Ok(actor),
                None => {
                    let configuration_root = self.configuration_root(&plugin.scope, project);
                    let hub = CapabilityHub::new(
                        project.to_path_buf(),
                        configuration_root,
                        manifest.permissions.clone(),
                        capabilities.clone(),
                    )?;
                    match ComponentActor::load(plugin.root.clone(), manifest, hub).await {
                        Ok(actor) => {
                            self.cache_component(project, &plugin.id, actor.clone());
                            Ok(actor)
                        }
                        Err(error) => Err(error),
                    }
                }
            };
            match loaded {
                Ok(actor) => {
                    let provider_tools = match tools(actor.clone()).await {
                        Ok(tools) => tools,
                        Err(error) => {
                            tracing::warn!(
                                plugin = %plugin.id,
                                %error,
                                "skipping Wasmtime provider tools"
                            );
                            Vec::new()
                        }
                    };
                    for tool in provider_tools {
                        let name = tool.descriptor().name;
                        if registry.get(&name).is_some() {
                            tracing::warn!(tool = %name, plugin = %plugin.id, "keeping the first tool registration");
                        } else {
                            registry.register(tool);
                        }
                    }
                    provider_actors.push((plugin.id.clone(), actor.clone()));
                    components.insert(plugin.id.clone(), actor);
                }
                Err(error) => {
                    tracing::warn!(plugin = %plugin.id, %error, "skipping a component plugin")
                }
            }
        }

        let plugin_sections = load_prompt_sections(
            &self.global_configuration_root,
            project,
            capabilities.clone(),
        )
        .await;

        let jobs: Arc<dyn JobRuntime> = Arc::new(NativeJobRuntime::new(registry.jobs().clone()));
        // The nested runner is handed the registry *as it stands before the
        // delegating Component registers its tool*, which is what bounds
        // delegation to a single level: a nested agent cannot delegate again.
        let nested: Arc<dyn NestedAgentRuntime> = Arc::new(NativeNestedAgent::new(
            client.clone(),
            Arc::new(registry.clone()),
            self.settings.clone(),
            model.context,
            project.to_path_buf(),
            jobs.clone(),
            self.sink.clone(),
        ));
        load_agents_component(
            project,
            &self.global_configuration_root,
            capabilities.clone(),
            nested,
            &mut registry,
            &mut components,
        )
        .await;

        let registry = Arc::new(registry);
        let mut native = native_services(
            client,
            registry,
            self.settings.clone(),
            Arc::new(super::prompt::NativePromptProvider::new()),
        );
        let events = match WasmEventRuntime::load(provider_actors).await {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(%error, "Wasm event handler providers failed to load");
                WasmEventRuntime::empty()
            }
        };
        native.events = Arc::new(events);
        let services = Arc::new(native);
        let host_tools = capabilities;
        let project_path = project.to_path_buf();
        let project_instructions = tokio::task::spawn_blocking(move || {
            super::prompt::read_project_instructions(&project_path)
        })
        .await
        .map_err(|error| {
            crate::error::AgentError::internal(format!(
                "Project instruction reader failed: {error}"
            ))
        })?;
        let prompt_context = PromptContext {
            tools: services.tools.descriptors(),
            plugin_sections,
            project_instructions,
        };
        let system_prompt = services.prompts.build_system_prompt(&prompt_context)?;
        let agent = Arc::new(Agent::from_services(
            services,
            project.to_path_buf(),
            model.context,
            system_prompt,
        ));

        Ok(Arc::new(ProjectRuntime {
            agent,
            jobs,
            host_tools,
            components,
        }))
    }
}

/// The bundled Component that contributes prompt text for a scope.
///
/// Fixed: it is not a catalogue plugin, so it cannot be disabled and does not
/// appear in the plugins window. The host only binds each
/// instance to one scope root and appends whatever text comes back, so it never
/// learns what the Component reads or how it words its section.
const PROMPT_PROVIDER_COMPONENT: &[u8] = include_bytes!("../../plugin-src/skills/plugin.wasm");

/// The bundled Component that contributes the delegating tool.
///
/// Fixed: it is not a catalogue plugin, so it cannot be disabled and does not
/// appear in the plugins window. It owns the role files
/// and the tool's shape; the host only binds it to one scope root, grants it the
/// nested-agent capability, and registers whatever tool comes back.
const AGENTS_COMPONENT: &[u8] = include_bytes!("../../plugin-src/agents/plugin.wasm");

/// Loads the bundled agents Component and registers the tool it contributes.
///
/// A failure is logged and skipped: delegation is a convenience, and a broken
/// Component must not make the rest of a project's agent unusable.
async fn load_agents_component(
    project: &Path,
    scope_root: &Path,
    capabilities: Arc<dyn crate::harness::ports::ToolRuntime>,
    nested: Arc<dyn NestedAgentRuntime>,
    registry: &mut ToolRegistry,
    components: &mut BTreeMap<String, Arc<ComponentActor>>,
) {
    let hub = match CapabilityHub::new(
        project.to_path_buf(),
        scope_root.to_path_buf(),
        Permissions::default(),
        capabilities,
    ) {
        Ok(hub) => hub.with_nested_agent(nested),
        Err(error) => {
            tracing::warn!(%error, "bundled agents provider failed to start");
            return;
        }
    };
    match ComponentActor::load_bytes(AGENTS_COMPONENT, hub).await {
        Ok(actor) => {
            components.insert("__deluxe_agents".into(), actor.clone());
            match tools(actor).await {
                Ok(tools) => {
                    for tool in tools {
                        let name = tool.descriptor().name;
                        if registry.get(&name).is_some() {
                            tracing::warn!(tool = %name, "keeping the first tool registration");
                        } else {
                            registry.register(tool);
                        }
                    }
                }
                Err(error) => tracing::warn!(%error, "bundled agents provider failed"),
            }
        }
        Err(error) => tracing::warn!(%error, "bundled agents provider failed to load"),
    }
}

/// Collects the prompt text the global and project scopes contribute, through
/// the bundled prompt Component.
///
/// The Component owns the layout, the parsing, and the wording; the host only
/// binds each instance to one scope root and asks for the text. A scope that
/// fails is logged and skipped, so a broken global directory cannot take a
/// project's contribution down with it.
async fn load_prompt_sections(
    global_root: &Path,
    project: &Path,
    capabilities: Arc<dyn crate::harness::ports::ToolRuntime>,
) -> Vec<String> {
    let mut sections = Vec::new();
    let scopes = [
        (global_root.to_path_buf(), "global"),
        (project.join(HOME_DIR), "project"),
    ];
    for (root, scope) in scopes {
        match load_scope_prompt(&root, project, capabilities.clone()).await {
            Ok(text) if !text.trim().is_empty() => sections.push(text),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, scope, "skipping a scope's prompt contribution"),
        }
    }
    sections
}

/// Asks one prompt Component instance for its scope's text.
///
/// The instance is loaded per call and dropped afterwards: it holds no state
/// between reads, so there is nothing to cache.
async fn load_scope_prompt(
    root: &Path,
    project: &Path,
    capabilities: Arc<dyn crate::harness::ports::ToolRuntime>,
) -> Result<String> {
    let hub = CapabilityHub::new(
        project.to_path_buf(),
        root.to_path_buf(),
        Permissions::default(),
        capabilities,
    )?;
    let actor = ComponentActor::load_bytes(PROMPT_PROVIDER_COMPONENT, hub).await?;
    actor.call(Operation::PromptSections).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::harness::AgentEvent;
    use crate::plugins::capabilities::CapabilityHub;
    use crate::tools::{ToolRegistry, ToolSettings};

    struct NoopSink;

    impl AgentEventSink for NoopSink {
        fn emit(&self, _event: AgentEvent) {}
    }

    fn factory() -> ProjectRuntimeFactory {
        ProjectRuntimeFactory::new(
            Arc::new(RwLock::new(ToolSettings::default())),
            Arc::new(NoopSink),
            PathBuf::from("/global"),
        )
    }

    async fn load_actor() -> Arc<ComponentActor> {
        let package = crate::plugins::defaults::PLUGINS
            .iter()
            .find(|package| package.name == "mcp")
            .expect("the application ships a bundled MCP Component");
        let root = tempfile::tempdir().expect("a temporary root is available");
        let hub = CapabilityHub::new(
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            Default::default(),
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .expect("scope host");
        ComponentActor::load_bytes(package.component, hub)
            .await
            .expect("the bundled MCP Component loads")
    }

    #[tokio::test]
    async fn the_bundled_prompt_provider_renders_its_scope() {
        let root = tempfile::tempdir().expect("a temporary scope root is available");
        std::fs::create_dir_all(root.path().join("skills/desktop-ui"))
            .expect("the fixture is writable");
        std::fs::write(
            root.path().join("skills/desktop-ui/SKILL.md"),
            "---\nname: desktop-ui\ndescription: Control Windows apps\n---\n\nBody.\n",
        )
        .expect("the fixture is writable");

        let text = load_scope_prompt(
            root.path(),
            root.path(),
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .await
        .expect("the bundled prompt Component renders a section");

        // The guest joins the bound root with its own layout and normalises the
        // separator, so the path it prints is absolute and forward-slashed.
        let root_slash = root.path().to_string_lossy().replace('\\', "/");
        let expected = format!(
            "{}/skills/desktop-ui/SKILL.md",
            root_slash.trim_end_matches('/')
        );
        assert!(text.contains("<skills>"), "{text}");
        assert!(text.contains("`desktop-ui`"), "{text}");
        assert!(text.contains("Control Windows apps"), "{text}");
        assert!(
            text.contains("do not act on a skill from its summary alone"),
            "the catalog must warn that a summary is not the instructions: {text}"
        );
        assert!(
            text.contains("`read_file`"),
            "the catalog must name the tool that loads the body: {text}"
        );
        assert!(text.contains(&expected), "expected `{expected}` in {text}");
    }

    #[tokio::test]
    async fn a_scope_without_skills_contributes_nothing() {
        let root = tempfile::tempdir().expect("a temporary scope root is available");

        let text = load_scope_prompt(
            root.path(),
            root.path(),
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .await
        .expect("a missing skills directory is not a failure");

        assert!(text.trim().is_empty(), "got {text:?}");
    }

    /// A nested-agent stand-in that records what the Component asked for.
    #[derive(Default)]
    struct RecordingAgent {
        requests: Mutex<Vec<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl crate::harness::NestedAgentRuntime for RecordingAgent {
        async fn run(
            &self,
            request_json: &str,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> crate::error::Result<String> {
            let request: serde_json::Value =
                serde_json::from_str(request_json).expect("the request is JSON");
            let background = request["background"].as_bool().unwrap_or(false);
            self.requests.lock().unwrap().push(request);
            Ok(if background {
                r#"{"jobId":"subagent-9"}"#.to_string()
            } else {
                r#"{"answer":"done"}"#.to_string()
            })
        }
    }

    async fn load_agents(root: &Path, agent: Arc<RecordingAgent>) -> Arc<ComponentActor> {
        let hub = CapabilityHub::new(
            root.to_path_buf(),
            root.to_path_buf(),
            Default::default(),
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .expect("scope host")
        .with_nested_agent(agent);
        ComponentActor::load_bytes(AGENTS_COMPONENT, hub)
            .await
            .expect("the bundled agents Component loads")
    }

    #[tokio::test]
    async fn the_bundled_agents_component_offers_a_task_tool_and_runs_it() {
        let root = tempfile::tempdir().expect("a temporary scope root is available");
        std::fs::create_dir_all(root.path().join("agents")).expect("the fixture is writable");
        std::fs::write(
            root.path().join("agents/example-agent.md"),
            "You are the Example Agent.\n\nTranslate a node into code.\n",
        )
        .expect("the fixture is writable");

        let agent = Arc::new(RecordingAgent::default());
        let actor = load_agents(root.path(), agent.clone()).await;

        let json = actor
            .call(Operation::ListTools)
            .await
            .expect("the Component lists its tool");
        let tools: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("the tool list is JSON");
        assert_eq!(tools.len(), 1, "one `task` tool: {json}");
        assert_eq!(tools[0]["name"], "task");
        let description = tools[0]["description"].as_str().unwrap_or_default();
        assert!(description.contains("`example-agent`"), "{description}");
        assert!(
            description.contains("You are the Example Agent."),
            "{description}"
        );

        let output = actor
            .call(Operation::Execute {
                name: "task".into(),
                arguments: serde_json::json!({
                    "agent": "example-agent",
                    "prompt": "do it",
                })
                .to_string(),
            })
            .await
            .expect("the task tool runs");
        let output: serde_json::Value = serde_json::from_str(&output).expect("output is JSON");
        assert_eq!(output["isError"], false, "{output}");
        assert_eq!(output["content"][0]["text"], "done", "{output}");

        let requests = agent.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["name"], "example-agent");
        assert_eq!(requests[0]["prompt"], "do it");
        assert_eq!(requests[0]["background"], false);
        assert!(
            requests[0]["instructions"]
                .as_str()
                .unwrap_or_default()
                .contains("Example Agent"),
            "the role body is carried whole: {}",
            requests[0]["instructions"]
        );
    }

    #[tokio::test]
    async fn a_background_delegation_returns_the_job_id() {
        let root = tempfile::tempdir().expect("a temporary scope root is available");
        std::fs::create_dir_all(root.path().join("agents")).expect("the fixture is writable");
        std::fs::write(
            root.path().join("agents/worker.md"),
            "You are the worker.\n",
        )
        .expect("the fixture is writable");

        let agent = Arc::new(RecordingAgent::default());
        let actor = load_agents(root.path(), agent.clone()).await;

        let output = actor
            .call(Operation::Execute {
                name: "task".into(),
                arguments: serde_json::json!({
                    "agent": "worker",
                    "prompt": "do it",
                    "runInBackground": true,
                })
                .to_string(),
            })
            .await
            .expect("the task tool runs");
        let output: serde_json::Value = serde_json::from_str(&output).expect("output is JSON");
        assert_eq!(output["isError"], false, "{output}");
        assert!(
            output["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("started background job subagent-9"),
            "{output}"
        );
        assert_eq!(agent.requests.lock().unwrap()[0]["background"], true);
    }

    #[tokio::test]
    async fn a_scope_without_agents_offers_no_tool() {
        let root = tempfile::tempdir().expect("a temporary scope root is available");
        let agent = Arc::new(RecordingAgent::default());
        let actor = load_agents(root.path(), agent).await;

        assert_eq!(
            actor
                .call(Operation::ListTools)
                .await
                .expect("the Component lists its tools"),
            "[]",
            "a missing agents directory contributes no tool"
        );
    }

    #[tokio::test]
    async fn the_component_cache_is_scoped_and_clears() {
        let factory = factory();
        let project = Path::new("/work/repo");
        assert!(factory.component(project, "mcp@deluxe-defaults").is_none());

        let actor = load_actor().await;
        factory.cache_component(project, "mcp@deluxe-defaults", actor.clone());
        let cached = factory
            .component(project, "mcp@deluxe-defaults")
            .expect("the cached actor comes back");
        assert!(
            Arc::ptr_eq(&cached, &actor),
            "the cache returns the same actor, not a copy"
        );
        assert!(
            factory
                .component(Path::new("/other"), "mcp@deluxe-defaults")
                .is_none(),
            "another project must not reuse the actor"
        );

        factory.clear_components();
        assert!(factory.component(project, "mcp@deluxe-defaults").is_none());
    }
}
