//! The LLM provider Component.
//!
//! It owns everything about reaching a model — which vendors exist, which is
//! active, how each request is shaped, and how each response stream is decoded —
//! while the host owns the socket. That split is what lets a strictly serialized
//! actor serve a streamed turn without head-of-line blocking: the Component does
//! no I/O inside these calls beyond reading its own configuration, so the host
//! can pump bytes at it one chunk at a time.
//!
//! It also implements the harness `plugin` interface, which is what lets the
//! plugin platform load it as an ordinary Component and show its `providers`
//! settings surface.

mod codec;
mod config;
mod state;
mod surface;

use serde_json::json;

wit_bindgen::generate!({
    path: "../../wit",
    world: "llm-provider",
    async: true,
});

use crate::codec::{Decoder, Protocol};
use exports::deluxe::harness::llm::Guest as LlmGuest;
use exports::deluxe::harness::plugin::Guest as PluginGuest;

struct LlmProvider;

impl PluginGuest for LlmProvider {
    async fn configure() -> Result<(), String> {
        config::load().await
    }

    async fn describe() -> Result<String, String> {
        // The surface actor and the provider actor are separate instances, so
        // the configuration must be re-read here rather than trusted from a
        // stale in-memory cache another instance may have since rewritten.
        config::load().await?;
        let payload = match config::active_provider()? {
            Some(provider) => json!({
                "ready": !provider.base_url.is_empty() && !provider.model.is_empty(),
                "capabilities": { "imageInput": provider.supports_images },
                "limits": { "contextTokens": provider.context_limit },
            }),
            None => json!({
                "ready": false,
                "capabilities": { "imageInput": false },
                "limits": { "contextTokens": 0 },
            }),
        };
        Ok(payload.to_string())
    }

    async fn list_tools() -> String {
        "[]".into()
    }

    async fn execute_tool(_name: String, _arguments_json: String) -> Result<String, String> {
        Err("LLM provider provides no tools".into())
    }

    async fn list_event_handlers() -> String {
        "[]".into()
    }

    async fn handle_event(_handler_id: String, _event_json: String) -> Result<String, String> {
        Err("LLM provider has no event handlers".into())
    }

    async fn open_surface(request_json: String) -> Result<String, String> {
        config::load().await?;
        Ok(surface::document(&request_json))
    }

    async fn handle_action(action_json: String) -> Result<String, String> {
        // Reload before mutating so a change made by another instance is not
        // clobbered by this actor's stale snapshot.
        config::load().await?;
        surface::handle_action(&action_json).await
    }

    async fn close_surface(_surface_id: String) {}
}

impl LlmGuest for LlmProvider {
    async fn build_request(request_json: String) -> Result<String, String> {
        // Re-read the profile so a model switch made in the UI takes effect on
        // the very next request, even mid-run.
        config::load().await?;
        let provider = config::active_provider()?
            .ok_or_else(|| "No LLM provider is configured".to_string())?;
        let request = serde_json::from_str(&request_json)
            .map_err(|error| format!("Canonical request is invalid: {error}"))?;
        let key = config::resolve_key(&provider).await?;
        let http = codec::build_request(&provider, &key, &request)?;
        serde_json::to_string(&http).map_err(|error| error.to_string())
    }

    async fn parse_stream(stream_id: String, chunk: Vec<u8>) -> Result<String, String> {
        let protocol = active_protocol()?;
        let mut streams = state::streams().lock().map_err(|_| state::poisoned())?;
        let decoder = streams
            .entry(stream_id.clone())
            .or_insert_with(|| Decoder::new(protocol));
        let result = decoder.push(&chunk)?;
        if result.done {
            let decoder = streams
                .remove(&stream_id)
                .ok_or_else(|| "LLM stream state vanished".to_string())?;
            let turn = decoder.turn();
            Ok(json!({"events": result.events, "done": true, "turn": turn}).to_string())
        } else {
            Ok(json!({"events": result.events, "done": false, "turn": null}).to_string())
        }
    }

    async fn close_stream(stream_id: String) {
        if let Ok(mut streams) = state::streams().lock() {
            streams.remove(&stream_id);
        }
    }

    async fn parse_complete(body: Vec<u8>) -> Result<String, String> {
        let turn = codec::parse_complete(active_protocol()?, &body)?;
        serde_json::to_string(&turn).map_err(|error| error.to_string())
    }
}

/// The protocol of the active profile.
fn active_protocol() -> Result<Protocol, String> {
    let provider =
        config::active_provider()?.ok_or_else(|| "No LLM provider is configured".to_string())?;
    Protocol::parse(&provider.protocol)
        .ok_or_else(|| format!("Unknown provider protocol `{}`", provider.protocol))
}

export!(LlmProvider);
