#![no_std]

extern crate alloc;

use alloc::string::String;

wit_bindgen::generate!({
    path: "../../wit",
    world: "harness-plugin",
    async: true,
});

use exports::deluxe::harness::plugin::Guest;

struct BuiltinTools;

impl Guest for BuiltinTools {
    async fn configure() -> Result<(), String> {
        Ok(())
    }

    async fn list_tools() -> String {
        deluxe::harness::host::list_tools().await
    }

    async fn execute_tool(name: String, arguments_json: String) -> Result<String, String> {
        deluxe::harness::host::invoke_tool(name, arguments_json).await
    }

    async fn list_event_handlers() -> String {
        "[]".into()
    }

    async fn handle_event(_handler_id: String, _event_json: String) -> Result<String, String> {
        Err("bundled tools provider has no event handlers".into())
    }

    async fn open_surface(_request_json: String) -> Result<String, String> {
        Err("bundled tools provider has no UI".into())
    }

    async fn handle_action(_action_json: String) -> Result<String, String> {
        Err("bundled tools provider has no UI".into())
    }

    async fn close_surface(_surface_id: String) {}
}

export!(BuiltinTools);
