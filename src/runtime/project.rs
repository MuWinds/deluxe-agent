//! Project-scoped runtime construction.
//!
//! The worker owns lifecycle and caching; this factory owns the concrete
//! assembly of a model provider, tools, MCP clients, delegation, and an agent.
//! Keeping that composition here prevents the IPC loop from becoming another
//! place where runtime dependencies are wired by hand.

use std::path::Path;
use std::sync::Arc;

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
use crate::mcp::{tool::McpTool, McpClient};
use crate::plugins::{LoadedPlugin, PluginCatalogue};
use crate::tools::task::Task;
use crate::tools::{ToolRegistry, ToolSettings};

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
}

#[derive(Clone)]
pub struct ProjectRuntimeFactory {
    settings: Arc<RwLock<ToolSettings>>,
    sink: Arc<dyn AgentEventSink>,
}

impl ProjectRuntimeFactory {
    /// Creates a factory that shares worker tool policy and UI event delivery.
    pub fn new(settings: Arc<RwLock<ToolSettings>>, sink: Arc<dyn AgentEventSink>) -> Self {
        Self { settings, sink }
    }

    /// Builds the complete native runtime for one project and model setting.
    ///
    /// MCP handshake failures are logged and skipped by design; one broken
    /// plugin must not make the rest of the project's agent unusable.
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

        let mut registry = if model.supports_images {
            ToolRegistry::with_image_input()
        } else {
            ToolRegistry::with_builtins()
        };

        let plugins = catalogue.for_project(project);
        register_mcp_tools(&mut registry, &plugins).await;

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

        let hooks = plugins
            .iter()
            .flat_map(|plugin| plugin.hooks.iter().cloned())
            .collect();
        let registry = Arc::new(registry);
        let services = Arc::new(native_services(
            client,
            registry,
            self.settings.clone(),
            hooks,
            Arc::new(super::prompt::NativePromptProvider::new()),
        ));
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

        Ok(Arc::new(ProjectRuntime { agent, jobs }))
    }
}

async fn register_mcp_tools(registry: &mut ToolRegistry, plugins: &[&LoadedPlugin]) {
    for plugin in plugins {
        for (server, config) in &plugin.mcp_servers {
            let mut client = match McpClient::connect(server, config, &plugin.root).await {
                Ok(client) => client,
                Err(error) => {
                    tracing::warn!(server, plugin = %plugin.id, %error, "skipping an MCP server");
                    continue;
                }
            };

            let specs = match client.list_tools().await {
                Ok(specs) => specs,
                Err(error) => {
                    tracing::warn!(server, plugin = %plugin.id, %error, "skipping an MCP server");
                    continue;
                }
            };

            let client = crate::mcp::shared(client);
            for spec in specs {
                let tool = McpTool::new(server, spec, client.clone());
                let name = tool.name().to_string();
                if registry.get(&name).is_some() {
                    tracing::warn!(
                        tool = %name,
                        server,
                        "another tool already has this name; keeping the first"
                    );
                    continue;
                }

                tracing::info!(tool = %name, server, plugin = %plugin.id, "registered an MCP tool");
                registry.register(Arc::new(tool));
            }
        }
    }
}
