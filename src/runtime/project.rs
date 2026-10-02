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
    AgentEventSink, AgentRole, JobRuntime, NativeJobRuntime, PromptAgent, PromptContext,
    PromptSkill, SubagentContext,
};
use crate::llm::LlmClient;
use crate::plugins::capabilities::CapabilityHub;
use crate::plugins::providers::WasmHookRuntime;
use crate::plugins::wasm::tools;
use crate::plugins::wasm_runtime::ComponentActor;
use crate::plugins::PluginCatalogue;
use crate::tools::task::Task;
use crate::tools::{JobRegistry, ToolRegistry, ToolSettings};

use super::subagents::NativeSubagentRunner;

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

    /// Returns the component belonging to an enabled plugin in this project.
    pub fn component(&self, plugin_id: &str) -> Option<Arc<ComponentActor>> {
        self.components.get(plugin_id).cloned()
    }
}

#[derive(Clone)]
pub struct HostCapabilities {
    pub runtime: Arc<dyn crate::harness::ports::ToolRuntime>,
    pub jobs: Arc<JobRegistry>,
}

#[derive(Clone)]
pub struct ProjectRuntimeFactory {
    settings: Arc<RwLock<ToolSettings>>,
    sink: Arc<dyn AgentEventSink>,
    global_configuration_root: PathBuf,
    host_capabilities: Arc<Mutex<BTreeMap<(std::path::PathBuf, bool), HostCapabilities>>>,
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
        }
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
        let mut registry = ToolRegistry::empty_with_jobs(host.jobs.clone());

        let plugins = catalogue.for_project(project);
        let capabilities = host.runtime.clone();
        let mut components = BTreeMap::new();
        let mut provider_actors = Vec::new();
        let builtin_manifest = builtin_manifest(model.supports_images)?;
        let builtin_hub = CapabilityHub::new(
            project.to_path_buf(),
            project.to_path_buf(),
            builtin_manifest.permissions.clone(),
            capabilities.clone(),
        )?;
        match ComponentActor::load_bytes(
            include_bytes!("../../plugin-fixtures/builtin-tools/plugin.wasm"),
            builtin_hub,
        )
        .await
        {
            Ok(actor) => {
                components.insert("__deluxe_builtin_tools".into(), actor.clone());
                match tools(actor.clone()).await {
                    Ok(tools) => {
                        for tool in tools {
                            registry.register(tool);
                        }
                    }
                    Err(error) => tracing::warn!(%error, "bundled tool provider failed"),
                }
            }
            Err(error) => tracing::warn!(%error, "bundled tool provider failed to load"),
        }
        for plugin in &plugins {
            let Some(manifest) = plugin.manifest.wasm_runtime() else {
                continue;
            };
            let configuration_root = self.configuration_root(&plugin.scope, project);
            let hub = CapabilityHub::new(
                project.to_path_buf(),
                configuration_root,
                manifest.permissions.clone(),
                capabilities.clone(),
            )?;
            let loaded = async {
                let actor = ComponentActor::load(plugin.root.clone(), manifest, hub).await?;
                Ok::<_, crate::error::AgentError>(actor)
            }
            .await;
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

        let roles: Vec<AgentRole> = plugins
            .iter()
            .flat_map(|plugin| plugin.agents.iter())
            .map(|role| AgentRole {
                name: role.name.clone(),
                description: role.description.clone(),
                instructions: role.instructions.clone(),
                plugin: role.plugin.clone(),
                path: role.path.clone(),
            })
            .collect();

        let jobs: Arc<dyn JobRuntime> = Arc::new(NativeJobRuntime::new(registry.jobs().clone()));
        if !roles.is_empty() {
            let sub_registry = Arc::new(registry.clone());
            let runner = Arc::new(NativeSubagentRunner::new(
                client.clone(),
                sub_registry,
                self.settings.clone(),
            ));
            registry.register(Arc::new(Task::new(
                roles.clone(),
                runner,
                jobs.clone(),
                SubagentContext {
                    project: project.to_path_buf(),
                    context_settings: model.context,
                },
                self.sink.clone(),
            )));
        }

        let registry = Arc::new(registry);
        let mut native = native_services(
            client,
            registry,
            self.settings.clone(),
            Arc::new(super::prompt::NativePromptProvider::new()),
        );
        let hooks = match WasmHookRuntime::load(provider_actors).await {
            Ok(hooks) => hooks,
            Err(error) => {
                tracing::warn!(%error, "Wasm hook providers failed to load");
                WasmHookRuntime::empty()
            }
        };
        native.hooks = Arc::new(hooks);
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
            skills: plugins
                .iter()
                .flat_map(|plugin| plugin.skills.iter())
                .map(|skill| PromptSkill {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    path: skill.path.clone(),
                    plugin: skill.plugin.clone(),
                })
                .collect(),
            agents: if services
                .tools
                .descriptors()
                .iter()
                .any(|tool| tool.name == "task")
            {
                roles
                    .iter()
                    .map(|role| PromptAgent {
                        name: role.name.clone(),
                        description: role.description.clone(),
                        plugin: role.plugin.clone(),
                    })
                    .collect()
            } else {
                Vec::new()
            },
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

fn builtin_manifest(supports_images: bool) -> Result<crate::plugins::wasm_manifest::WasmManifest> {
    let mut invoke_tools = vec![
        "read_file".to_string(),
        "list_dir".to_string(),
        "exec".to_string(),
        "apply_patch".to_string(),
        "job_output".to_string(),
        "job_list".to_string(),
        "job_kill".to_string(),
    ];
    if supports_images {
        invoke_tools.push("read_image".to_string());
    }
    serde_json::from_value(serde_json::json!({
        "type": "wasm",
        "module": "builtin-tools.wasm",
        "apiVersion": crate::plugins::wasm_manifest::API_VERSION,
        "permissions": { "invokeTools": invoke_tools }
    }))
    .map_err(|error| {
        crate::error::AgentError::internal(format!("Build bundled provider manifest: {error}"))
    })
}
