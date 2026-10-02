//! Wasmtime-backed adapters for generic tools and hooks.
//!
//! Components own their hook semantics. This adapter only validates the
//! Component's JSON metadata and translates it into the harness port used by
//! the agent loop.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{code, AgentError, Result};
use crate::harness::{HookContext, HookRuntime};

use super::ui_protocol::MAX_PAYLOAD_BYTES;
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

/// A hook exported by a Wasmtime Component.
struct WasmHook {
    plugin_id: String,
    descriptor: HookDescriptor,
    actor: Arc<ComponentActor>,
}

/// Runs Component hooks in stable plugin and declaration order.
pub struct WasmHookRuntime {
    hooks: Vec<WasmHook>,
}

impl WasmHookRuntime {
    /// Loads and validates hook declarations from the supplied Components.
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
                    || !valid_identifier(&descriptor.id)
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

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PROVIDER_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
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
