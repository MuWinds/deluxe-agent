//! Converts component exports into the existing host tool and UI ports.

use std::sync::Arc;

use serde_json::Value;

use crate::error::{code, AgentError, Result};
use crate::tools::{Tool, ToolDescriptor, ToolOutput, ToolSettings};

use super::runtime::PluginUiExecutor;
use super::ui_protocol::{
    decode_document, PluginUiAction, PluginUiDocument, SurfaceRequest, MAX_PAYLOAD_BYTES,
};
use super::wasm_runtime::{ComponentActor, Operation};

pub struct WasmUiExecutor {
    actor: Arc<ComponentActor>,
}

impl WasmUiExecutor {
    /// Adapts an isolated component actor without exposing it to the GUI.
    pub fn new(actor: Arc<ComponentActor>) -> Self {
        Self { actor }
    }
}

#[async_trait::async_trait]
impl PluginUiExecutor for WasmUiExecutor {
    async fn open_surface(&mut self, request: &SurfaceRequest) -> Result<PluginUiDocument> {
        let request = serde_json::to_string(request)
            .map_err(|error| AgentError::internal(error.to_string()))?;
        decode_document(&self.actor.call(Operation::Open(request)).await?)
    }

    async fn handle_action(&mut self, action: &PluginUiAction) -> Result<PluginUiDocument> {
        let action = serde_json::to_string(action)
            .map_err(|error| AgentError::internal(error.to_string()))?;
        decode_document(&self.actor.call(Operation::Action(action)).await?)
    }

    async fn close_surface(&mut self, request: &SurfaceRequest) -> Result<()> {
        self.actor
            .call(Operation::Close(request.surface_id.clone()))
            .await?;
        Ok(())
    }
}

pub struct WasmTool {
    descriptor: ToolDescriptor,
    actor: Arc<ComponentActor>,
}

#[async_trait::async_trait]
impl Tool for WasmTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(&self, arguments: Value, _settings: &ToolSettings) -> Result<ToolOutput> {
        let arguments = arguments.to_string();
        if arguments.len() > MAX_PAYLOAD_BYTES {
            return Err(AgentError::invalid_params(
                "Component tool arguments exceed the payload limit",
            ));
        }
        let json = self
            .actor
            .call(Operation::Execute {
                name: self.descriptor.name.clone(),
                arguments,
            })
            .await?;
        decode_tool_output(&json)
    }
}

/// Loads bounded descriptors and adapters. Returns `Err` for malformed tool metadata.
pub async fn tools(actor: Arc<ComponentActor>) -> Result<Vec<Arc<dyn Tool>>> {
    let json = actor.call(Operation::ListTools).await?;
    let descriptors: Vec<ToolDescriptor> = serde_json::from_str(&json).map_err(|_| {
        AgentError::new(
            code::PLUGIN_INVALID_OUTPUT,
            "Invalid component tool descriptors",
        )
    })?;
    if descriptors.len() > 64
        || descriptors.iter().any(|tool| {
            tool.name.is_empty()
                || tool.name.len() > 64
                || !tool
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                || tool.input_schema.schema_type != "object"
                || tool
                    .input_schema
                    .required
                    .iter()
                    .any(|key| !tool.input_schema.properties.contains_key(key))
        })
    {
        return Err(AgentError::new(
            code::PLUGIN_INVALID_OUTPUT,
            "Invalid component tool schema or name",
        ));
    }
    Ok(descriptors
        .into_iter()
        .map(|descriptor| {
            Arc::new(WasmTool {
                descriptor,
                actor: actor.clone(),
            }) as Arc<dyn Tool>
        })
        .collect())
}

/// Decodes a component tool result.
///
/// Returns `Err` for malformed data or payloads over the component limit. The
/// component protocol leaves output ownership to the user, so plugins may
/// return images and patch hunk metadata for their own workflows.
pub fn decode_tool_output(json: &str) -> Result<ToolOutput> {
    if json.len() > MAX_PAYLOAD_BYTES {
        return Err(AgentError::new(
            code::PLUGIN_INVALID_OUTPUT,
            "Tool output exceeds the payload limit",
        ));
    }
    let output: ToolOutput = serde_json::from_str(json).map_err(|_| {
        AgentError::new(code::PLUGIN_INVALID_OUTPUT, "Invalid component tool output")
    })?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::Duration;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::RwLock;

    use crate::plugins::capabilities::CapabilityHub;
    use crate::plugins::runtime::PluginUiExecutor;
    use crate::plugins::ui_protocol::{PluginUiAction, SurfaceRequest, UiNode};
    use crate::plugins::wasm_manifest::{Permissions, WasmManifest};
    use crate::tools::{ToolRegistry, ToolSettings};

    fn fixture_manifest() -> WasmManifest {
        let manifest: crate::plugins::PluginManifest =
            serde_json::from_str(include_str!("../../plugin-fixtures/echo-tool/plugin.json"))
                .expect("the checked-in fixture manifest is valid");
        manifest
            .wasm_runtime()
            .expect("the checked-in fixture declares a Wasm runtime")
    }

    fn fixture_hub(project: PathBuf) -> CapabilityHub {
        CapabilityHub::new(
            project.clone(),
            project,
            Default::default(),
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .expect("raw host")
    }

    async fn read_http_request(socket: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let header_end = loop {
            let mut chunk = [0u8; 1024];
            let read = socket
                .read(&mut chunk)
                .await
                .expect("the fixture server can read the request");
            assert!(read > 0, "the provider must send a complete HTTP request");
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let content_length = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let total = header_end + content_length;
        while request.len() < total {
            let mut chunk = [0u8; 1024];
            let read = socket
                .read(&mut chunk)
                .await
                .expect("the fixture server can read the request body");
            assert!(read > 0, "the provider must send the declared request body");
            request.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8_lossy(&request[header_end..total]).into_owned()
    }

    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn checked_in_component_loads_and_registers_its_echo_tool() {
        let root = tempfile::tempdir().expect("a temporary plugin root is available");
        std::fs::write(
            root.path().join("plugin.wasm"),
            include_bytes!("../../plugin-fixtures/echo-tool/plugin.wasm"),
        )
        .expect("the fixture component is copied");

        let actor = ComponentActor::load(
            root.path().to_path_buf(),
            fixture_manifest(),
            fixture_hub(root.path().to_path_buf()),
        )
        .await
        .expect("the checked-in component implements the harness world");

        let tools = tools(actor.clone())
            .await
            .expect("the fixture descriptors are valid");
        assert_eq!(tools.len(), 1, "the fixture exposes one tool");
        assert_eq!(tools[0].descriptor().name, "wasm_echo");

        let output = tools[0]
            .execute(json!({"value":"hello"}), &ToolSettings::default())
            .await
            .expect("the fixture echo call succeeds");
        assert!(
            output.as_text().contains("hello"),
            "echo output should preserve the JSON arguments"
        );

        let request = SurfaceRequest {
            plugin_id: "echo@test".into(),
            project: root.path().to_path_buf(),
            surface_id: "main".into(),
            request_id: 1,
        };
        let mut ui = WasmUiExecutor::new(actor);
        let document = ui
            .open_surface(&request)
            .await
            .expect("the fixture surface opens");
        assert!(matches!(document.root, UiNode::Button { .. }));

        let document = ui
            .handle_action(&PluginUiAction {
                surface: request.clone(),
                revision: document.revision,
                control_id: "echo".into(),
                action: "echo".into(),
                value: None,
            })
            .await
            .expect("the fixture action returns a newer snapshot");
        assert_eq!(document.revision, 2);
        assert!(matches!(document.root, UiNode::Text { .. }));
        ui.close_surface(&request)
            .await
            .expect("the fixture surface closes");
    }

    #[tokio::test]
    async fn checked_in_hooks_provider_loads_its_hooks_input() {
        let plugin_root = tempfile::tempdir().expect("a temporary plugin root is available");
        let project_root = tempfile::tempdir().expect("a temporary project root is available");
        let manifest = serde_json::from_str::<crate::plugins::PluginManifest>(include_str!(
            "../../plugin-fixtures/hooks-provider/plugin.json"
        ))
        .expect("the checked-in Hooks manifest is valid")
        .wasm_runtime()
        .expect("the checked-in Hooks manifest declares a Wasmtime runtime");
        std::fs::write(
            plugin_root.path().join(".hooks.json"),
            br#"{"hooks":{"PostToolUse":[{"matcher":".*","hooks":[{"type":"command","command":"echo wrong-plugin-root"}]}]}}"#,
        )
        .expect("the plugin-root decoy configuration is writable");
        std::fs::write(
            plugin_root.path().join("plugin.wasm"),
            include_bytes!("../../plugin-fixtures/hooks-provider/plugin.wasm"),
        )
        .expect("the Hooks component is copied into its plugin root");
        std::fs::write(
            project_root.path().join(".hooks.json"),
            r#"{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": ".*",
        "hooks": [
          {
            "type": "command",
            "command": "echo hooks-provider-ran"
          }
        ]
      }
    ]
  }
}"#,
        )
        .expect("the scoped Hooks configuration is writable");
        let hub = CapabilityHub::new(
            project_root.path().to_path_buf(),
            project_root.path().to_path_buf(),
            Default::default(),
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .expect("the project configuration host is valid");
        let actor = ComponentActor::load(plugin_root.path().to_path_buf(), manifest, hub)
            .await
            .expect("the Hooks provider implements the harness world");

        let hooks = actor
            .call(Operation::ListHooks)
            .await
            .expect("the Hooks provider lists its configured hooks");
        let hooks: serde_json::Value = serde_json::from_str(&hooks).expect("the hook list is JSON");
        assert_eq!(
            hooks[0]["label"], "echo hooks-provider-ran",
            "the Hooks Component reads the project configuration, not the plugin directory"
        );
        actor.shutdown();
    }

    #[tokio::test]
    async fn bundled_mcp_component_loads_with_an_empty_default_configuration() {
        let root = tempfile::tempdir().expect("a temporary plugin root is available");
        let package = crate::plugins::defaults::PLUGINS
            .iter()
            .find(|package| package.name == "mcp")
            .expect("the application ships a bundled MCP Component");
        let _manifest = serde_json::from_str::<crate::plugins::PluginManifest>(package.manifest)
            .expect("the bundled MCP manifest is valid")
            .wasm_runtime()
            .expect("the bundled MCP manifest declares Wasmtime");
        let actor =
            ComponentActor::load_bytes(package.component, fixture_hub(root.path().to_path_buf()))
                .await
                .expect("the bundled MCP Component starts with no configured servers");

        assert_eq!(
            actor
                .call(Operation::ListTools)
                .await
                .expect("the bundled provider lists its configured tools"),
            "[]"
        );
        let request = SurfaceRequest {
            plugin_id: "mcp@deluxe-defaults".into(),
            project: root.path().to_path_buf(),
            surface_id: "mcp".into(),
            request_id: 1,
        };
        let mut ui = WasmUiExecutor::new(actor);
        let document = ui
            .open_surface(&request)
            .await
            .expect("the bundled MCP provider exposes its Wasmtime UI");
        assert_eq!(document.title, "MCP");
        ui.close_surface(&request)
            .await
            .expect("the bundled MCP Component closes its UI");
    }

    #[tokio::test]
    async fn mcp_surface_stops_and_uninstalls_a_configured_server() {
        let project = tempfile::tempdir().expect("a temporary project root is available");
        let package = crate::plugins::defaults::PLUGINS
            .iter()
            .find(|package| package.name == "mcp")
            .expect("the application ships a bundled MCP Component");
        // Two servers, so removing one proves the other survives the rewrite.
        std::fs::write(
            project.path().join(".mcp.json"),
            r#"{"mcpServers":{"alpha":{"url":"http://127.0.0.1:1/a"},"beta":{"url":"http://127.0.0.1:1/b"}}}"#,
        )
        .expect("the scoped MCP configuration is writable");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions {
                write_plugin_files: true,
                ..Default::default()
            },
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .expect("scope host");
        let actor = ComponentActor::load_bytes(package.component, hub)
            .await
            .expect("the bundled MCP Component reads its scoped configuration");
        let request = SurfaceRequest {
            plugin_id: "mcp@deluxe-defaults".into(),
            project: project.path().to_path_buf(),
            surface_id: "mcp".into(),
            request_id: 1,
        };
        let mut ui = WasmUiExecutor::new(actor);
        let document = ui
            .open_surface(&request)
            .await
            .expect("the MCP surface opens with both servers listed");

        let has_toggle = |document: &crate::plugins::ui_protocol::PluginUiDocument, id: &str| {
            fn walk(node: &UiNode, id: &str) -> bool {
                match node {
                    UiNode::Button { id: button, .. } if button == id => true,
                    UiNode::Column { children }
                    | UiNode::Row { children }
                    | UiNode::Section { children, .. } => {
                        children.iter().any(|child| walk(child, id))
                    }
                    _ => false,
                }
            }
            walk(&document.root, id)
        };
        assert!(
            has_toggle(&document, "toggle.alpha") && has_toggle(&document, "toggle.beta"),
            "each configured server gets its own stop control"
        );

        let action = |revision: u64, control_id: &str, name: &str| PluginUiAction {
            surface: request.clone(),
            revision,
            control_id: control_id.into(),
            action: name.into(),
            value: None,
        };
        let stopped = ui
            .handle_action(&action(document.revision, "toggle.alpha", "toggle_server"))
            .await
            .expect("stopping a server returns a newer snapshot");
        let text = format!("{:?}", stopped.root);
        assert!(
            text.contains("已停止"),
            "the stopped server is reported as stopped: {text}"
        );

        let after_stop =
            std::fs::read_to_string(project.path().join(".mcp.json")).expect("the config remains");
        assert!(
            after_stop.contains("\"alpha\""),
            "stopping keeps the declaration so the server can be started again"
        );

        let removed = ui
            .handle_action(&action(stopped.revision, "remove.alpha", "remove_server"))
            .await
            .expect("uninstalling a server returns a newer snapshot");
        let text = format!("{:?}", removed.root);
        assert!(
            !text.contains("alpha"),
            "the removed server is gone from the refreshed snapshot: {text}"
        );
        assert!(text.contains("beta"), "the other server is untouched");
        let rewritten =
            std::fs::read_to_string(project.path().join(".mcp.json")).expect("the config remains");
        let parsed: serde_json::Value =
            serde_json::from_str(&rewritten).expect("the rewritten config is JSON");
        assert!(
            parsed["mcpServers"].get("alpha").is_none() && parsed["mcpServers"]["beta"].is_object(),
            "the uninstalled server is deleted from `.mcp.json` and the rest is preserved: {rewritten}"
        );
        ui.close_surface(&request)
            .await
            .expect("the MCP surface closes");
    }

    #[tokio::test]
    async fn a_guest_error_does_not_reset_a_stopped_mcp_server() {
        let project = tempfile::tempdir().expect("a temporary project root is available");
        let package = crate::plugins::defaults::PLUGINS
            .iter()
            .find(|package| package.name == "mcp")
            .expect("the application ships a bundled MCP Component");
        std::fs::write(
            project.path().join(".mcp.json"),
            r#"{"mcpServers":{"alpha":{"url":"http://127.0.0.1:1/a"}}}"#,
        )
        .expect("the scoped MCP configuration is writable");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions {
                write_plugin_files: true,
                ..Default::default()
            },
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .expect("scope host");
        let actor = ComponentActor::load_bytes(package.component, hub)
            .await
            .expect("the bundled MCP Component reads its scoped configuration");
        let request = SurfaceRequest {
            plugin_id: "mcp@deluxe-defaults".into(),
            project: project.path().to_path_buf(),
            surface_id: "mcp".into(),
            request_id: 1,
        };
        let mut ui = WasmUiExecutor::new(actor.clone());
        let document = ui
            .open_surface(&request)
            .await
            .expect("the MCP surface opens with the server listed");
        let stopped = ui
            .handle_action(&PluginUiAction {
                surface: request.clone(),
                revision: document.revision,
                control_id: "toggle.alpha".into(),
                action: "toggle_server".into(),
                value: None,
            })
            .await
            .expect("stopping the server returns a newer snapshot");
        assert!(
            format!("{:?}", stopped.root).contains("已停止"),
            "the stopped server is reported as stopped"
        );

        // A guest error is normal control flow, not a trap. It must not rebuild
        // the component and wipe the stopped flag the user just set.
        let error = actor
            .call(Operation::Execute {
                name: "mcp__alpha__missing".into(),
                arguments: "{}".into(),
            })
            .await
            .expect_err("a stopped server has no callable tools");
        assert_eq!(error.code, code::PLUGIN_INVALID_OUTPUT);

        let reopened = ui
            .open_surface(&request)
            .await
            .expect("the surface reopens after the guest error");
        assert!(
            format!("{:?}", reopened.root).contains("已停止"),
            "the stopped server stays stopped across a guest error"
        );
        ui.close_surface(&request)
            .await
            .expect("the MCP surface closes");
    }

    #[tokio::test]
    async fn a_stopped_server_is_persisted_and_a_fresh_actor_reads_it_back() {
        let project = tempfile::tempdir().expect("a temporary project root is available");
        let package = crate::plugins::defaults::PLUGINS
            .iter()
            .find(|package| package.name == "mcp")
            .expect("the application ships a bundled MCP Component");
        std::fs::write(
            project.path().join(".mcp.json"),
            r#"{"mcpServers":{"alpha":{"url":"http://127.0.0.1:1/a"}}}"#,
        )
        .expect("the scoped MCP configuration is writable");
        let request = SurfaceRequest {
            plugin_id: "mcp@deluxe-defaults".into(),
            project: project.path().to_path_buf(),
            surface_id: "mcp".into(),
            request_id: 1,
        };
        let hub = || {
            CapabilityHub::new(
                project.path().to_path_buf(),
                project.path().to_path_buf(),
                Permissions {
                    write_plugin_files: true,
                    ..Default::default()
                },
                Arc::new(crate::harness::services::RegistryToolRuntime::new(
                    Arc::new(ToolRegistry::with_builtins()),
                    Arc::new(RwLock::new(ToolSettings::default())),
                )),
            )
            .expect("scope host")
        };

        let first = ComponentActor::load_bytes(package.component, hub())
            .await
            .expect("the bundled MCP Component loads");
        let mut ui = WasmUiExecutor::new(first);
        let document = ui
            .open_surface(&request)
            .await
            .expect("the MCP surface opens with the server listed");
        let stopped = ui
            .handle_action(&PluginUiAction {
                surface: request.clone(),
                revision: document.revision,
                control_id: "toggle.alpha".into(),
                action: "toggle_server".into(),
                value: None,
            })
            .await
            .expect("stopping the server returns a newer snapshot");
        assert!(format!("{:?}", stopped.root).contains("已停止"));

        let persisted =
            std::fs::read_to_string(project.path().join(".mcp.json")).expect("the config remains");
        let parsed: serde_json::Value =
            serde_json::from_str(&persisted).expect("the rewritten config is JSON");
        assert_eq!(
            parsed["mcpServers"]["alpha"]["disabled"],
            serde_json::Value::Bool(true),
            "the stop switch is persisted to `.mcp.json`: {persisted}"
        );

        // A brand-new actor, as after a reload or an application restart, must
        // read the switch back rather than silently starting the server.
        let second = ComponentActor::load_bytes(package.component, hub())
            .await
            .expect("a fresh MCP Component loads");
        let mut fresh_ui = WasmUiExecutor::new(second);
        let reopened = fresh_ui
            .open_surface(&request)
            .await
            .expect("the fresh surface opens");
        assert!(
            format!("{:?}", reopened.root).contains("已停止"),
            "a fresh actor reads the persisted stop"
        );
        fresh_ui
            .close_surface(&request)
            .await
            .expect("the fresh surface closes");
    }

    #[tokio::test]
    async fn wasm_provider_owns_mcp_http_protocol_and_transport_framing() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the loopback listener is available");
        let url = format!(
            "http://{}/mcp",
            listener.local_addr().expect("the listener has an address")
        );
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..4 {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("the provider can connect to the fixture server");
                let body = read_http_request(&mut socket).await;
                let request_id = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|request| request.get("id").and_then(serde_json::Value::as_u64))
                    .unwrap_or_default();
                let response_body = if body.contains(r#""method":"initialize""#) {
                    format!(
                        "data: {{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"result\":\
                         {{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}},\
                         \"serverInfo\":{{\"name\":\"fixture\",\"version\":\"0.1\"}}}}}}\n\n"
                    )
                } else if body.contains(r#""method":"tools/list""#) {
                    format!(
                        "data: {{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"result\":\
                         {{\"tools\":[{{\"name\":\"remote_echo\",\"description\":\"fixture\"}}]}}}}\n\n"
                    )
                } else if body.contains(r#""method":"tools/call""#) {
                    format!(
                        "data: {{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"result\":\
                         {{\"content\":[{{\"type\":\"text\",\"text\":\"server-response\"}}]}}}}\n\n"
                    )
                } else {
                    String::new()
                };
                let status = if body.contains(r#""method":"notifications/initialized""#) {
                    "202 Accepted"
                } else {
                    "200 OK"
                };
                socket
                    .write_all(http_response(status, &response_body).as_bytes())
                    .await
                    .expect("the fixture response head can be written");
                requests.push(body);
            }
            requests
        });

        let plugin_root = tempfile::tempdir().expect("a temporary plugin root is available");
        let project_root = tempfile::tempdir().expect("a temporary project root is available");
        let manifest: WasmManifest = serde_json::from_value(json!({
            "type": "wasm",
            "module": "plugin.wasm",
            "apiVersion": crate::plugins::wasm_manifest::API_VERSION,
            "permissions": {
                "processCommands": ["*"],
                "networkHosts": ["127.0.0.1"],
            }
        }))
        .expect("the dynamic MCP fixture manifest is valid");
        let mcp_json = json!({
            "mcpServers": {
                "fixture": {
                    "type": "http",
                    "url": url,
                }
            }
        })
        .to_string();
        std::fs::write(
            plugin_root.path().join(".mcp.json"),
            r#"{"mcpServers":{"wrong":{"url":"http://127.0.0.1:1/wrong"}}}"#,
        )
        .expect("the plugin-root decoy configuration is writable");
        std::fs::write(
            plugin_root.path().join("plugin.wasm"),
            include_bytes!("../../plugin-fixtures/mcp-client/plugin.wasm"),
        )
        .expect("the MCP component is copied into its plugin root");
        std::fs::write(project_root.path().join(".mcp.json"), &mcp_json)
            .expect("the scoped MCP configuration is writable");
        let hub = CapabilityHub::new(
            project_root.path().to_path_buf(),
            project_root.path().to_path_buf(),
            manifest.permissions.clone(),
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .expect("raw host");
        let actor = ComponentActor::load(plugin_root.path().to_path_buf(), manifest, hub)
            .await
            .expect("the MCP HTTP component implements the harness world");

        let tools = tools(actor)
            .await
            .expect("the provider can discover its remote MCP tools");
        assert_eq!(tools.len(), 1, "the fixture exposes one remote MCP tool");
        assert_eq!(tools[0].descriptor().name, "mcp__fixture__remote_echo");
        let output = tools[0]
            .execute(json!({"text":"hello"}), &ToolSettings::default())
            .await
            .expect("the provider can call the remote MCP tool");
        assert_eq!(output.as_text(), "server-response");

        let requests = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the fixture server receives every provider request")
            .expect("the fixture server task exits successfully");
        assert_eq!(
            requests.len(),
            4,
            "discovery and invocation use one handshake and one request each"
        );
        assert!(
            requests
                .iter()
                .filter(|body| body.contains(r#""method":"initialize""#))
                .count()
                == 1,
            "the provider performs one MCP initialize handshake per server"
        );
        assert!(
            requests
                .iter()
                .any(|body| body.contains(r#""method":"tools/list""#)),
            "the provider owns tools/list"
        );
        assert!(
            requests.iter().any(|body| {
                body.contains(r#""method":"tools/call""#)
                    && body.contains(r#""name":"remote_echo""#)
                    && body.contains(r#""text":"hello""#)
            }),
            "the provider owns tools/call and forwards arguments"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn wasm_provider_owns_mcp_stdio_protocol_and_transport_framing() {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::WARN)
            .try_init();
        let script = r#"$line = [Console]::In.ReadLine(); while ($null -ne $line) {
            $request = $line | ConvertFrom-Json
            if ($request.method -eq 'initialize') {
                $response = @{jsonrpc='2.0'; id=$request.id; result=@{protocolVersion='2025-06-18'; capabilities=@{}; serverInfo=@{name='fixture'; version='0.1'}}} | ConvertTo-Json -Compress -Depth 10
                [Console]::Out.WriteLine($response)
            } elseif ($request.method -eq 'tools/list') {
                $response = @{jsonrpc='2.0'; id=$request.id; result=@{tools=@(@{name='remote_echo'; description='fixture'})}} | ConvertTo-Json -Compress -Depth 10
                [Console]::Out.WriteLine($response)
            } elseif ($request.method -eq 'tools/call') {
                $response = @{jsonrpc='2.0'; id=$request.id; result=@{content=@(@{type='text'; text='server-response'})}} | ConvertTo-Json -Compress -Depth 10
                [Console]::Out.WriteLine($response)
            }
            $line = [Console]::In.ReadLine()
        }"#;
        let args = vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script.to_string(),
        ];
        let plugin_root = tempfile::tempdir().expect("a temporary plugin root is available");
        let project_root = tempfile::tempdir().expect("a temporary project root is available");
        let manifest: WasmManifest = serde_json::from_value(json!({
            "type": "wasm",
            "module": "plugin.wasm",
            "apiVersion": crate::plugins::wasm_manifest::API_VERSION,
            "permissions": {
                "processCommands": ["powershell.exe"],
            }
        }))
        .expect("the dynamic MCP stdio manifest is valid");
        let mcp_json = json!({
            "mcpServers": {
                "fixture": {
                    "type": "stdio",
                    "command": "powershell.exe",
                    "args": args,
                    "env": {},
                }
            }
        })
        .to_string();
        std::fs::write(
            plugin_root.path().join(".mcp.json"),
            r#"{"mcpServers":{"wrong":{"command":"not-the-fixture"}}}"#,
        )
        .expect("the plugin-root decoy configuration is writable");
        std::fs::write(
            plugin_root.path().join("plugin.wasm"),
            include_bytes!("../../plugin-fixtures/mcp-client/plugin.wasm"),
        )
        .expect("the MCP component is copied into its plugin root");
        std::fs::write(project_root.path().join(".mcp.json"), &mcp_json)
            .expect("the scoped MCP configuration is writable");
        let hub = CapabilityHub::new(
            project_root.path().to_path_buf(),
            project_root.path().to_path_buf(),
            manifest.permissions.clone(),
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .expect("raw host");
        let raw_process_hub = hub.clone();
        let raw_handle = raw_process_hub
            .spawn_process(
                "powershell.exe",
                &serde_json::to_string(&args).expect("stdio arguments serialize"),
                "",
                "{}",
            )
            .await
            .expect("the raw process capability starts the declared server");
        raw_process_hub
            .process_write(
                raw_handle,
                br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            )
            .await
            .expect("the raw process capability writes provider bytes");
        raw_process_hub
            .process_write(raw_handle, b"\n")
            .await
            .expect("the raw process capability writes provider framing");
        let raw_response = raw_process_hub
            .process_read(raw_handle, MAX_PAYLOAD_BYTES as u32)
            .await
            .expect("the raw process capability reads provider bytes");
        assert!(
            String::from_utf8_lossy(&raw_response).contains("protocolVersion"),
            "the declared process must answer the raw JSON-RPC line"
        );
        raw_process_hub.process_close(raw_handle).await;
        let actor = ComponentActor::load(plugin_root.path().to_path_buf(), manifest, hub)
            .await
            .expect("the MCP stdio component implements the harness world");

        let tools = tools(actor.clone())
            .await
            .expect("the provider can discover its stdio MCP tools");
        assert_eq!(tools.len(), 1, "the stdio fixture exposes one remote tool");
        assert_eq!(tools[0].descriptor().name, "mcp__fixture__remote_echo");
        let output = tools[0]
            .execute(json!({"text":"hello"}), &ToolSettings::default())
            .await
            .expect("the provider can call the stdio MCP tool");
        assert_eq!(output.as_text(), "server-response");

        actor.shutdown();
    }

    #[test]
    fn component_outputs_can_return_images() {
        let output = serde_json::json!({
            "content": [{
                "type": "image",
                "image": {
                    "id": "plugin-owned",
                    "mediaType": "image/png",
                    "bytes": 12,
                    "width": 2,
                    "height": 2
                }
            }],
            "isError": false
        });
        let decoded = decode_tool_output(&output.to_string())
            .expect("plugin image output is allowed by the component protocol");
        assert_eq!(decoded.images()[0].id, "plugin-owned");
    }

    #[test]
    fn component_outputs_preserve_host_patch_hunks() {
        let output = serde_json::json!({
            "content": [{"type": "text", "text": "Applied 1 patch operation"}],
            "isError": false,
            "hunks": [{"path": "src/main.rs", "lines": [7, null, 8]}]
        });
        let decoded = decode_tool_output(&output.to_string())
            .expect("host apply_patch metadata is valid component output");
        assert_eq!(decoded.hunks.len(), 1);
        assert_eq!(decoded.hunks[0].path, "src/main.rs");
        assert_eq!(decoded.hunks[0].lines, vec![Some(7), None, Some(8)]);
    }

    #[test]
    fn component_outputs_can_return_patch_hunks() {
        let output = serde_json::json!({
            "content": [{"type": "text", "text": "done"}],
            "isError": false,
            "hunks": [{"path": "src/main.rs", "lines": [7]}]
        });
        let decoded = decode_tool_output(&output.to_string())
            .expect("plugin patch metadata is allowed by the component protocol");
        assert_eq!(decoded.hunks[0].path, "src/main.rs");
    }

    #[test]
    fn bundled_provider_outputs_can_preserve_host_images() {
        let output = serde_json::json!({
            "content": [{
                "type": "image",
                "image": {
                    "id": "host-owned",
                    "mediaType": "image/png",
                    "bytes": 12,
                    "width": 2,
                    "height": 2
                }
            }],
            "isError": false
        });
        let decoded = decode_tool_output(&output.to_string())
            .expect("a component can return an image reference");
        assert_eq!(decoded.images()[0].id, "host-owned");
    }

    #[test]
    fn component_output_payloads_are_bounded() {
        let error = decode_tool_output(&"x".repeat(MAX_PAYLOAD_BYTES + 1))
            .expect_err("oversized guest output is denied");
        assert_eq!(error.code, code::PLUGIN_INVALID_OUTPUT);
    }
}
