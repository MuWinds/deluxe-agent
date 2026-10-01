//! Wasmtime-backed provider adapters for tools, hooks, and MCP.
//!
//! The component owns provider semantics. These adapters only validate the
//! provider's JSON metadata and translate it into the harness ports used by the
//! agent loop.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{code, AgentError, Result};
use crate::harness::{HookContext, HookRuntime};
use crate::tools::{Tool, ToolDescriptor, ToolOutput, ToolSettings};

use super::ui_protocol::MAX_PAYLOAD_BYTES;
use super::wasm::{decode_tool_output, tools};
use super::wasm_runtime::{ComponentActor, Operation};

const MAX_PROVIDER_ITEMS: usize = 128;
const MAX_PROVIDER_ID_BYTES: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookDescriptor {
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookOutput {
    output: String,
    #[serde(default)]
    failed: bool,
    #[serde(default = "default_hook_matched")]
    matched: bool,
}

fn default_hook_matched() -> bool {
    true
}

/// A hook exported by a Wasm provider.
struct WasmHook {
    plugin_id: String,
    descriptor: HookDescriptor,
    actor: Arc<ComponentActor>,
}

/// Runs provider hooks in stable plugin and declaration order.
pub struct WasmHookRuntime {
    hooks: Vec<WasmHook>,
}

impl WasmHookRuntime {
    /// Loads and validates hook declarations from the supplied providers.
    pub async fn load(
        providers: impl IntoIterator<Item = (String, Arc<ComponentActor>)>,
    ) -> Result<Self> {
        let mut hooks = Vec::new();
        for (plugin_id, actor) in providers {
            let json = match actor.call(Operation::ListHooks).await {
                Ok(json) => json,
                Err(error) => {
                    tracing::warn!(plugin = %plugin_id, %error, "skipping Wasm hook provider");
                    continue;
                }
            };
            let descriptors: Vec<HookDescriptor> = match decode_list(&json, "hook") {
                Ok(descriptors) => descriptors,
                Err(error) => {
                    tracing::warn!(plugin = %plugin_id, %error, "skipping invalid Wasm hook declarations");
                    continue;
                }
            };
            if descriptors.len() > MAX_PROVIDER_ITEMS {
                tracing::warn!(plugin = %plugin_id, "skipping provider with too many hook declarations");
                continue;
            }
            let mut provider_hooks = Vec::new();
            for descriptor in descriptors {
                if descriptor.id.len() > MAX_PROVIDER_ID_BYTES
                    || descriptor.tools.len() > MAX_PROVIDER_ITEMS
                    || descriptor.tools.iter().any(|tool| tool.is_empty())
                    || validate_identifier(&descriptor.id).is_err()
                {
                    tracing::warn!(
                        plugin = %plugin_id,
                        "skipping invalid Wasm hook declaration"
                    );
                    continue;
                }
                provider_hooks.push(WasmHook {
                    plugin_id: plugin_id.clone(),
                    descriptor,
                    actor: actor.clone(),
                });
            }
            hooks.extend(provider_hooks);
        }
        hooks.sort_by(|left, right| {
            left.plugin_id
                .cmp(&right.plugin_id)
                .then_with(|| left.descriptor.id.cmp(&right.descriptor.id))
        });
        Ok(Self { hooks })
    }

    /// Creates a runtime that ignores all hook events.
    pub fn empty() -> Self {
        Self { hooks: Vec::new() }
    }
}

#[async_trait]
impl HookRuntime for WasmHookRuntime {
    async fn after_tool(
        &self,
        tool: &str,
        output: &mut String,
        context: &HookContext,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let event = serde_json::json!({
            "tool": tool,
            "output": output,
            "project": context.project,
            "maxOutputChars": context.max_output_chars,
        })
        .to_string();
        if event.len() > MAX_PAYLOAD_BYTES {
            return Err(AgentError::new(
                code::PLUGIN_RESOURCE_LIMIT,
                "Hook event exceeds the provider payload limit",
            ));
        }

        for hook in &self.hooks {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if !hook.descriptor.tools.is_empty()
                && !hook
                    .descriptor
                    .tools
                    .iter()
                    .any(|name| name == tool || name == "*")
            {
                continue;
            }

            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                result = hook.actor.call(Operation::InvokeHook {
                    id: hook.descriptor.id.clone(),
                    event: event.clone(),
                }) => result,
            };
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    output.push_str(&format!(
                        "\n\nPostToolUse hook `{}` (plugin {}) failed:\n{}",
                        hook.descriptor.label_or_id(),
                        hook.plugin_id,
                        error
                    ));
                    continue;
                }
            };
            let hook_output: HookOutput = decode_json(&result, "hook output")?;
            if !hook_output.matched {
                continue;
            }
            if hook_output.output.len() > MAX_PAYLOAD_BYTES {
                return Err(AgentError::new(
                    code::PLUGIN_INVALID_OUTPUT,
                    "Hook output exceeds the provider payload limit",
                ));
            }
            output.push_str("\n\n");
            output.push_str(&format!(
                "PostToolUse hook `{}` (plugin {}){}:\n{}",
                hook.descriptor.label_or_id(),
                hook.plugin_id,
                if hook_output.failed { " failed" } else { "" },
                hook_output.output.trim()
            ));
        }
        Ok(())
    }
}

impl HookDescriptor {
    fn label_or_id(&self) -> &str {
        if self.label.is_empty() {
            &self.id
        } else {
            &self.label
        }
    }
}

/// Loads all MCP tools exported by one provider.
pub async fn mcp_tools(
    actor: Arc<ComponentActor>,
    provider_id: &str,
) -> Result<Vec<Arc<dyn Tool>>> {
    let servers_json = actor.call(Operation::ListMcpServers).await?;
    let servers: Vec<String> = decode_list(&servers_json, "MCP server")?;
    if servers.len() > MAX_PROVIDER_ITEMS {
        return Err(invalid_provider("Too many MCP servers"));
    }

    let mut result = Vec::new();
    for server in servers {
        validate_identifier(&server)?;
        let tools_json = actor
            .call(Operation::ListMcpTools {
                server: server.clone(),
            })
            .await?;
        let listed: Vec<ToolDescriptor> = decode_list(&tools_json, "MCP tool")?;
        if listed.len() > MAX_PROVIDER_ITEMS {
            return Err(invalid_provider("Too many MCP tools"));
        }
        for descriptor in listed {
            validate_tool_descriptor(&descriptor)?;
            result.push(Arc::new(WasmMcpTool {
                provider_id: provider_id.to_string(),
                server: server.clone(),
                remote_name: descriptor.name.clone(),
                descriptor: exposed_descriptor(&server, descriptor),
                actor: actor.clone(),
            }) as Arc<dyn Tool>);
        }
    }
    Ok(result)
}

/// Loads regular tools and provider-owned MCP tools from one component.
pub async fn all_tools(
    actor: Arc<ComponentActor>,
    provider_id: &str,
) -> Result<Vec<Arc<dyn Tool>>> {
    let mut result = tools(actor.clone()).await?;
    match mcp_tools(actor, provider_id).await {
        Ok(mcp) => result.extend(mcp),
        Err(error) => {
            tracing::warn!(plugin = %provider_id, %error, "skipping Wasm MCP tools");
        }
    }
    Ok(result)
}

struct WasmMcpTool {
    provider_id: String,
    server: String,
    remote_name: String,
    descriptor: ToolDescriptor,
    actor: Arc<ComponentActor>,
}

#[async_trait]
impl Tool for WasmMcpTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(&self, arguments: Value, _settings: &ToolSettings) -> Result<ToolOutput> {
        let arguments = arguments.to_string();
        if arguments.len() > MAX_PAYLOAD_BYTES {
            return Err(AgentError::invalid_params(
                "MCP arguments exceed the provider payload limit",
            ));
        }
        let result = self
            .actor
            .call(Operation::InvokeMcp {
                server: self.server.clone(),
                name: self.remote_name.clone(),
                arguments,
            })
            .await?;
        decode_tool_output(&result).map_err(|error| {
            tracing::warn!(plugin = %self.provider_id, server = %self.server, %error, "invalid provider MCP output");
            error
        })
    }
}

fn validate_tool_descriptor(descriptor: &ToolDescriptor) -> Result<()> {
    if descriptor.name.is_empty()
        || descriptor.name.len() > MAX_PROVIDER_ID_BYTES
        || !descriptor
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        || descriptor.input_schema.schema_type != "object"
    {
        return Err(invalid_provider("Invalid provider MCP tool descriptor"));
    }
    if descriptor
        .input_schema
        .required
        .iter()
        .any(|key| !descriptor.input_schema.properties.contains_key(key))
    {
        return Err(invalid_provider(
            "Provider MCP schema has an unknown required field",
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_PROVIDER_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(invalid_provider("Provider identifier is invalid"));
    }
    Ok(())
}

fn decode_list<T: for<'de> Deserialize<'de>>(json: &str, kind: &str) -> Result<T> {
    let value: Value = decode_json(json, kind)?;
    if !value.is_array() {
        return Err(invalid_provider(format!(
            "Provider {kind} list is not an array"
        )));
    }
    serde_json::from_value(value)
        .map_err(|_| invalid_provider(format!("Provider {kind} list is malformed")))
}

fn decode_json<T: for<'de> Deserialize<'de>>(json: &str, kind: &str) -> Result<T> {
    if json.len() > MAX_PAYLOAD_BYTES {
        return Err(AgentError::new(
            code::PLUGIN_INVALID_OUTPUT,
            format!("Provider {kind} exceeds the payload limit"),
        ));
    }
    serde_json::from_str(json)
        .map_err(|_| invalid_provider(format!("Provider {kind} is malformed")))
}

fn invalid_provider(message: impl Into<String>) -> AgentError {
    AgentError::new(code::PLUGIN_INVALID_OUTPUT, message)
}

fn exposed_descriptor(server: &str, mut descriptor: ToolDescriptor) -> ToolDescriptor {
    descriptor.name = format!("mcp__{server}__{}", descriptor.name);
    descriptor
}
