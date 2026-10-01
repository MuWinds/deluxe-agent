use std::sync::{Mutex, OnceLock};

wit_bindgen::generate!({
    path: "../../wit",
    world: "harness-plugin",
    async: true,
});

use exports::deluxe::harness::plugin::Guest;

struct McpHttpFixture;

const TOOL_DESCRIPTOR: &str = r#"[{"name":"remote_echo","summary":"MCP echo fixture","description":"Calls an HTTP MCP server through provider-owned framing","guidelines":[],"inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]},"hostValidatesArguments":false,"mutating":false}]"#;

#[derive(Clone)]
enum TransportConfig {
    Http {
        url: String,
    },
    Stdio {
        command: String,
        arguments_json: String,
        cwd: String,
        environment_json: String,
    },
}

static MCP_CONFIG: OnceLock<Mutex<Option<TransportConfig>>> = OnceLock::new();
static PROCESS_HANDLE: OnceLock<Mutex<Option<u64>>> = OnceLock::new();
static PROCESS_PENDING: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();

fn configured_transport(config: &str) -> Result<TransportConfig, String> {
    let root: serde_json::Value =
        serde_json::from_str(config).map_err(|_| "MCP fixture config is invalid".to_string())?;
    let mcp_json = root
        .get("mcpJson")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "MCP fixture `.mcp.json` input is missing".to_string())?;
    let mcp: serde_json::Value = serde_json::from_str(mcp_json)
        .map_err(|_| "MCP fixture `.mcp.json` is invalid".to_string())?;
    let server = mcp
        .get("mcpServers")
        .and_then(|servers| servers.get("fixture"))
        .ok_or_else(|| "MCP fixture declaration is missing".to_string())?;
    if let Some(url) = server.get("url").and_then(serde_json::Value::as_str) {
        return Ok(TransportConfig::Http { url: url.into() });
    }
    let command = server
        .get("command")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "MCP fixture transport command is missing".to_string())?;
    let arguments = server
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let environment = server
        .get("env")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(TransportConfig::Stdio {
        command: command.into(),
        arguments_json: serde_json::to_string(&arguments)
            .map_err(|_| "MCP fixture arguments are invalid".to_string())?,
        cwd: server
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .into(),
        environment_json: serde_json::to_string(&environment)
            .map_err(|_| "MCP fixture environment is invalid".to_string())?,
    })
}

fn mcp_config() -> Result<TransportConfig, String> {
    MCP_CONFIG
        .get()
        .ok_or_else(|| String::from("MCP fixture is not configured"))?
        .lock()
        .map_err(|_| "MCP fixture configuration is poisoned".to_string())
        .and_then(|config| {
            config
                .clone()
                .ok_or_else(|| "MCP fixture is not configured".to_string())
        })
}

fn response_handle(metadata: &str) -> Result<u64, String> {
    let marker = r#""handle":"#;
    let Some(start) = metadata.find(marker) else {
        return Err("HTTP response handle is missing".into());
    };
    let value = &metadata[start + marker.len()..];
    let end = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    value[..end]
        .parse()
        .map_err(|_| "HTTP response handle is invalid".into())
}

async fn read_response(metadata: String) -> Result<String, String> {
    let handle = response_handle(&metadata)?;
    let mut bytes = Vec::new();
    loop {
        let chunk = deluxe::harness::host::http_read(handle, 64 * 1024).await?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() > 256 * 1024 {
            deluxe::harness::host::http_close(handle).await;
            return Err("MCP response exceeds the fixture limit".into());
        }
    }
    deluxe::harness::host::http_close(handle).await;
    String::from_utf8(bytes).map_err(|_| "MCP response is not UTF-8".into())
}

async fn process_handle(config: &TransportConfig) -> Result<u64, String> {
    let TransportConfig::Stdio {
        command,
        arguments_json,
        cwd,
        environment_json,
    } = config
    else {
        return Err("MCP fixture is not configured for stdio".into());
    };
    let state = PROCESS_HANDLE.get_or_init(|| Mutex::new(None));
    if let Some(handle) = state
        .lock()
        .map_err(|_| "MCP fixture process state is poisoned".to_string())?
        .as_ref()
        .copied()
    {
        return Ok(handle);
    }
    let handle = deluxe::harness::host::spawn_process(
        command.clone(),
        arguments_json.clone(),
        cwd.clone(),
        environment_json.clone(),
    )
    .await?;
    state
        .lock()
        .map_err(|_| "MCP fixture process state is poisoned".to_string())?
        .replace(handle);
    Ok(handle)
}

async fn read_process_line(handle: u64) -> Result<String, String> {
    let pending = PROCESS_PENDING.get_or_init(|| Mutex::new(Vec::new()));
    loop {
        let line = {
            let mut bytes = pending
                .lock()
                .map_err(|_| "MCP fixture process buffer is poisoned".to_string())?;
            bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|line| bytes.drain(..=line).collect::<Vec<_>>())
        };
        if let Some(result) = line {
            return String::from_utf8(result)
                .map(|line| line.trim_end_matches(['\r', '\n']).to_string())
                .map_err(|_| "MCP stdio response is not UTF-8".into());
        }
        let chunk = deluxe::harness::host::process_read(handle, 64 * 1024).await?;
        if chunk.is_empty() {
            return Err("MCP stdio server closed its output".into());
        }
        pending
            .lock()
            .map_err(|_| "MCP fixture process buffer is poisoned".to_string())?
            .extend_from_slice(&chunk);
    }
}

async fn request_stdio(
    config: &TransportConfig,
    body: String,
    expects_response: bool,
) -> Result<String, String> {
    let handle = process_handle(config).await?;
    let mut framed = body.into_bytes();
    framed.push(b'\n');
    deluxe::harness::host::process_write(handle, framed).await?;
    if expects_response {
        read_process_line(handle).await
    } else {
        Ok(String::new())
    }
}

async fn request(
    config: &TransportConfig,
    body: String,
    expects_response: bool,
) -> Result<String, String> {
    match config {
        TransportConfig::Http { url } => {
            let metadata = deluxe::harness::host::http_request(
                "POST".into(),
                url.clone(),
                r#"{"content-type":"application/json","accept":"text/event-stream"}"#.into(),
                body,
            )
            .await?;
            if expects_response {
                read_response(metadata).await
            } else {
                let handle = response_handle(&metadata)?;
                deluxe::harness::host::http_close(handle).await;
                Ok(String::new())
            }
        }
        TransportConfig::Stdio { .. } => request_stdio(config, body, expects_response).await,
    }
}

fn has_jsonrpc_result(response: &str, field: &str) -> bool {
    let payload = response
        .find("data:")
        .map(|start| &response[start + "data:".len()..])
        .unwrap_or(response);
    payload.contains(r#""jsonrpc":"2.0""#)
        && payload.contains(r#""result":"#)
        && payload.contains(field)
}

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

fn ui_document(request: &str) -> String {
    let plugin_id = request_field(request, "pluginId", "mcp-http-fixture@test");
    let surface_id = request_field(request, "surfaceId", "mcp");
    serde_json::json!({
        "schemaVersion": 1,
        "pluginId": plugin_id,
        "surfaceId": surface_id,
        "revision": 1,
        "title": "MCP",
        "root": {
            "type": "column",
            "children": [
                {
                    "type": "text",
                    "text": "Configured MCP servers",
                    "emphasis": "strong"
                },
                {
                    "type": "section",
                    "id": "fixture",
                    "title": "fixture",
                    "children": [
                        {
                            "type": "text",
                            "text": "remote_echo",
                            "emphasis": "normal"
                        }
                    ]
                }
            ]
        }
    })
    .to_string()
}

async fn initialize(config: &TransportConfig) -> Result<(), String> {
    let response = request(
        config,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"deluxe-fixture","version":"0.1"}}}"#.into(),
        true,
    )
    .await?;
    if !has_jsonrpc_result(&response, r#""protocolVersion":"#) {
        return Err("MCP initialize response is invalid".into());
    }
    request(
        config,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.into(),
        false,
    )
    .await?;
    Ok(())
}

async fn list_remote_tools(config: &TransportConfig) -> Result<(), String> {
    let response = request(
        config,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#.into(),
        true,
    )
    .await?;
    if !has_jsonrpc_result(&response, "remote_echo") {
        return Err("MCP tools/list response is invalid".into());
    }
    Ok(())
}

impl Guest for McpHttpFixture {
    async fn configure(config_json: String) -> Result<(), String> {
        let config = configured_transport(&config_json)?;
        MCP_CONFIG
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| "MCP fixture configuration is poisoned".to_string())?
            .replace(config);
        PROCESS_HANDLE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| "MCP fixture process state is poisoned".to_string())?
            .take();
        PROCESS_PENDING
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .map_err(|_| "MCP fixture process buffer is poisoned".to_string())?
            .clear();
        Ok(())
    }

    async fn list_tools() -> String {
        "[]".into()
    }

    async fn execute_tool(_name: String, _arguments_json: String) -> Result<String, String> {
        Err("MCP fixture has no regular tools".into())
    }

    async fn list_hooks() -> String {
        "[]".into()
    }

    async fn invoke_hook(_hook_id: String, _event_json: String) -> Result<String, String> {
        Err("MCP fixture has no hooks".into())
    }

    async fn list_mcp_servers() -> String {
        r#"["fixture"]"#.into()
    }

    async fn list_mcp_tools(server: String) -> String {
        if server != "fixture" {
            return "[]".into();
        }
        let Ok(config) = mcp_config() else {
            return "[]".into();
        };
        if initialize(&config).await.is_err() || list_remote_tools(&config).await.is_err() {
            return "[]".into();
        }
        TOOL_DESCRIPTOR.into()
    }

    async fn invoke_mcp_tool(
        server: String,
        _name: String,
        arguments_json: String,
    ) -> Result<String, String> {
        if server != "fixture" {
            return Err("unknown MCP server".into());
        }
        let config = mcp_config()?;
        initialize(&config).await?;
        let response = request(
            &config,
            format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"remote_echo","arguments":{arguments_json}}}}}"#
            ),
            true,
        )
        .await?;
        if !has_jsonrpc_result(&response, r#""content":"#) {
            return Err("MCP tools/call response is invalid".into());
        }
        Ok(r#"{"content":[{"type":"text","text":"mcp-wasm-transport"}],"isError":false}"#.into())
    }

    async fn open_surface(_request_json: String) -> Result<String, String> {
        Ok(ui_document(&_request_json))
    }

    async fn handle_action(_action_json: String) -> Result<String, String> {
        Err("MCP fixture has no UI actions".into())
    }

    async fn close_surface(_surface_id: String) {}
}

export!(McpHttpFixture);
