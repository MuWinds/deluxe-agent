use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

wit_bindgen::generate!({
    path: "../../wit",
    world: "harness-plugin",
    async: true,
});

use exports::deluxe::harness::plugin::Guest;

struct McpProvider;

const SERVER_PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_DESCRIPTION_CHARS: usize = 200;

#[derive(Clone)]
enum TransportConfig {
    Http {
        url: String,
        version: String,
        session: Option<String>,
    },
    Stdio {
        command: String,
        arguments_json: String,
        cwd: String,
        environment_json: String,
    },
}

#[derive(Clone)]
struct ServerState {
    transport: TransportConfig,
    next_id: u64,
    initialized: bool,
    /// Whether the server is stopped: it is neither connected nor listed, but
    /// its declaration stays in `.mcp.json` so it can be started again.
    stopped: bool,
}

static MCP_CONFIG: OnceLock<Mutex<BTreeMap<String, ServerState>>> = OnceLock::new();
static PROCESS_HANDLES: OnceLock<Mutex<BTreeMap<String, u64>>> = OnceLock::new();
static PROCESS_PENDING: OnceLock<Mutex<BTreeMap<String, Vec<u8>>>> = OnceLock::new();

async fn configured_servers() -> Result<BTreeMap<String, ServerState>, String> {
    let bytes = match deluxe::harness::host::read_plugin_file(".mcp.json".into()).await {
        Ok(bytes) => bytes,
        Err(error) if error.starts_with("plugin_file_not_found:") => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let mcp_json = String::from_utf8(bytes)
        .map_err(|_| "MCP provider `.mcp.json` is not UTF-8".to_string())?;
    let mcp: Value = serde_json::from_str(&mcp_json)
        .map_err(|_| "MCP provider `.mcp.json` is invalid".to_string())?;
    let Some(servers) = mcp.get("mcpServers").and_then(Value::as_object) else {
        return Ok(BTreeMap::new());
    };

    let mut result = BTreeMap::new();
    for (name, server) in servers {
        let Some(config) = transport_config(server)? else {
            continue;
        };
        // `disabled` is the persisted stop switch, so a stopped server stays
        // stopped across a reload, a fresh actor, or an application restart —
        // not just for the lifetime of one Wasm instance.
        let stopped = server
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        result.insert(
            name.clone(),
            ServerState {
                transport: config,
                next_id: 1,
                initialized: false,
                stopped,
            },
        );
    }
    Ok(result)
}

fn transport_config(server: &Value) -> Result<Option<TransportConfig>, String> {
    if let Some(url) = server.get("url").and_then(Value::as_str) {
        return Ok(Some(TransportConfig::Http {
            url: url.to_string(),
            version: SERVER_PROTOCOL_VERSION.to_string(),
            session: None,
        }));
    }

    let Some(command) = server
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
    else {
        return Ok(None);
    };
    let arguments = server.get("args").cloned().unwrap_or_else(|| json!([]));
    let environment = server.get("env").cloned().unwrap_or_else(|| json!({}));
    if !arguments.is_array() || !environment.is_object() {
        return Err("MCP stdio args must be an array and env must be an object".into());
    }
    Ok(Some(TransportConfig::Stdio {
        command: command.to_string(),
        arguments_json: serde_json::to_string(&arguments)
            .map_err(|_| "MCP stdio arguments are invalid".to_string())?,
        cwd: server
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        environment_json: serde_json::to_string(&environment)
            .map_err(|_| "MCP stdio environment is invalid".to_string())?,
    }))
}

fn configured_state(server: &str) -> Result<ServerState, String> {
    MCP_CONFIG
        .get()
        .ok_or_else(|| "MCP provider is not configured".to_string())?
        .lock()
        .map_err(|_| "MCP provider configuration is poisoned".to_string())?
        .get(server)
        .cloned()
        .ok_or_else(|| format!("MCP server `{server}` is not configured"))
}

fn configured_names() -> Result<Vec<String>, String> {
    Ok(MCP_CONFIG
        .get()
        .ok_or_else(|| "MCP provider is not configured".to_string())?
        .lock()
        .map_err(|_| "MCP provider configuration is poisoned".to_string())?
        .keys()
        .cloned()
        .collect())
}

/// The servers a call may reach: every configured one that is not stopped.
fn active_names() -> Result<Vec<String>, String> {
    Ok(MCP_CONFIG
        .get()
        .ok_or_else(|| "MCP provider is not configured".to_string())?
        .lock()
        .map_err(|_| "MCP provider configuration is poisoned".to_string())?
        .iter()
        .filter(|(_, state)| !state.stopped)
        .map(|(name, _)| name.clone())
        .collect())
}

fn server_stopped(server: &str) -> bool {
    MCP_CONFIG
        .get()
        .and_then(|config| config.lock().ok())
        .and_then(|config| config.get(server).map(|state| state.stopped))
        .unwrap_or(false)
}

fn set_stopped(server: &str, stopped: bool) -> Result<(), String> {
    MCP_CONFIG
        .get()
        .ok_or_else(|| "MCP provider is not configured".to_string())?
        .lock()
        .map_err(|_| "MCP provider configuration is poisoned".to_string())?
        .get_mut(server)
        .map(|state| state.stopped = stopped)
        .ok_or_else(|| format!("MCP server `{server}` is not configured"))
}

/// Drops a server from the in-memory set after its declaration was removed.
fn forget(server: &str) {
    if let Some(config) = MCP_CONFIG.get() {
        if let Ok(mut config) = config.lock() {
            config.remove(server);
        }
    }
}

/// A copy of the configured servers, for building a snapshot.
fn snapshot() -> Result<BTreeMap<String, ServerState>, String> {
    MCP_CONFIG
        .get()
        .ok_or_else(|| "MCP provider is not configured".to_string())?
        .lock()
        .map_err(|_| "MCP provider configuration is poisoned".to_string())
        .map(|config| config.clone())
}

/// Drops a server's live connection without touching its declaration.
async fn disconnect(server: &str) {
    let handle = PROCESS_HANDLES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .ok()
        .and_then(|mut handles| handles.remove(server));
    if let Some(handle) = handle {
        deluxe::harness::host::process_close(handle).await;
    }
    if let Some(pending) = PROCESS_PENDING.get() {
        if let Ok(mut pending) = pending.lock() {
            pending.remove(server);
        }
    }
}

fn update_http_state(server: &str, session: Option<String>, version: Option<String>) {
    let Some(config) = MCP_CONFIG.get() else {
        return;
    };
    let Ok(mut config) = config.lock() else {
        return;
    };
    let Some(ServerState {
        transport:
            TransportConfig::Http {
                session: current_session,
                version: current_version,
                ..
            },
        ..
    }) = config.get_mut(server)
    else {
        return;
    };
    if session.is_some() {
        *current_session = session;
    }
    if let Some(version) = version {
        *current_version = version;
    }
}

async fn close_processes() {
    let handles = PROCESS_HANDLES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .ok()
        .map(|mut handles| std::mem::take(&mut *handles))
        .unwrap_or_default();
    for handle in handles.into_values() {
        deluxe::harness::host::process_close(handle).await;
    }
    if let Some(pending) = PROCESS_PENDING.get() {
        if let Ok(mut pending) = pending.lock() {
            pending.clear();
        }
    }
}

async fn process_handle(server: &str, config: &TransportConfig) -> Result<u64, String> {
    let TransportConfig::Stdio {
        command,
        arguments_json,
        cwd,
        environment_json,
    } = config
    else {
        return Err("MCP server is not configured for stdio".into());
    };
    let handles = PROCESS_HANDLES.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(handle) = handles
        .lock()
        .map_err(|_| "MCP process state is poisoned".to_string())?
        .get(server)
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
    handles
        .lock()
        .map_err(|_| "MCP process state is poisoned".to_string())?
        .insert(server.to_string(), handle);
    Ok(handle)
}

async fn read_process_line(server: &str, handle: u64) -> Result<String, String> {
    let pending = PROCESS_PENDING.get_or_init(|| Mutex::new(BTreeMap::new()));
    loop {
        let line = {
            let mut pending = pending
                .lock()
                .map_err(|_| "MCP process buffer is poisoned".to_string())?;
            let bytes = pending.entry(server.to_string()).or_default();
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
            .map_err(|_| "MCP process buffer is poisoned".to_string())?
            .entry(server.to_string())
            .or_default()
            .extend_from_slice(&chunk);
    }
}

fn response_metadata(metadata: &str) -> Result<(u64, u16, BTreeMap<String, String>), String> {
    let value: Value = serde_json::from_str(metadata)
        .map_err(|_| "MCP HTTP response metadata is invalid".to_string())?;
    let handle = value
        .get("handle")
        .and_then(Value::as_u64)
        .ok_or_else(|| "MCP HTTP response handle is missing".to_string())?;
    let status = value
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .ok_or_else(|| "MCP HTTP response status is missing".to_string())?;
    let headers = value
        .get("headers")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok((handle, status, headers))
}

fn header(headers: &BTreeMap<String, String>, name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

async fn read_http_response(
    metadata: String,
) -> Result<(String, BTreeMap<String, String>), String> {
    let (handle, status, headers) = response_metadata(&metadata)?;
    let mut bytes = Vec::new();
    loop {
        let chunk = deluxe::harness::host::http_read(handle, 64 * 1024).await?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() > 256 * 1024 {
            deluxe::harness::host::http_close(handle).await;
            return Err("MCP response exceeds the provider limit".into());
        }
    }
    deluxe::harness::host::http_close(handle).await;
    if !(200..300).contains(&status) {
        return Err(format!("MCP HTTP server answered with status {status}"));
    }
    String::from_utf8(bytes)
        .map(|body| (body, headers))
        .map_err(|_| "MCP HTTP response is not UTF-8".into())
}

fn response_message(body: &str, wanted: u64) -> Result<Value, String> {
    let normalized = body.replace("\r\n", "\n");
    let mut candidates = Vec::new();
    if let Ok(message) = serde_json::from_str::<Value>(&normalized) {
        candidates.push(message);
    }
    for frame in normalized.split("\n\n") {
        let mut payload = String::new();
        for line in frame.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(data.trim_start());
        }
        if let Ok(message) = serde_json::from_str::<Value>(&payload) {
            candidates.push(message);
        }
    }
    let message = candidates
        .into_iter()
        .find(|message| message.get("id").and_then(Value::as_u64) == Some(wanted))
        .ok_or_else(|| "MCP response did not contain the requested JSON-RPC result".to_string())?;
    if let Some(error) = message.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let detail = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown MCP error");
        return Err(format!(
            "MCP server rejected the request ({code}): {detail}"
        ));
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

async fn request(
    server: &str,
    method: &str,
    params: Value,
    expects_response: bool,
) -> Result<Option<Value>, String> {
    let mut state = configured_state(server)?;
    let id = state.next_id;
    if expects_response {
        state.next_id = state.next_id.saturating_add(1);
        if let Some(config) = MCP_CONFIG.get() {
            if let Some(current) = config
                .lock()
                .map_err(|_| "MCP provider configuration is poisoned".to_string())?
                .get_mut(server)
            {
                current.next_id = state.next_id;
            }
        }
    }
    let message = if expects_response {
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    } else {
        json!({"jsonrpc":"2.0","method":method,"params":params})
    };
    let body = serde_json::to_string(&message)
        .map_err(|_| "MCP JSON-RPC request is not serializable".to_string())?;

    match state.transport {
        TransportConfig::Http {
            url,
            version,
            session,
        } => {
            let mut headers = BTreeMap::from([
                ("content-type".to_string(), "application/json".to_string()),
                (
                    "accept".to_string(),
                    "application/json, text/event-stream".to_string(),
                ),
                ("mcp-protocol-version".to_string(), version),
            ]);
            if let Some(session) = session {
                headers.insert("mcp-session-id".into(), session);
            }
            let metadata = deluxe::harness::host::http_request(
                "POST".into(),
                url,
                serde_json::to_string(&headers)
                    .map_err(|_| "MCP HTTP headers are invalid".to_string())?,
                body,
            )
            .await?;
            let (response, response_headers) = read_http_response(metadata).await?;
            update_http_state(server, header(&response_headers, "mcp-session-id"), None);
            if expects_response {
                response_message(&response, id).map(Some)
            } else {
                Ok(None)
            }
        }
        TransportConfig::Stdio { .. } => {
            let handle = process_handle(server, &state.transport).await?;
            let mut framed = body.into_bytes();
            framed.push(b'\n');
            deluxe::harness::host::process_write(handle, framed).await?;
            if expects_response {
                response_message(&read_process_line(server, handle).await?, id).map(Some)
            } else {
                Ok(None)
            }
        }
    }
}

async fn initialize(server: &str) -> Result<(), String> {
    if configured_state(server)?.initialized {
        return Ok(());
    }
    if server_stopped(server) {
        return Err(format!("MCP server `{server}` is stopped"));
    }
    let result = request(
        server,
        "initialize",
        json!({
            "protocolVersion": SERVER_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "deluxe-agent-mcp", "version": "0.1"}
        }),
        true,
    )
    .await?
    .ok_or_else(|| "MCP initialize returned no result".to_string())?;
    if let Some(version) = result.get("protocolVersion").and_then(Value::as_str) {
        update_http_state(server, None, Some(version.to_string()));
    }
    request(server, "notifications/initialized", json!({}), false).await?;
    if let Some(config) = MCP_CONFIG.get() {
        if let Some(state) = config
            .lock()
            .map_err(|_| "MCP provider configuration is poisoned".to_string())?
            .get_mut(server)
        {
            state.initialized = true;
        }
    }
    Ok(())
}

async fn list_remote_tools(server: &str) -> Result<Vec<Value>, String> {
    initialize(server).await?;
    let result = request(server, "tools/list", json!({}), true)
        .await?
        .ok_or_else(|| "MCP tools/list returned no result".to_string())?;
    Ok(result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut result: String = text.chars().take(max).collect();
    result.push('…');
    result
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn exposed_tool_name(server: &str, remote_name: &str) -> String {
    format!("mcp__{server}__{remote_name}")
}

fn tool_descriptor(server: &str, tool: &Value) -> Option<Value> {
    let server = server.trim();
    let name = tool.get("name").and_then(Value::as_str)?;
    if !valid_identifier(server) || !valid_identifier(name) {
        return None;
    }
    let schema = tool
        .get("inputSchema")
        .filter(|schema| schema.is_object())
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    required.retain(|field| properties.contains_key(field));
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .filter(|description| !description.trim().is_empty())
        .unwrap_or("This MCP server gave no description.")
        .trim()
        .to_string();
    let summary = cap_chars(
        description.lines().next().unwrap_or(""),
        MAX_DESCRIPTION_CHARS,
    );
    let schema_type = schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("object");
    let mut extra = schema.as_object().cloned().unwrap_or_default();
    extra.remove("type");
    extra.remove("properties");
    extra.remove("required");
    let description = if extra.is_empty() {
        description
    } else {
        format!(
            "{description}\n\nThe server's schema also declares:\n{}",
            serde_json::to_string_pretty(&Value::Object(extra))
                .unwrap_or_else(|_| String::from("{}"))
        )
    };
    Some(json!({
        "name": exposed_tool_name(server, name),
        "summary": summary,
        "description": description,
        "guidelines": [],
        "inputSchema": {
            "type": schema_type,
            "properties": properties,
            "required": required
        },
        "hostValidatesArguments": false,
        "mutating": true
    }))
}

fn tool_output(result: &Value) -> String {
    let mut content = Vec::new();
    for block in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    content.push(json!({"type":"text","text":text}));
                }
            }
            Some("image") => {
                let media = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type");
                content.push(json!({
                    "type":"text",
                    "text":format!(
                        "[the server returned an image ({media}); this host cannot display it]"
                    )
                }));
            }
            Some("resource") => {
                let resource = block.get("resource");
                let text = resource
                    .and_then(|resource| resource.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!(
                            "[a resource the server returned: {}]",
                            resource
                                .and_then(|resource| resource.get("uri"))
                                .and_then(Value::as_str)
                                .unwrap_or("no uri")
                        )
                    });
                content.push(json!({"type":"text","text":text}));
            }
            Some(kind) => content.push(json!({
                "type":"text",
                "text":format!("[a `{kind}` content block, which this host does not display]")
            })),
            None => {}
        }
    }
    if let Some(structured) = result
        .get("structuredContent")
        .filter(|value| !value.is_null())
    {
        content.push(json!({
            "type":"text",
            "text":serde_json::to_string_pretty(structured)
                .unwrap_or_else(|_| structured.to_string())
        }));
    }
    if content.is_empty() {
        content.push(json!({"type":"text","text":"The tool returned no content."}));
    }
    json!({
        "content": content,
        "isError": result.get("isError").and_then(Value::as_bool).unwrap_or(false)
    })
    .to_string()
}

fn transport_label(transport: &TransportConfig) -> &'static str {
    match transport {
        TransportConfig::Http { .. } => "HTTP",
        TransportConfig::Stdio { .. } => "stdio",
    }
}

async fn ui_document(request: &str) -> String {
    let request: Value = serde_json::from_str(request).unwrap_or_else(|_| json!({}));
    ui_document_with_revision(&request, 1)
}

fn ui_document_with_revision(request: &Value, revision: u64) -> String {
    let plugin_id = request
        .get("pluginId")
        .and_then(Value::as_str)
        .unwrap_or("mcp@deluxe-defaults");
    let surface_id = request
        .get("surfaceId")
        .and_then(Value::as_str)
        .unwrap_or("mcp");
    let mut children = vec![json!({
        "type": "text",
        "text": "Configured MCP servers",
        "emphasis": "strong"
    })];
    children.push(json!({
        "type": "text",
        "text": "停止会断开连接并写入 .mcp.json 的 disabled，重开面板或重启应用后仍是停止；\
                 卸载会从 .mcp.json 删掉该 server。变更后请刷新插件，让 agent 重新读取工具列表。",
        "emphasis": "muted"
    }));
    match snapshot() {
        Ok(config) if config.is_empty() => children.push(json!({
            "type": "text",
            "text": "No MCP servers configured.",
            "emphasis": "normal"
        })),
        Ok(config) => {
            for (name, state) in config {
                // The verb is the declared action and the server name rides in the
                // control id, because a manifest declares its actions as a fixed
                // list and a server name cannot be known when it is written.
                let status = if state.stopped {
                    format!(
                        "[已停止] {} · 该 server 的工具不再可用",
                        transport_label(&state.transport)
                    )
                } else {
                    format!("[运行中] {}", transport_label(&state.transport))
                };
                children.push(json!({
                    "type": "section",
                    "id": format!("server.{name}"),
                    "title": name,
                    "children": [
                        {"type": "text", "text": status, "emphasis": "normal"},
                        {
                            "type": "row",
                            "children": [
                                {
                                    "type": "button",
                                    "id": format!("toggle.{name}"),
                                    "label": if state.stopped { "启动" } else { "停止" },
                                    "action": "toggle_server",
                                    "enabled": true
                                },
                                {
                                    "type": "button",
                                    "id": format!("remove.{name}"),
                                    "label": "卸载",
                                    "action": "remove_server",
                                    "enabled": true
                                }
                            ]
                        }
                    ]
                }));
            }
        }
        // The surface still opens when the provider is unconfigured: an empty
        // panel reads better than a blank one.
        Err(message) => children.push(json!({
            "type": "text",
            "text": message,
            "emphasis": "normal"
        })),
    }
    json!({
        "schemaVersion": 1,
        "pluginId": plugin_id,
        "surfaceId": surface_id,
        "revision": revision,
        "title": "MCP",
        "root": {"type": "column", "children": children}
    })
    .to_string()
}

/// Removes one server's declaration from `.mcp.json`.
///
/// The whole file is rewritten through the host's bounded write capability:
/// rustc's Wasm target has no ambient filesystem, and going through the host is
/// what keeps the write inside the bound scope root. Every other top-level key
/// and every other server is preserved, so a hand-written field this Component
/// does not understand survives the edit.
async fn remove_server(name: &str) -> Result<(), String> {
    let bytes = match deluxe::harness::host::read_plugin_file(".mcp.json".into()).await {
        Ok(bytes) => bytes,
        Err(error) if error.starts_with("plugin_file_not_found:") => {
            return Err(format!("MCP server `{name}` is not configured"))
        }
        Err(error) => return Err(error),
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| "MCP provider `.mcp.json` is not UTF-8".to_string())?;
    let mut root: Value = serde_json::from_str(&text)
        .map_err(|_| "MCP provider `.mcp.json` is invalid".to_string())?;
    let removed = root
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .map(|servers| servers.remove(name).is_some())
        .unwrap_or(false);
    if !removed {
        return Err(format!("MCP server `{name}` is not configured"));
    }
    let encoded = serde_json::to_string_pretty(&root)
        .map_err(|_| "MCP provider `.mcp.json` is not serializable".to_string())?;
    deluxe::harness::host::write_plugin_file(".mcp.json".into(), encoded.into_bytes()).await
}

/// Persists one server's stop switch in `.mcp.json`.
///
/// Stopping is a user decision, not a transient runtime detail: it has to
/// outlive this Wasm instance, and the file is the only state that survives a
/// reload. The whole file is rewritten through the host's bounded write
/// capability, preserving every other key and server.
async fn set_disabled(name: &str, disabled: bool) -> Result<(), String> {
    let bytes = match deluxe::harness::host::read_plugin_file(".mcp.json".into()).await {
        Ok(bytes) => bytes,
        Err(error) if error.starts_with("plugin_file_not_found:") => {
            return Err(format!("MCP server `{name}` is not configured"))
        }
        Err(error) => return Err(error),
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| "MCP provider `.mcp.json` is not UTF-8".to_string())?;
    let mut root: Value = serde_json::from_str(&text)
        .map_err(|_| "MCP provider `.mcp.json` is invalid".to_string())?;
    let Some(server) = root
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.get_mut(name))
        .and_then(Value::as_object_mut)
    else {
        return Err(format!("MCP server `{name}` is not configured"));
    };
    if disabled {
        server.insert("disabled".into(), Value::Bool(true));
    } else {
        server.remove("disabled");
    }
    let encoded = serde_json::to_string_pretty(&root)
        .map_err(|_| "MCP provider `.mcp.json` is not serializable".to_string())?;
    deluxe::harness::host::write_plugin_file(".mcp.json".into(), encoded.into_bytes()).await
}

impl Guest for McpProvider {
    async fn configure() -> Result<(), String> {
        // The stopped switch lives in `.mcp.json`, so a reload or a fresh
        // instance restores the same set of stopped servers instead of
        // silently starting everything the file still declares.
        let configured = configured_servers().await?;
        close_processes().await;
        MCP_CONFIG
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .map_err(|_| "MCP provider configuration is poisoned".to_string())?
            .clone_from(&configured);
        Ok(())
    }

    async fn describe() -> Result<String, String> {
        // An MCP provider contributes tools, not model metadata; the host
        // ignores every key this could carry.
        Ok("{}".into())
    }

    async fn list_tools() -> String {
        let mut descriptors = Vec::new();
        for server in active_names().unwrap_or_default() {
            if let Ok(tools) = list_remote_tools(&server).await {
                descriptors.extend(
                    tools
                        .iter()
                        .filter_map(|tool| tool_descriptor(&server, tool)),
                );
            }
        }
        serde_json::to_string(&descriptors).unwrap_or_else(|_| "[]".into())
    }

    async fn execute_tool(name: String, arguments_json: String) -> Result<String, String> {
        let arguments: Value = serde_json::from_str(&arguments_json)
            .map_err(|_| "MCP tool arguments are invalid JSON".to_string())?;
        let (server, remote_name) = active_names()
            .unwrap_or_default()
            .into_iter()
            .find_map(|server| {
                let prefix = format!("mcp__{server}__");
                name.strip_prefix(&prefix)
                    .filter(|remote_name| !remote_name.is_empty())
                    .map(|remote_name| (server, remote_name.to_string()))
            })
            .ok_or_else(|| format!("Unknown MCP tool `{name}`"))?;
        let _ = configured_state(&server)?;
        initialize(&server).await?;
        let result = request(
            &server,
            "tools/call",
            json!({"name":remote_name,"arguments":arguments}),
            true,
        )
        .await?
        .ok_or_else(|| "MCP tools/call returned no result".to_string())?;
        Ok(tool_output(&result))
    }

    async fn list_event_handlers() -> String {
        "[]".into()
    }

    async fn handle_event(_handler_id: String, _event_json: String) -> Result<String, String> {
        Err("MCP provider has no event handlers".into())
    }

    async fn open_surface(request_json: String) -> Result<String, String> {
        Ok(ui_document(&request_json).await)
    }

    async fn handle_action(action_json: String) -> Result<String, String> {
        let action: Value = serde_json::from_str(&action_json)
            .map_err(|_| "MCP UI action is invalid JSON".to_string())?;
        // The response repeats the request's own identity so the host's
        // revision and identity checks see the surface the user acted on.
        let request = action.get("surface").cloned().unwrap_or_else(|| json!({}));
        let revision = action
            .get("revision")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let verb = action
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| "MCP UI action has no name".to_string())?;
        // The control the host validated is `toggle.<server>` or
        // `remove.<server>`; the action only says which verb it was.
        let control_id = action
            .get("controlId")
            .and_then(Value::as_str)
            .ok_or_else(|| "MCP UI action has no control".to_string())?;
        let server = control_id
            .split_once('.')
            .map(|(_, server)| server)
            .filter(|server| !server.is_empty())
            .ok_or_else(|| "MCP UI action does not name a server".to_string())?;
        if !configured_names().is_ok_and(|names| names.iter().any(|name| name == server)) {
            return Err(format!("MCP server `{server}` is not configured"));
        }
        match verb {
            // Stopping tears down the connection but leaves the declaration, so
            // the row stays and can be started again.
            "toggle_server" => {
                // Persist before touching the live state: if the write fails,
                // the in-memory switch must not claim a state a reload undoes.
                let stopped = server_stopped(server);
                set_disabled(server, !stopped).await?;
                set_stopped(server, !stopped)?;
                if !stopped {
                    disconnect(server).await;
                }
            }
            // Removing takes the declaration out of `.mcp.json` and then drops
            // the connection and the in-memory entry, so the snapshot the host
            // shows next already reflects the removal even before a reload.
            "remove_server" => {
                remove_server(server).await?;
                disconnect(server).await;
                forget(server);
            }
            other => return Err(format!("Unknown MCP UI action `{other}`")),
        }
        Ok(ui_document_with_revision(&request, revision))
    }

    async fn close_surface(_surface_id: String) {}
}

export!(McpProvider);
