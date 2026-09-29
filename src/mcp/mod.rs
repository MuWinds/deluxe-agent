//! MCP servers, as this agent consumes them.
//!
//! A plugin may bring MCP servers: `.mcp.json` names them, and each is either a
//! child process spoken to over its pipes or a remote endpoint spoken to over
//! HTTP. This module turns one of those into a set of host tools — connect,
//! handshake, list what the server offers, and then call them like anything
//! else in the catalogue.
//!
//! # Why the client is a mutex, not a pool
//!
//! A call is one request and one response over one connection, and the tools of
//! a server share the connection, so they take turns. That also matches how the
//! agent loop drives tools: it runs them one at a time, in the order the model
//! asked for them.
//!
//! # What is deliberately not done here
//!
//! * **No OAuth.** A server that declares `oauth_resource` needs a browser
//!   flow. It is reported as the reason it was skipped rather than attempted
//!   and failed later, because a silent failure reads as a broken plugin.
//! * **No server-initiated requests.** The handshake declares no capabilities,
//!   so a server must not ask this client for anything — sampling, roots, and
//!   elicitation are all out of scope.

pub mod tool;
pub mod transport;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::error::{AgentError, Result};
use crate::plugins::manifest::McpServerConfig;

/// The protocol revision this client asks for.
///
/// A server may answer with a different one — the spec allows it, and this
/// client carries on, warning rather than refusing, because the revisions are
/// usually compatible. The revision asked for is the current one, which is also
/// the family that streamable HTTP belongs to.
pub const SERVER_PROTOCOL_VERSION: &str = "2025-06-18";

/// How long a server has to answer, when its config does not say.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// One tool a server offers, as `tools/list` describes it.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    /// The server's JSON Schema, verbatim.
    ///
    /// Passed through rather than narrowed, because the server validates against
    /// it: it is the server's schema and not this host's to simplify. What of it
    /// fits the host's own [`crate::tools::ObjectSchema`] is carried over by
    /// [`tool::McpTool`], and the rest travels in the description.
    pub input_schema: Value,
}

/// One conversation with an MCP server.
pub struct McpClient {
    transport: Box<dyn transport::Transport>,
    /// The server's name, for error messages — a failure from a transport does
    /// not otherwise say which server it came from.
    server: String,
    next_id: u64,
}

impl McpClient {
    /// Starts a server and completes the handshake.
    ///
    /// `plugin_root` is what a relative `cwd` in the config resolves against —
    /// which is what makes a plugin's own `./scripts/server.py` work.
    pub async fn connect(
        server: &str,
        config: &McpServerConfig,
        plugin_root: &Path,
    ) -> Result<Self> {
        let transport: Box<dyn transport::Transport> = if config.is_http() {
            if config.oauth_resource.is_some() {
                // Not a fault in the server: this agent simply does not do the
                // browser dance. Saying so plainly makes the skip legible.
                return Err(AgentError::internal(format!(
                    "The `{server}` MCP server needs an OAuth flow, which this agent does not support"
                )));
            }
            let url = config.url.as_deref().ok_or_else(|| {
                AgentError::internal(format!("The `{server}` MCP server declares no `url`"))
            })?;
            Box::new(transport::HttpTransport::new(url)?)
        } else {
            let command = config.command.as_deref().ok_or_else(|| {
                AgentError::internal(format!("The `{server}` MCP server declares no `command`"))
            })?;
            // A relative `cwd` is relative to the plugin, not to this agent's
            // own working directory.
            let cwd = match &config.cwd {
                Some(relative) => plugin_root.join(relative.trim_start_matches("./")),
                None => plugin_root.to_path_buf(),
            };
            Box::new(transport::StdioTransport::spawn(
                server,
                command,
                &config.args,
                &cwd,
                &config.env,
            )?)
        };

        let mut client = Self {
            transport,
            server: server.to_string(),
            next_id: 1,
        };

        // A server that will not handshake is one the user should not wait on
        // indefinitely: the first run in a project pays this cost, and it is
        // paid before the run can start.
        let timeout = config
            .startup_timeout_sec
            .map_or(STARTUP_TIMEOUT, Duration::from_secs);
        tokio::time::timeout(timeout, client.handshake())
            .await
            .map_err(|_| {
                AgentError::timeout(format!(
                    "The `{server}` MCP server did not answer `initialize` within {timeout:?}"
                ))
            })??;

        Ok(client)
    }

    /// `initialize`, then the notification that completes the handshake.
    async fn handshake(&mut self) -> Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": SERVER_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "deluxe-agent",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;

        let negotiated = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if negotiated != SERVER_PROTOCOL_VERSION {
            tracing::warn!(
                server = %self.server,
                asked = SERVER_PROTOCOL_VERSION,
                negotiated,
                "the MCP server negotiated a different protocol version"
            );
        }
        // The header names the revision in use, which after negotiation is the
        // server's answer rather than the request. A server that named nothing
        // leaves the requested revision in place, that being the only guess
        // available.
        if negotiated != "unknown" {
            self.transport.negotiated(negotiated);
        }

        // The second half of the handshake. A server is entitled to wait for it
        // before answering anything else.
        self.notify("notifications/initialized", json!({})).await
    }

    /// The tools the server offers.
    ///
    /// Bounded by the same timeout as the handshake: a server that cannot answer
    /// this promptly is as unusable as one that cannot start.
    pub async fn list_tools(&mut self) -> Result<Vec<ToolSpec>> {
        let server = self.server.clone();
        let result = tokio::time::timeout(STARTUP_TIMEOUT, self.request("tools/list", json!({})))
            .await
            .map_err(|_| {
                AgentError::timeout(format!(
                    "The `{server}` MCP server did not answer `tools/list` within \
                     {STARTUP_TIMEOUT:?}"
                ))
            })??;

        let mut specs = Vec::new();
        for tool in result
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // A tool with no name cannot be called, so it is passed over rather
            // than registered under a guess.
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            specs.push(ToolSpec {
                name: name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                input_schema: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            });
        }
        Ok(specs)
    }

    /// Calls one of the server's tools and returns its `result` payload.
    pub async fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    /// One request, with the JSON-RPC error envelope turned into a host error.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});

        let response = match self.transport.round_trip(&message).await {
            Ok(response) => response,
            Err(error) => {
                return Err(AgentError::internal(format!(
                    "The `{}` MCP server failed on `{method}`: {error}",
                    self.server
                )));
            }
        };

        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let detail = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            return Err(AgentError::internal(format!(
                "The `{}` MCP server rejected `{method}` ({code}): {detail}",
                self.server
            )));
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// One notification: sent, and not waited on.
    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.transport.send(&message).await.map_err(|error| {
            AgentError::internal(format!(
                "The `{}` MCP server failed on `{method}`: {error}",
                self.server
            ))
        })
    }

    /// A client that speaks over `stream`, for tests in sibling modules.
    ///
    /// Gated on `cfg(test)` so it never becomes part of the shipped surface. It
    /// exists so a test can build a tool — which needs a client — without
    /// starting a server it never calls.
    #[cfg(test)]
    pub(crate) fn for_test(stream: tokio::io::DuplexStream) -> Self {
        let (read, write) = tokio::io::split(stream);
        Self {
            transport: Box::new(transport::LineTransport::new(
                tokio::io::BufReader::new(read),
                write,
            )),
            server: "test".to_string(),
            next_id: 1,
        }
    }
}

/// A server's client, shared by every tool it exposes.
pub type SharedClient = Arc<Mutex<McpClient>>;

/// Wraps a client so the tools it produced can share it.
///
/// The `Mutex` is the client's business, not the caller's: the worker that
/// starts a server should not have to name a lock type to hand its tools out.
pub fn shared(client: McpClient) -> SharedClient {
    Arc::new(Mutex::new(client))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A client over a duplex pipe, with the other end scripted to answer the
    /// first `replies.len()` requests.
    ///
    /// The transport is a [`transport::LineTransport`] rather than a
    /// `StdioTransport`, so the JSON-RPC sequence is exercised without starting
    /// a process: what is under test here is the sequence, not `Command::spawn`.
    fn scripted(replies: Vec<Value>) -> McpClient {
        let (client, server) = duplex(8192);
        let (server_read, mut server_write) = tokio::io::split(server);

        tokio::spawn(async move {
            let mut reader = BufReader::new(server_read);
            let mut replies = replies.into_iter();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                // Only a request consumes a scripted answer; the handshake's
                // closing notification expects none.
                let Ok(request) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if request.get("id").is_none() {
                    continue;
                }

                let Some(reply) = replies.next() else {
                    // The script is spent. Closing the pipe is the honest signal
                    // that there is nothing more to say.
                    return;
                };
                let mut text = serde_json::to_string(&reply).unwrap();
                text.push('\n');
                if server_write.write_all(text.as_bytes()).await.is_err() {
                    return;
                }
            }
        });

        McpClient::for_test(client)
    }

    /// The reply to request 1, which is always `initialize`.
    fn initialized() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": SERVER_PROTOCOL_VERSION}})
    }

    #[tokio::test]
    async fn the_handshake_initializes_and_then_says_so() {
        let mut client = scripted(vec![initialized()]);
        client.handshake().await.unwrap();

        assert_eq!(
            client.next_id, 2,
            "`initialize` was request 1, so the next request is 2"
        );
    }

    #[tokio::test]
    async fn tools_are_read_from_the_servers_list() {
        let mut client = scripted(vec![
            initialized(),
            json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": [
                {"name": "search", "description": "Find things",
                 "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}},
                {"description": "no name, so it cannot be called"},
            ]}}),
        ]);
        client.handshake().await.unwrap();
        let tools = client.list_tools().await.unwrap();

        assert_eq!(
            tools.len(),
            1,
            "a tool the server did not name cannot be called, so it is dropped"
        );
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description.as_deref(), Some("Find things"));
        assert_eq!(tools[0].input_schema["properties"]["q"]["type"], "string");
    }

    #[tokio::test]
    async fn a_tool_with_no_schema_gets_a_permissive_object() {
        let mut client = scripted(vec![
            initialized(),
            json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": [{"name": "bare"}]}}),
        ]);
        client.handshake().await.unwrap();
        let tools = client.list_tools().await.unwrap();

        assert_eq!(tools[0].input_schema, json!({"type": "object"}));
    }

    #[tokio::test]
    async fn a_json_rpc_error_becomes_a_host_error_naming_the_server() {
        let mut client = scripted(vec![
            initialized(),
            json!({"jsonrpc": "2.0", "id": 2,
                   "error": {"code": -32601, "message": "Method not found"}}),
        ]);
        client.handshake().await.unwrap();

        let error = client.list_tools().await.unwrap_err();
        assert!(error.message.contains("-32601"), "{}", error.message);
        assert!(
            error.message.contains("Method not found"),
            "{}",
            error.message
        );
        assert!(error.message.contains("test"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_call_passes_the_arguments_through() {
        let mut client = scripted(vec![
            initialized(),
            json!({"jsonrpc": "2.0", "id": 2, "result": {"content": [{"type": "text", "text": "hi"}]}}),
        ]);
        client.handshake().await.unwrap();

        let result = client
            .call_tool("search", &json!({"q": "x"}))
            .await
            .unwrap();
        assert_eq!(result["content"][0]["text"], "hi");
    }

    /// The error `connect` returns.
    ///
    /// Spelled out rather than `unwrap_err`, which would require `McpClient` to
    /// implement `Debug` purely for the test.
    async fn connect_error(server: &str, config: &McpServerConfig) -> AgentError {
        match McpClient::connect(server, config, Path::new(".")).await {
            Ok(_) => panic!("connecting `{server}` should have failed"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn a_server_that_needs_oauth_is_refused_with_that_reason() {
        let config: McpServerConfig = serde_json::from_str(
            r#"{"type":"http","url":"https://mcp.example/mcp",
                "oauth_resource":"https://mcp.example/mcp"}"#,
        )
        .unwrap();

        let error = connect_error("figma", &config).await;
        assert!(error.message.contains("OAuth"), "{}", error.message);
    }

    #[tokio::test]
    async fn a_stdio_server_without_a_command_is_refused() {
        let config: McpServerConfig = serde_json::from_str(r#"{"type":"stdio"}"#).unwrap();

        let error = connect_error("broken", &config).await;
        assert!(error.message.contains("command"), "{}", error.message);
    }
}
