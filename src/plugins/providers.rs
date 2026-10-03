//! Wasmtime-backed adapters for generic tools and plugin events.
//!
//! Components own their event semantics. This adapter only validates the
//! Component's JSON metadata and translates it into the harness port used by
//! the agent loop.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{code, AgentError, Result};
use crate::harness::{PluginEvent, PluginEventRuntime};

use super::wasm_runtime::{ComponentActor, Operation};

const MAX_PROVIDER_ITEMS: usize = 128;
const MAX_PROVIDER_ID_BYTES: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventHandlerDescriptor {
    id: String,
    #[serde(default)]
    label: String,
    /// Event kinds this handler subscribes to. Empty means every kind.
    #[serde(default)]
    events: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventHandlerOutput {
    output: String,
    #[serde(default)]
    failed: bool,
    #[serde(default = "default_event_matched")]
    matched: bool,
}

fn default_event_matched() -> bool {
    true
}

/// An event handler exported by a Wasmtime Component.
struct WasmEventHandler {
    plugin_id: String,
    descriptor: EventHandlerDescriptor,
    actor: Arc<ComponentActor>,
}

/// Delivers plugin events to Component handlers in stable plugin and declaration order.
pub struct WasmEventRuntime {
    handlers: Vec<WasmEventHandler>,
}

impl WasmEventRuntime {
    /// Loads and validates event handler declarations from the supplied Components.
    pub async fn load(
        providers: impl IntoIterator<Item = (String, Arc<ComponentActor>)>,
    ) -> Result<Self> {
        let mut handlers = Vec::new();
        for (plugin_id, actor) in providers {
            let json = match actor.call(Operation::ListEventHandlers).await {
                Ok(json) => json,
                Err(error) => {
                    tracing::warn!(plugin = %plugin_id, %error, "skipping Wasm event handler provider");
                    continue;
                }
            };
            let descriptors: Vec<EventHandlerDescriptor> = match decode_list(&json, "event handler")
            {
                Ok(descriptors) => descriptors,
                Err(error) => {
                    tracing::warn!(plugin = %plugin_id, %error, "skipping invalid Wasm event handler declarations");
                    continue;
                }
            };
            if descriptors.len() > MAX_PROVIDER_ITEMS {
                tracing::warn!(plugin = %plugin_id, "skipping provider with too many event handler declarations");
                continue;
            }
            let mut provider_handlers = Vec::new();
            for descriptor in descriptors {
                if descriptor.id.len() > MAX_PROVIDER_ID_BYTES
                    || descriptor.events.len() > MAX_PROVIDER_ITEMS
                    || descriptor
                        .events
                        .iter()
                        .any(|event| event.is_empty() || event.len() > MAX_PROVIDER_ID_BYTES)
                    || !valid_identifier(&descriptor.id)
                {
                    tracing::warn!(
                        plugin = %plugin_id,
                        "skipping invalid Wasm event handler declaration"
                    );
                    continue;
                }
                provider_handlers.push(WasmEventHandler {
                    plugin_id: plugin_id.clone(),
                    descriptor,
                    actor: actor.clone(),
                });
            }
            handlers.extend(provider_handlers);
        }
        handlers.sort_by(|left, right| {
            left.plugin_id
                .cmp(&right.plugin_id)
                .then_with(|| left.descriptor.id.cmp(&right.descriptor.id))
        });
        Ok(Self { handlers })
    }

    /// Creates a runtime that ignores all plugin events.
    pub fn empty() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }
}

#[async_trait]
impl PluginEventRuntime for WasmEventRuntime {
    async fn dispatch(&self, event: &PluginEvent, cancel: &CancellationToken) -> Result<String> {
        let event_json = serde_json::json!({
            "kind": event.kind,
            "project": event.project,
            "maxOutputChars": event.max_output_chars,
            "payload": event.payload,
        })
        .to_string();

        let mut contributed = String::new();
        for handler in &self.handlers {
            if cancel.is_cancelled() {
                return Ok(contributed);
            }
            if !handler.descriptor.events.is_empty()
                && !handler
                    .descriptor
                    .events
                    .iter()
                    .any(|kind| kind == event.kind)
            {
                continue;
            }

            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(contributed),
                result = handler.actor.call(Operation::HandleEvent {
                    id: handler.descriptor.id.clone(),
                    event: event_json.clone(),
                }) => result,
            };
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    contributed.push_str(&format!(
                        "\n\nPlugin event handler `{}` (plugin {}) failed:\n{}",
                        handler.descriptor.label_or_id(),
                        handler.plugin_id,
                        error
                    ));
                    continue;
                }
            };
            let handler_output: EventHandlerOutput = decode_json(&result, "event handler output")?;
            if !handler_output.matched {
                continue;
            }
            contributed.push_str("\n\n");
            contributed.push_str(&format!(
                "Plugin event handler `{}` (plugin {}){}:\n{}",
                handler.descriptor.label_or_id(),
                handler.plugin_id,
                if handler_output.failed { " failed" } else { "" },
                handler_output.output.trim()
            ));
        }
        Ok(contributed)
    }
}

impl EventHandlerDescriptor {
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
    serde_json::from_str(json)
        .map_err(|_| invalid_provider(format!("Provider {kind} is malformed")))
}

fn invalid_provider(message: impl Into<String>) -> AgentError {
    AgentError::new(code::PLUGIN_INVALID_OUTPUT, message)
}
