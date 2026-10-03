//! The transcript renderer Component.
//!
//! Pure computation: it turns request JSON into a JSON display list and holds no
//! state between calls. It has no host import, so it cannot read files, spawn
//! processes, reach the network, or touch the UI — the host owns every effect,
//! the fonts, and the egui layout.
//!
//! Two entry points share one Component:
//!
//! * `render-message` — a message body → a display list;
//! * `render-tool` — a tool call → a display list (a fold whose body is the
//!   tool's panel).
//!
//! It also implements the harness `plugin` interface, inertly. That is what
//! lets the plugin platform load it as an ordinary built-in plugin: it
//! contributes no tools, no event handlers, and no UI, and the host calls its
//! render entry points directly.

mod code_view;
mod display;
mod markdown;
mod protocol;

// The renderer builds against the shared `wit/` package: it exports both the
// harness `plugin` interface and its own `renderer` interface.
wit_bindgen::generate!({
    path: "../../wit",
    world: "transcript-renderer",
    async: true,
});

use exports::deluxe::harness::plugin::Guest as PluginGuest;
use exports::deluxe::harness::renderer::Guest as RendererGuest;

struct TranscriptRenderer;

impl RendererGuest for TranscriptRenderer {
    async fn render_message(request_json: String) -> Result<String, String> {
        protocol::render_message(&request_json)
    }

    async fn render_tool(request_json: String) -> Result<String, String> {
        protocol::render_tool(&request_json)
    }
}

impl PluginGuest for TranscriptRenderer {
    async fn configure() -> Result<(), String> {
        Ok(())
    }

    async fn list_tools() -> String {
        "[]".into()
    }

    async fn execute_tool(_name: String, _arguments_json: String) -> Result<String, String> {
        Err("transcript renderer provides no tools".into())
    }

    async fn list_event_handlers() -> String {
        "[]".into()
    }

    async fn handle_event(_handler_id: String, _event_json: String) -> Result<String, String> {
        Err("transcript renderer has no event handlers".into())
    }

    async fn open_surface(_request_json: String) -> Result<String, String> {
        Err("transcript renderer has no UI".into())
    }

    async fn handle_action(_action_json: String) -> Result<String, String> {
        Err("transcript renderer has no UI".into())
    }

    async fn close_surface(_surface_id: String) {}
}

export!(TranscriptRenderer);
