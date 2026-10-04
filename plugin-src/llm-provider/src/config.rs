//! Provider profiles, owned entirely by the Component.
//!
//! The host stores no provider profile and no API key: the whole model — which
//! vendors exist, which one is active, how to reach each — lives in this
//! Component's configuration file. That is why the host has to ask `describe()`
//! for the metadata it needs (image support, context limit) instead of reading
//! it from its own config.
//!
//! The file is read and written through the host's bounded file capability, so
//! it stays inside the scope root the host bound; the Component never touches a
//! path of its own.

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::codec::Protocol;
use crate::deluxe::harness::host;

/// The profile file, relative to this Component's configuration root.
const CONFIG_FILE: &str = "llm-provider.json";

/// One named provider profile.
///
/// `protocol` and the vendor identity are separate on purpose: DeepSeek, Kimi,
/// and GLM all speak `openai-chat`, so they share one codec and differ only in
/// their base URL and model. `id` is the stable identifier a secret is looked up
/// by and a UI control is addressed by; `name` is only a label.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub model: String,
    /// An inline key, or empty to fall back to `get-secret(id)`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    #[serde(default)]
    pub supports_images: bool,
    /// The context window to budget against, in tokens. `0` means "unknown".
    #[serde(default)]
    pub context_limit: u32,
    /// `max_tokens` / `max_output_tokens` to send, or `None` for the provider's
    /// own ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// The whole configuration file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// The id of the active profile, or empty when none is selected.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub active: String,
    #[serde(default)]
    pub providers: Vec<Provider>,
}

impl Provider {
    /// Builds a blank profile the user fills in by hand.
    ///
    /// There are no vendor presets: the endpoint, the model name, and the
    /// protocol are the user's to know, and a guessed default reads as
    /// authoritative while being wrong. Only the protocol starts on a value
    /// because a profile cannot exist without one.
    pub fn custom(id: String) -> Self {
        Self {
            id,
            name: "自定义".to_string(),
            protocol: "openai-chat".to_string(),
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            supports_images: false,
            context_limit: 128_000,
            max_output_tokens: None,
        }
    }
}

impl Config {
    /// Drops profiles that cannot be addressed or decoded, and points `active`
    /// at a profile that exists.
    ///
    /// A hand-edited file can name a protocol no codec knows or leave `active`
    /// pointing at a deleted profile; repairing here means the rest of the
    /// Component can assume a profile it holds is usable.
    fn repaired(mut self) -> Self {
        self.providers.retain(|provider| {
            valid_identifier(&provider.id) && Protocol::parse(&provider.protocol).is_some()
        });
        if !self
            .providers
            .iter()
            .any(|provider| provider.id == self.active)
        {
            self.active = self
                .providers
                .first()
                .map(|provider| provider.id.clone())
                .unwrap_or_default();
        }
        self
    }
}

/// Accepts the identifiers a UI control id and a secret name can carry.
pub fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn cache() -> &'static Mutex<Config> {
    static CONFIG: OnceLock<Mutex<Config>> = OnceLock::new();
    CONFIG.get_or_init(|| Mutex::new(Config::default()))
}

fn poisoned() -> String {
    "LLM provider configuration is poisoned".to_string()
}

/// Loads the profile file into the in-memory cache.
///
/// Called from `configure()`. A missing file is the normal empty case; a file
/// that does not parse is treated as empty rather than failing the whole
/// plugin, because a Component that cannot configure contributes no model at
/// all.
pub async fn load() -> Result<(), String> {
    let config = match host::read_plugin_file(CONFIG_FILE.into()).await {
        Ok(bytes) => String::from_utf8(bytes)
            .ok()
            .and_then(|text| serde_json::from_str::<Config>(&text).ok())
            .unwrap_or_default(),
        Err(error) if error.starts_with("plugin_file_not_found:") => Config::default(),
        Err(error) => return Err(error),
    };
    *cache().lock().map_err(|_| poisoned())? = config.repaired();
    Ok(())
}

/// A copy of the current configuration.
pub fn snapshot() -> Result<Config, String> {
    cache()
        .lock()
        .map(|config| config.clone())
        .map_err(|_| poisoned())
}

/// The active profile, or `None` when none is selected.
pub fn active_provider() -> Result<Option<Provider>, String> {
    let config = snapshot()?;
    Ok(config
        .providers
        .iter()
        .find(|provider| provider.id == config.active)
        .cloned())
}

/// Mutates the cached configuration and returns the closure's value.
pub fn mutate<R>(f: impl FnOnce(&mut Config) -> R) -> Result<R, String> {
    let mut config = cache().lock().map_err(|_| poisoned())?;
    Ok(f(&mut config))
}

/// Writes the cached configuration back to the profile file.
///
/// The whole file is rewritten through the host's bounded write capability, so
/// the Component never needs an ambient filesystem.
pub async fn persist() -> Result<(), String> {
    let config = snapshot()?;
    let text = serde_json::to_string_pretty(&config)
        .map_err(|_| "LLM provider configuration is not serializable".to_string())?;
    host::write_plugin_file(CONFIG_FILE.into(), text.into_bytes()).await
}

/// Resolves the key to authenticate a request with.
///
/// An inline key wins; otherwise the host is asked for the secret named after
/// the profile id, which reads the environment and then the OS credential store.
pub async fn resolve_key(provider: &Provider) -> Result<String, String> {
    if !provider.api_key.trim().is_empty() {
        return Ok(provider.api_key.clone());
    }
    match host::get_secret(provider.id.clone()).await? {
        Some(secret) if !secret.trim().is_empty() => Ok(secret),
        _ => Err(format!(
            "No API key for provider `{}`. Enter one in the plugin, or store it as the `{}` secret.",
            provider.name, provider.id
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str) -> Provider {
        Provider {
            id: id.into(),
            name: "P".into(),
            protocol: "openai-chat".into(),
            base_url: "https://api.example.com/v1".into(),
            model: "m".into(),
            api_key: String::new(),
            supports_images: false,
            context_limit: 0,
            max_output_tokens: None,
        }
    }

    #[test]
    fn an_active_id_that_names_no_profile_falls_back_to_the_first() {
        let config = Config {
            active: "gone".into(),
            providers: vec![provider("provider-1")],
        }
        .repaired();
        assert_eq!(config.active, "provider-1");
    }

    #[test]
    fn profiles_with_an_unknown_protocol_or_id_are_dropped() {
        let config = Config {
            active: String::new(),
            providers: vec![
                provider("provider-1"),
                Provider {
                    protocol: "nope".into(),
                    ..provider("provider-2")
                },
                Provider {
                    id: "bad id".into(),
                    ..provider("provider-3")
                },
            ],
        }
        .repaired();
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].id, "provider-1");
    }

    #[test]
    fn an_empty_configuration_repairs_to_nothing() {
        let config = Config::default().repaired();
        assert!(config.active.is_empty());
        assert!(config.providers.is_empty());
    }

    #[test]
    fn a_custom_profile_speaks_a_known_protocol_and_starts_blank() {
        let provider = Provider::custom("provider-1".into());
        assert!(Protocol::parse(&provider.protocol).is_some());
        assert!(provider.base_url.is_empty(), "the endpoint is the user's");
        assert!(provider.model.is_empty(), "the model name is the user's");
        assert!(provider.api_key.is_empty());
    }

    #[test]
    fn a_profile_round_trips_through_json() {
        let config = Config {
            active: "provider-1".into(),
            providers: vec![Provider {
                max_output_tokens: Some(4096),
                ..provider("provider-1")
            }],
        };
        let text = serde_json::to_string(&config).expect("serializes");
        let parsed: Config = serde_json::from_str(&text).expect("parses");

        assert_eq!(parsed.active, "provider-1");
        assert_eq!(parsed.providers[0].max_output_tokens, Some(4096));
    }

    #[test]
    fn identifiers_reject_spaces_control_characters_and_dots() {
        assert!(valid_identifier("provider-1"));
        assert!(valid_identifier("llm_api_key"));
        assert!(!valid_identifier(""));
        assert!(!valid_identifier("bad id"));
        assert!(!valid_identifier("bad/id"));
        // A dot would make `field.<id>.<name>` ambiguous to split.
        assert!(!valid_identifier("llm.api.key"));
    }
}
