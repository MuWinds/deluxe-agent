wit_bindgen::generate!({
    path: "../../wit",
    world: "harness-plugin",
    async: true,
});

use exports::deluxe::harness::plugin::Guest;

struct EchoTool;

fn request_field(request: &str, field: &str, fallback: &str) -> String {
    serde_json::from_str::<serde_json::Value>(request)
        .ok()
        .and_then(|value| {
            value
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| fallback.into())
}

impl Guest for EchoTool {
    async fn configure() -> Result<(), String> {
        Ok(())
    }

    async fn describe() -> Result<String, String> {
        // A tool fixture contributes no model metadata; the host ignores every
        // key this could carry.
        Ok("{}".into())
    }

    async fn list_tools() -> String {
        r#"[{"name":"wasm_echo","summary":"Echo fixture","description":"Returns a bounded fixture result","guidelines":[],"inputSchema":{"type":"object","properties":{},"required":[]},"hostValidatesArguments":true,"mutating":false}]"#.into()
    }

    async fn execute_tool(name: String, arguments_json: String) -> Result<String, String> {
        if name != "wasm_echo" {
            return Err(format!("unknown fixture tool: {name}"));
        }
        Ok(format!(
            r#"{{"content":[{{"type":"text","text":{arguments_json:?}}}],"isError":false,"truncated":false}}"#
        ))
    }

    async fn list_event_handlers() -> String {
        "[]".into()
    }

    async fn handle_event(_handler_id: String, _event_json: String) -> Result<String, String> {
        Err("echo fixture has no event handlers".into())
    }

    async fn open_surface(request_json: String) -> Result<String, String> {
        let plugin_id = request_field(&request_json, "pluginId", "echo@test");
        let surface_id = request_field(&request_json, "surfaceId", "main");
        Ok(format!(
            r#"{{"schemaVersion":1,"pluginId":{plugin_id:?},"surfaceId":{surface_id:?},"revision":1,"title":"Echo","root":{{"type":"button","id":"echo","label":"Echo","action":"echo","enabled":true}}}}"#
        ))
    }

    async fn handle_action(action_json: String) -> Result<String, String> {
        let plugin_id = request_field(&action_json, "pluginId", "echo@test");
        let surface_id = request_field(&action_json, "surfaceId", "main");
        Ok(format!(
            r#"{{"schemaVersion":1,"pluginId":{plugin_id:?},"surfaceId":{surface_id:?},"revision":2,"title":"Echo","root":{{"type":"text","text":"handled","emphasis":"normal"}}}}"#
        ))
    }

    async fn close_surface(_surface_id: String) {}
}

export!(EchoTool);
