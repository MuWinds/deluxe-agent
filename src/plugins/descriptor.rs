//! A plugin's self-description, read generically by the host.
//!
//! Every Component answers `plugin.describe` with a JSON object. The host reads
//! only the generic keys it acts on — `ready`, `capabilities.imageInput`, and
//! `limits.contextTokens` — so the host stays ignorant of what a plugin's
//! capability actually is. A plugin with nothing to declare returns `{}`, which
//! decodes to the defaults here; a model provider fills them from its active
//! profile, since it owns the endpoint and the key.

use serde_json::Value;

use crate::error::{code, AgentError, Result};

use super::wasm_runtime::{ComponentActor, Operation};

/// What a plugin reports about its current capability, before a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginDescriptor {
    /// Whether the plugin can serve requests now. A model provider reports
    /// false when no profile is selected, in which case a run cannot reach a
    /// model at all.
    pub ready: bool,
    /// Whether the active model accepts image input.
    pub image_input: bool,
    /// The active model's context window, in tokens; 0 means unknown.
    pub context_tokens: u64,
}

impl PluginDescriptor {
    /// Reads the generic descriptor keys out of a plugin's `describe` JSON.
    ///
    /// Returns `Err` only when the reply is not JSON; a missing key reads as
    /// its default, so a plugin that declares nothing is not an error.
    pub fn from_json(json: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(json).map_err(|_| {
            AgentError::new(
                code::PLUGIN_INVALID_OUTPUT,
                "Plugin descriptor is invalid JSON",
            )
        })?;
        Ok(Self {
            ready: value.get("ready").and_then(Value::as_bool).unwrap_or(false),
            image_input: value
                .pointer("/capabilities/imageInput")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            context_tokens: value
                .pointer("/limits/contextTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        })
    }
}

/// Asks a Component for its self-description.
pub async fn describe(actor: &ComponentActor) -> Result<PluginDescriptor> {
    let json = actor.call(Operation::Describe).await?;
    PluginDescriptor::from_json(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_descriptor_reads_every_generic_key() {
        let descriptor = PluginDescriptor::from_json(
            r#"{"ready":true,"capabilities":{"imageInput":true},"limits":{"contextTokens":128000}}"#,
        )
        .expect("the descriptor decodes");
        assert!(descriptor.ready);
        assert!(descriptor.image_input);
        assert_eq!(descriptor.context_tokens, 128_000);
    }

    #[test]
    fn an_empty_descriptor_decodes_to_defaults() {
        let descriptor = PluginDescriptor::from_json("{}").expect("an empty object decodes");
        assert_eq!(descriptor, PluginDescriptor::default());
    }

    #[test]
    fn a_partial_descriptor_fills_the_missing_keys_with_defaults() {
        let descriptor =
            PluginDescriptor::from_json(r#"{"ready":true}"#).expect("a partial object decodes");
        assert!(descriptor.ready);
        assert!(!descriptor.image_input);
        assert_eq!(descriptor.context_tokens, 0);
    }

    #[test]
    fn a_non_json_reply_is_rejected() {
        assert!(PluginDescriptor::from_json("not json").is_err());
    }
}
