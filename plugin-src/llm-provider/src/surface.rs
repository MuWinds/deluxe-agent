//! The `providers` settings surface.
//!
//! The whole provider model is editable here: every profile shows all of its
//! fields inline, so there is no separate editor to keep in sync with the
//! snapshot. A control's id encodes the entity it addresses (`field.<id>.model`)
//! because a manifest declares a fixed list of actions and a profile id cannot
//! be known when it is written.

use serde_json::{json, Value};

use crate::codec::Protocol;
use crate::config::{self, Provider};

/// The surface id of the inline composer control.
const COMPOSER_SURFACE_ID: &str = "composer";

/// Builds a surface's first snapshot, dispatching on `surfaceId`.
pub fn document(request_json: &str) -> String {
    let request: Value = serde_json::from_str(request_json).unwrap_or_else(|_| json!({}));
    match surface_id(&request).as_deref() {
        Some(COMPOSER_SURFACE_ID) => composer_document(&request, 1),
        _ => providers_document(&request, 1),
    }
}

/// The `surfaceId` a request names, if any.
fn surface_id(request: &Value) -> Option<String> {
    request
        .get("surfaceId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The plugin id a request names, or this plugin's bundled default.
fn plugin_id(request: &Value) -> &str {
    request
        .get("pluginId")
        .and_then(Value::as_str)
        .unwrap_or("llm-provider@deluxe-defaults")
}

/// Builds the providers window's snapshot at an explicit revision.
fn providers_document(request: &Value, revision: u64) -> String {
    let plugin_id = plugin_id(request);
    let surface_id = surface_id(request).unwrap_or_else(|| "providers".to_string());
    let config = config::snapshot().unwrap_or_default();

    let mut children = vec![
        json!({"type": "text", "text": "供应商", "emphasis": "strong"}),
        json!({
            "type": "text",
            "text": "每个供应商是一份档案，切换「设为当前」即可更换。协议决定请求与流的形状；\
                     密钥留空时按档案 id 向宿主取（环境变量 DELUXE_AGENT_SECRET_<ID> 或系统凭据库）。",
            "emphasis": "muted"
        }),
    ];

    let active = config
        .providers
        .iter()
        .find(|provider| provider.id == config.active);
    children.push(json!({
        "type": "text",
        "text": match active {
            Some(provider) => format!("当前：{} · {} · {}", provider.name, provider.model, provider.protocol),
            None => "当前：未选择供应商".to_string(),
        },
        "emphasis": "normal"
    }));
    children.push(json!({"type": "divider"}));

    for provider in &config.providers {
        children.push(provider_section(provider, provider.id == config.active));
    }

    children.push(json!({"type": "divider"}));
    children.push(json!({"type": "text", "text": "新增供应商", "emphasis": "strong"}));
    children.push(add_button());

    json!({
        "schemaVersion": 1,
        "pluginId": plugin_id,
        "surfaceId": surface_id,
        "revision": revision,
        "title": "LLM 供应商",
        "root": {"type": "column", "children": children}
    })
    .to_string()
}

/// Builds the inline composer control: a model picker over the profiles.
///
/// The host renders this straight into the input row, so it holds exactly one
/// control and no prose. With no profile configured there is nothing to pick,
/// and the host falls back to the settings window.
fn composer_document(request: &Value, revision: u64) -> String {
    let plugin_id = plugin_id(request);
    let surface_id = surface_id(request).unwrap_or_else(|| COMPOSER_SURFACE_ID.to_string());
    let config = config::snapshot().unwrap_or_default();

    let root = if config.providers.is_empty() {
        json!({"type": "empty"})
    } else {
        let options: Vec<Value> = config
            .providers
            .iter()
            .map(|provider| {
                json!({
                    "value": provider.id,
                    "label": model_label(provider),
                })
            })
            .collect();
        json!({
            "type": "select",
            "id": "model",
            "value": config.active,
            "options": options,
            "action": "select_model"
        })
    };

    json!({
        "schemaVersion": 1,
        "pluginId": plugin_id,
        "surfaceId": surface_id,
        "revision": revision,
        "title": "模型",
        "root": root
    })
    .to_string()
}

/// The composer dropdown's label: the model name, falling back to the profile's
/// name while it has no model.
///
/// The inline control is narrow and the model is what a switch is about, so the
/// provider is left out: the account that owns it is visible in the settings
/// surface, where the field can afford the room.
fn model_label(provider: &Provider) -> String {
    let model = provider.model.trim();
    if !model.is_empty() {
        model.to_string()
    } else {
        let name = provider.name.trim();
        if name.is_empty() {
            provider.id.clone()
        } else {
            name.to_string()
        }
    }
}

/// One profile's editable section.
fn provider_section(provider: &Provider, is_active: bool) -> Value {
    let max_output = provider
        .max_output_tokens
        .map(|tokens| tokens.to_string())
        .unwrap_or_default();
    let children = vec![
        json!({
            "type": "row",
            "children": [
                {
                    "type": "button",
                    "id": format!("active.{}", provider.id),
                    "label": "设为当前",
                    "action": "select_active",
                    "enabled": !is_active
                },
                {
                    "type": "button",
                    "id": format!("delete.{}", provider.id),
                    "label": "删除",
                    "action": "delete_provider",
                    "enabled": true
                }
            ]
        }),
        json!({
            "type": "select",
            "id": format!("protocol.{}", provider.id),
            "value": provider.protocol,
            "options": protocol_options(),
            "action": "set_protocol"
        }),
        field_row(
            provider,
            "name",
            "名称",
            &provider.name,
            "我的供应商",
            FieldKind::Text,
        ),
        field_row(
            provider,
            "baseUrl",
            "Base URL",
            &provider.base_url,
            "https://api.example.com/v1",
            FieldKind::Text,
        ),
        field_row(
            provider,
            "model",
            "模型",
            &provider.model,
            "模型名",
            FieldKind::Text,
        ),
        field_row(
            provider,
            "apiKey",
            "API Key",
            &provider.api_key,
            "留空则用 get-secret",
            FieldKind::Text,
        ),
        field_row(
            provider,
            "contextLimit",
            "上下文长度",
            &provider.context_limit.to_string(),
            "128000 或 128k",
            FieldKind::Tokens,
        ),
        field_row(
            provider,
            "maxOutputTokens",
            "最大输出",
            &max_output,
            "留空用供应商默认",
            FieldKind::Tokens,
        ),
        json!({
            "type": "checkbox",
            "id": format!("images.{}", provider.id),
            "label": "支持图片输入",
            "value": provider.supports_images,
            "action": "toggle_images"
        }),
        json!({
            "type": "text",
            "text": format!(
                "id: {} · {}",
                provider.id,
                if is_active { "当前使用" } else { "未启用" }
            ),
            "emphasis": "muted"
        }),
    ];
    json!({
        "type": "section",
        "id": format!("provider.{}", provider.id),
        "title": provider.name,
        "children": children
    })
}

/// Whether a field's text is a name or a token count.
#[derive(Clone, Copy, PartialEq)]
enum FieldKind {
    Text,
    Tokens,
}

/// A labelled text input bound to one profile field.
fn field_row(
    provider: &Provider,
    field: &str,
    label: &str,
    value: &str,
    placeholder: &str,
    kind: FieldKind,
) -> Value {
    let reader = match kind {
        FieldKind::Text => "text",
        FieldKind::Tokens => "tokens",
    };
    json!({
        "type": "row",
        "children": [
            {"type": "text", "text": label, "emphasis": "muted"},
            {
                "type": "textInput",
                "id": format!("field.{}.{}", provider.id, field),
                "value": value,
                "placeholder": placeholder,
                "reader": reader,
                "action": "set_field"
            }
        ]
    })
}

/// The button that appends a blank profile.
fn add_button() -> Value {
    json!({
        "type": "button",
        "id": "add",
        "label": "+ 自定义",
        "action": "add_provider",
        "enabled": true
    })
}

/// The three protocols a profile can speak.
fn protocol_options() -> Vec<Value> {
    vec![
        json!({"value": "openai-chat", "label": "OpenAI Chat Completions"}),
        json!({"value": "openai-responses", "label": "OpenAI Responses"}),
        json!({"value": "anthropic-messages", "label": "Anthropic Messages"}),
    ]
}

/// Applies one control action and returns the refreshed snapshot.
pub async fn handle_action(action_json: &str) -> Result<String, String> {
    let action: Value = serde_json::from_str(action_json)
        .map_err(|_| "LLM provider UI action is invalid JSON".to_string())?;
    // The response repeats the request's own identity so the host's revision and
    // identity checks see the surface the user acted on.
    let request = action.get("surface").cloned().unwrap_or_else(|| json!({}));
    let revision = action
        .get("revision")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(1);
    let verb = action
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "LLM provider UI action has no name".to_string())?;
    let control_id = action
        .get("controlId")
        .and_then(Value::as_str)
        .ok_or_else(|| "LLM provider UI action has no control".to_string())?;

    // The composer and the providers window are separate surfaces served by
    // separate actors; the action names which one it acted on.
    if surface_id(&request).as_deref() == Some(COMPOSER_SURFACE_ID) {
        if verb != "select_model" {
            return Err(format!("Unknown composer action `{verb}`"));
        }
        let id = value_string(&action)?;
        config::mutate(|config| {
            if config.providers.iter().any(|provider| provider.id == id) {
                config.active = id;
            }
        })?;
        config::persist().await?;
        return Ok(composer_document(&request, revision));
    }

    match verb {
        "add_provider" => {
            let id = config::mutate(next_id)?;
            let provider = Provider::custom(id);
            config::mutate(|config| {
                if config.active.is_empty() {
                    config.active = provider.id.clone();
                }
                config.providers.push(provider);
            })?;
            config::persist().await?;
        }
        "select_active" => {
            let id = prefixed(control_id, "active.")?;
            config::mutate(|config| config.active = id.to_string())?;
            config::persist().await?;
        }
        "delete_provider" => {
            let id = prefixed(control_id, "delete.")?;
            config::mutate(|config| {
                config.providers.retain(|provider| provider.id != id);
                if config.active == id {
                    config.active = config
                        .providers
                        .first()
                        .map(|provider| provider.id.clone())
                        .unwrap_or_default();
                }
            })?;
            config::persist().await?;
        }
        "set_protocol" => {
            let id = prefixed(control_id, "protocol.")?;
            let protocol = value_string(&action)?;
            if Protocol::parse(&protocol).is_none() {
                return Err(format!("Unknown protocol `{protocol}`"));
            }
            config::mutate(|config| {
                if let Some(provider) = config
                    .providers
                    .iter_mut()
                    .find(|provider| provider.id == id)
                {
                    provider.protocol = protocol;
                }
            })?;
            config::persist().await?;
        }
        "set_field" => {
            let (id, field) = control_id
                .strip_prefix("field.")
                .and_then(|rest| rest.split_once('.'))
                .ok_or_else(|| "Field action names no profile field".to_string())?;
            let value = value_string(&action)?;
            config::mutate(|config| {
                if let Some(provider) = config
                    .providers
                    .iter_mut()
                    .find(|provider| provider.id == id)
                {
                    apply_field(provider, field, &value);
                }
            })?;
            config::persist().await?;
        }
        "toggle_images" => {
            let id = prefixed(control_id, "images.")?;
            let value = value_bool(&action)?;
            config::mutate(|config| {
                if let Some(provider) = config
                    .providers
                    .iter_mut()
                    .find(|provider| provider.id == id)
                {
                    provider.supports_images = value;
                }
            })?;
            config::persist().await?;
        }
        other => return Err(format!("Unknown LLM provider UI action `{other}`")),
    }

    Ok(providers_document(&request, revision))
}

/// The suffix after a `prefix`, or an error naming the malformed control.
fn prefixed<'a>(control_id: &'a str, prefix: &str) -> Result<&'a str, String> {
    control_id
        .strip_prefix(prefix)
        .filter(|rest| !rest.is_empty())
        .ok_or_else(|| format!("UI control `{control_id}` is malformed"))
}

/// Applies one text field, ignoring an edit that would not parse.
fn apply_field(provider: &mut Provider, field: &str, value: &str) {
    let trimmed = value.trim();
    match field {
        "name" => provider.name = value.to_string(),
        "baseUrl" => provider.base_url = value.to_string(),
        "model" => provider.model = value.to_string(),
        "apiKey" => provider.api_key = value.to_string(),
        "contextLimit" => {
            if trimmed.is_empty() {
                provider.context_limit = 0;
            } else if let Some(parsed) = parse_token_count(trimmed) {
                provider.context_limit = parsed.min(u32::MAX as u64) as u32;
            }
        }
        "maxOutputTokens" => {
            if trimmed.is_empty() {
                provider.max_output_tokens = None;
            } else if let Some(parsed) = parse_token_count(trimmed) {
                provider.max_output_tokens = Some(parsed.min(u32::MAX as u64) as u32);
            }
        }
        _ => {}
    }
}

/// Reads a token count that may carry a `k` / `M` suffix.
///
/// The host commits the shorthand as a bare number, but a text field can be
/// edited and persisted by hand, so the shorthand is accepted on the way back
/// in as well.
fn parse_token_count(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (digits, multiplier) = match text.as_bytes().last().copied() {
        Some(b'k') | Some(b'K') => (&text[..text.len() - 1], 1_000u64),
        Some(b'm') | Some(b'M') => (&text[..text.len() - 1], 1_000_000),
        _ => (text, 1),
    };
    if digits.is_empty() {
        return None;
    }
    let mut seen_dot = false;
    for character in digits.chars() {
        match character {
            '0'..='9' => {}
            '.' if !seen_dot => seen_dot = true,
            _ => return None,
        }
    }
    let value: f64 = digits.parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    let tokens = value * multiplier as f64;
    (tokens <= u64::MAX as f64).then_some(tokens.round() as u64)
}

/// The smallest unused `provider-N` id.
fn next_id(config: &mut config::Config) -> String {
    let mut n = config.providers.len() + 1;
    loop {
        let id = format!("provider-{n}");
        if !config.providers.iter().any(|provider| provider.id == id) {
            return id;
        }
        n += 1;
    }
}

/// Reads a `UiValue::String` out of an action.
fn value_string(action: &Value) -> Result<String, String> {
    let value = action
        .get("value")
        .ok_or_else(|| "UI action carried no value".to_string())?;
    if value.get("type").and_then(Value::as_str) != Some("string") {
        return Err("UI action expected a string value".to_string());
    }
    value
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "UI string value is missing".to_string())
}

/// Reads a `UiValue::Bool` out of an action.
fn value_bool(action: &Value) -> Result<bool, String> {
    let value = action
        .get("value")
        .ok_or_else(|| "UI action carried no value".to_string())?;
    if value.get("type").and_then(Value::as_str) != Some("bool") {
        return Err("UI action expected a boolean value".to_string());
    }
    value
        .get("value")
        .and_then(Value::as_bool)
        .ok_or_else(|| "UI boolean value is missing".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inline control names the model, and falls back to something
    /// readable before a model has been typed into a fresh profile.
    #[test]
    fn the_composer_label_is_the_model_name() {
        let mut provider = Provider::custom("provider-1".into());
        provider.name = "自定义".into();
        assert_eq!(model_label(&provider), "自定义");
        provider.model = "deepseek-chat".into();
        assert_eq!(model_label(&provider), "deepseek-chat");
        // A blank model with no name either still has to be selectable, so the
        // stable id is the last resort.
        provider.name = String::new();
        provider.model = String::new();
        assert_eq!(model_label(&provider), "provider-1");
    }

    /// The two cache-dependent cases live in one test because they mutate the
    /// process-wide configuration cache and would otherwise race each other.
    #[test]
    fn the_composer_reflects_the_profiles_and_degrades_when_there_are_none() {
        config::mutate(|config| {
            config.providers = vec![
                Provider {
                    model: "m1".into(),
                    ..Provider::custom("a".into())
                },
                Provider {
                    model: "m2".into(),
                    ..Provider::custom("b".into())
                },
            ];
            config.active = "b".into();
        })
        .expect("the cache is not poisoned");

        let document: Value =
            serde_json::from_str(&composer_document(&json!({"surfaceId": "composer"}), 1))
                .expect("composer document is JSON");

        assert_eq!(document["surfaceId"], "composer");
        assert_eq!(document["root"]["type"], "select");
        assert_eq!(document["root"]["value"], "b");
        assert_eq!(document["root"]["action"], "select_model");
        assert_eq!(
            document["root"]["options"].as_array().map(Vec::len),
            Some(2)
        );

        // With nothing configured there is no option the host's select could
        // legally hold, so the composer degrades to an empty node.
        config::mutate(|config| {
            config.providers.clear();
            config.active.clear();
        })
        .expect("the cache is not poisoned");
        let empty: Value = serde_json::from_str(&composer_document(&json!({}), 1))
            .expect("composer document is JSON");
        assert_eq!(empty["root"]["type"], "empty");
    }

    /// The token fields declare the shorthand reader and accept it on the way
    /// back in, without which a hand-edited `128k` would silently not stick.
    #[test]
    fn token_fields_declare_the_reader_and_accept_the_shorthand() {
        let provider = Provider::custom("provider-1".into());
        let context = field_row(
            &provider,
            "contextLimit",
            "上下文长度",
            "128000",
            "128k",
            FieldKind::Tokens,
        );
        assert_eq!(context["children"][1]["reader"], "tokens");
        assert_eq!(context["children"][1]["action"], "set_field");

        let mut provider = Provider::custom("provider-1".into());
        apply_field(&mut provider, "contextLimit", "128k");
        assert_eq!(provider.context_limit, 128_000);
        apply_field(&mut provider, "maxOutputTokens", "1.5k");
        assert_eq!(provider.max_output_tokens, Some(1_500));

        // A field left half-typed keeps the value it had.
        apply_field(&mut provider, "contextLimit", "1M");
        assert_eq!(provider.context_limit, 1_000_000);
        apply_field(&mut provider, "contextLimit", "");
        assert_eq!(provider.context_limit, 0);
        apply_field(&mut provider, "maxOutputTokens", "nonsense");
        assert_eq!(provider.max_output_tokens, Some(1_500));
    }
}
