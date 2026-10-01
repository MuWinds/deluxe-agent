//! Converts component exports into the existing host tool and UI ports.

use std::sync::Arc;

use serde_json::Value;

use crate::error::{code, AgentError, Result};
use crate::tools::{ContentBlock, Tool, ToolDescriptor, ToolOutput, ToolSettings};

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

/// Decodes a tool result without accepting fabricated host image references.
///
/// Returns `Err` for malformed data or unsupported output blocks. Images must
/// originate from a future explicit host attachment capability, not guest JSON.
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
    if output
        .content
        .iter()
        .any(|block| !matches!(block, ContentBlock::Text { .. }))
        || !output.hunks.is_empty()
    {
        return Err(AgentError::new(
            code::PLUGIN_INVALID_OUTPUT,
            "Component returned unowned host resources",
        ));
    }
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
    use crate::plugins::providers::mcp_tools;
    use crate::plugins::runtime::PluginUiExecutor;
    use crate::plugins::ui_protocol::{PluginUiAction, SurfaceRequest, UiNode};
    use crate::plugins::wasm_manifest::WasmManifest;
    use crate::plugins::wasm_runtime::ProviderInputs;
    use crate::tools::{ToolRegistry, ToolSettings};

    fn fixture_manifest() -> WasmManifest {
        let manifest: crate::plugins::PluginManifest = serde_json::from_str(include_str!(
            "../../plugin-fixtures/echo-tool/.codex-plugin/plugin.json"
        ))
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
            ProviderInputs::default(),
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
        let root = tempfile::tempdir().expect("a temporary plugin root is available");
        let manifest = serde_json::from_str::<crate::plugins::PluginManifest>(include_str!(
            "../../plugin-fixtures/hooks-provider/.codex-plugin/plugin.json"
        ))
        .expect("the checked-in Hooks manifest is valid")
        .wasm_runtime()
        .expect("the checked-in Hooks manifest declares a Wasmtime runtime");
        let hooks_json =
            include_str!("../../plugin-fixtures/hooks-provider/hooks.json").to_string();
        let hub = fixture_hub(root.path().to_path_buf());
        let actor = ComponentActor::load_bytes(
            include_bytes!("../../plugin-fixtures/hooks-provider/plugin.wasm"),
            root.path().to_path_buf(),
            manifest,
            ProviderInputs {
                hooks_json: Some(hooks_json),
                ..Default::default()
            },
            hub,
        )
        .await
        .expect("the Hooks provider implements the harness world");

        let hooks = actor
            .call(Operation::ListHooks)
            .await
            .expect("the Hooks provider lists its configured hooks");
        let hooks: serde_json::Value = serde_json::from_str(&hooks).expect("the hook list is JSON");
        assert_eq!(
            hooks[0]["label"], "echo hooks-provider-ran",
            "the provider reads hooks.json instead of receiving manifest metadata"
        );
        actor.shutdown();
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
            for _ in 0..6 {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("the provider can connect to the fixture server");
                let body = read_http_request(&mut socket).await;
                let response_body = if body.contains(r#""method":"initialize""#) {
                    concat!(
                        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":",
                        "{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},",
                        "\"serverInfo\":{\"name\":\"fixture\",\"version\":\"0.1\"}}}\n\n"
                    )
                } else if body.contains(r#""method":"tools/list""#) {
                    concat!(
                        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":",
                        "[{\"name\":\"remote_echo\",\"description\":\"fixture\"}]}}\n\n"
                    )
                } else if body.contains(r#""method":"tools/call""#) {
                    concat!(
                        "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":",
                        "[{\"type\":\"text\",\"text\":\"server-response\"}]}}\n\n"
                    )
                } else {
                    ""
                };
                let status = if body.contains(r#""method":"notifications/initialized""#) {
                    "202 Accepted"
                } else {
                    "200 OK"
                };
                socket
                    .write_all(http_response(status, response_body).as_bytes())
                    .await
                    .expect("the fixture response head can be written");
                requests.push(body);
            }
            requests
        });

        let root = tempfile::tempdir().expect("a temporary plugin root is available");
        let manifest: WasmManifest = serde_json::from_value(json!({
            "type": "wasm",
            "module": "plugin.wasm",
            "apiVersion": crate::plugins::wasm_manifest::API_VERSION,
            "permissions": {
                "mcpServers": ["fixture"],
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
        let mcp_servers = serde_json::from_value::<
            std::collections::BTreeMap<String, crate::plugins::manifest::McpServerConfig>,
        >(json!({
            "fixture": {
                "type": "http",
                "url": url,
            }
        }))
        .expect("the dynamic MCP declarations are valid");
        let hub = CapabilityHub::new(
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            manifest.permissions.clone(),
            mcp_servers,
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                Arc::new(RwLock::new(ToolSettings::default())),
            )),
        )
        .expect("raw host");
        let actor = ComponentActor::load_bytes(
            include_bytes!("../../plugin-fixtures/mcp-http/plugin.wasm"),
            root.path().to_path_buf(),
            manifest,
            ProviderInputs {
                mcp_json: Some(mcp_json),
                ..Default::default()
            },
            hub,
        )
        .await
        .expect("the MCP HTTP component implements the harness world");

        let tools = mcp_tools(actor, "mcp-http-fixture")
            .await
            .expect("the provider can discover its remote MCP tools");
        assert_eq!(tools.len(), 1, "the fixture exposes one remote MCP tool");
        assert_eq!(tools[0].descriptor().name, "mcp__fixture__remote_echo");
        let output = tools[0]
            .execute(json!({"text":"hello"}), &ToolSettings::default())
            .await
            .expect("the provider can call the remote MCP tool");
        assert_eq!(output.as_text(), "mcp-wasm-transport");

        let requests = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the fixture server receives every provider request")
            .expect("the fixture server task exits successfully");
        assert_eq!(
            requests.len(),
            6,
            "discovery and invocation use six MCP requests"
        );
        assert!(
            requests
                .iter()
                .filter(|body| body.contains(r#""method":"initialize""#))
                .count()
                == 2,
            "the provider performs an MCP initialize handshake for each operation"
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
        let root = tempfile::tempdir().expect("a temporary plugin root is available");
        let manifest: WasmManifest = serde_json::from_value(json!({
            "type": "wasm",
            "module": "plugin.wasm",
            "apiVersion": crate::plugins::wasm_manifest::API_VERSION,
            "permissions": {
                "mcpServers": ["fixture"],
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
        let mcp_servers = serde_json::from_value::<
            std::collections::BTreeMap<String, crate::plugins::manifest::McpServerConfig>,
        >(json!({
            "fixture": {
                "type": "stdio",
                "command": "powershell.exe",
                "args": args,
                "env": {},
            }
        }))
        .expect("the dynamic MCP declarations are valid");
        let hub = CapabilityHub::new(
            root.path().to_path_buf(),
            root.path().to_path_buf(),
            manifest.permissions.clone(),
            mcp_servers,
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
        let actor = ComponentActor::load_bytes(
            include_bytes!("../../plugin-fixtures/mcp-http/plugin.wasm"),
            root.path().to_path_buf(),
            manifest,
            ProviderInputs {
                mcp_json: Some(mcp_json),
                ..Default::default()
            },
            hub,
        )
        .await
        .expect("the MCP stdio component implements the harness world");

        let tools = mcp_tools(actor.clone(), "mcp-stdio-fixture")
            .await
            .expect("the provider can discover its stdio MCP tools");
        assert_eq!(tools.len(), 1, "the stdio fixture exposes one remote tool");
        assert_eq!(tools[0].descriptor().name, "mcp__fixture__remote_echo");
        let output = tools[0]
            .execute(json!({"text":"hello"}), &ToolSettings::default())
            .await
            .expect("the provider can call the stdio MCP tool");
        assert_eq!(output.as_text(), "mcp-wasm-transport");

        actor.shutdown();
    }

    #[test]
    fn component_outputs_cannot_fabricate_host_owned_resources() {
        let output = serde_json::json!({
            "content": [{
                "type": "image",
                "image": {"id": "forged", "path": "outside"}
            }],
            "isError": false
        });
        let error = decode_tool_output(&output.to_string()).expect_err("guest images are denied");
        assert_eq!(error.code, code::PLUGIN_INVALID_OUTPUT);
    }

    #[test]
    fn component_output_payloads_are_bounded() {
        let error = decode_tool_output(&"x".repeat(MAX_PAYLOAD_BYTES + 1))
            .expect_err("oversized guest output is denied");
        assert_eq!(error.code, code::PLUGIN_INVALID_OUTPUT);
    }
}
