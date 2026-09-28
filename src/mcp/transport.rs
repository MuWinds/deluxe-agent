//! How one JSON-RPC message reaches an MCP server, and how the answer comes
//! back.
//!
//! Two wires, one shape. A plugin's server is usually a child process spoken to
//! over its stdin and stdout, but the format also allows a remote endpoint over
//! HTTP — and the only thing both do is carry one message out and one back. So
//! the trait is a round trip rather than a byte stream, and each transport
//! writes only the part that is genuinely different.
//!
//! Framing lives here; JSON-RPC does not. The id sequence and the error envelope
//! belong to [`super::McpClient`], and neither transport looks inside a message
//! beyond the id it needs to match a reply to a request.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::error::{AgentError, Result};

use super::SERVER_PROTOCOL_VERSION;

/// One message out, one message back.
///
/// `round_trip` may assume no other request is in flight. The client serialises
/// calls behind a mutex, which is what makes a stdio server's single pipe
/// enough: answers come back in the order they were asked for, so the first
/// reply bearing this request's id is the one.
#[async_trait]
pub trait Transport: Send {
    /// Sends a request and returns the response to it.
    async fn round_trip(&mut self, message: &Value) -> Result<Value>;

    /// Sends a message that expects no response — a JSON-RPC notification.
    async fn send(&mut self, message: &Value) -> Result<()>;

    /// Tells the transport which protocol revision the server settled on.
    ///
    /// Only HTTP has anywhere to put it, since stdio carries no headers, so the
    /// default does nothing and [`HttpTransport`] is the one that overrides.
    fn negotiated(&mut self, _version: &str) {}
}

/// The id a JSON-RPC request carries.
fn request_id(message: &Value) -> Option<u64> {
    message.get("id").and_then(Value::as_u64)
}

/// Whether `message` is the reply to the request numbered `wanted`.
///
/// A server may push notifications, which carry no id, and in principle a
/// request of its own. Neither answers anything this host asked, so the id is
/// what separates them from the reply being waited for.
fn is_answer(message: &Value, wanted: Option<u64>) -> bool {
    wanted.is_some() && request_id(message) == wanted
}

/// Frames one message as a line of newline-delimited JSON.
async fn write_line<W>(writer: &mut W, message: &Value) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut line = serde_json::to_string(message).map_err(|error| {
        AgentError::internal(format!("Could not encode an MCP message: {error}"))
    })?;
    line.push('\n');

    let failed =
        |error: std::io::Error| AgentError::from_io("Failed to write to an MCP server", error);
    writer.write_all(line.as_bytes()).await.map_err(failed)?;
    writer.flush().await.map_err(failed)
}

/// Reads lines until the answer to `wanted` arrives, skipping anything else.
async fn read_answer<R>(reader: &mut R, wanted: Option<u64>) -> Result<Value>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|error| AgentError::from_io("Failed to read from an MCP server", error))?;
        if read == 0 {
            return Err(AgentError::internal(
                "The MCP server closed its output before answering",
            ));
        }

        // A server that writes something other than JSON to its protocol stream
        // is broken, but the next line may still be the answer, so a bad line is
        // stepped over rather than treated as the end.
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if is_answer(&message, wanted) {
            return Ok(message);
        }
    }
}

/// Newline-delimited JSON over any reader and writer.
///
/// Generic rather than fixed to a child process's pipes so the framing can be
/// driven by a `tokio::io::duplex` pair in a test — how an unprompted
/// notification or a line that is not JSON is handled is worth checking without
/// starting a process.
pub struct LineTransport<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> LineTransport<R, W>
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    /// A transport speaking JSON-RPC over a line-delimited reader/writer pair —
    /// the stdio server case.
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }
}

#[async_trait]
impl<R, W> Transport for LineTransport<R, W>
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    async fn round_trip(&mut self, message: &Value) -> Result<Value> {
        let wanted = request_id(message);
        write_line(&mut self.writer, message).await?;
        read_answer(&mut self.reader, wanted).await
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        write_line(&mut self.writer, message).await
    }
}

/// A plugin's MCP server, run as a child process and spoken to over its pipes.
pub struct StdioTransport {
    /// `Option` so `Drop` can drop the framing — and with it the stdin pipe —
    /// before the process is killed.
    lines: Option<LineTransport<BufReader<ChildStdout>, ChildStdin>>,
    child: Child,
}

impl StdioTransport {
    /// Starts `command` as a child process.
    ///
    /// `cwd` is resolved by the caller: a plugin's relative `cwd` means relative
    /// to the plugin root, not to wherever this agent happened to be launched.
    pub fn spawn(
        server: &str,
        command: &str,
        args: &[String],
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .current_dir(cwd)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited rather than captured: a server's stderr is its
            // diagnostics, and swallowing it would leave a failure to start with
            // nothing to read.
            .stderr(Stdio::inherit());
        // A GUI build has no console, so a server that is a console program
        // would otherwise open one of its own over the app.
        crate::process::hide_console(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|error| {
                AgentError::from_io(
                    &format!("Failed to start the `{server}` MCP server (`{command}`)"),
                    error,
                )
            })?;

        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            // Unreachable — both pipes were asked for just above — but a child
            // dropped here would go on running, so it is killed on the way out.
            let _ = child.start_kill();
            return Err(AgentError::internal(format!(
                "The `{server}` MCP server was started without pipes"
            )));
        };

        Ok(Self {
            lines: Some(LineTransport::new(BufReader::new(stdout), stdin)),
            child,
        })
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        // Dropping the framing closes the stdin pipe first, which lets a server
        // that watches for EOF shut itself down; the kill is the backstop. It is
        // needed because `tokio::process::Child` — unlike `std::process::Child`
        // — leaves the process running when it is dropped. Tokio reaps it either
        // way.
        self.lines.take();
        let _ = self.child.start_kill();
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn round_trip(&mut self, message: &Value) -> Result<Value> {
        let Some(lines) = self.lines.as_mut() else {
            return Err(AgentError::internal(
                "The MCP server's pipes are already closed",
            ));
        };
        lines.round_trip(message).await
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        let Some(lines) = self.lines.as_mut() else {
            return Err(AgentError::internal(
                "The MCP server's pipes are already closed",
            ));
        };
        lines.send(message).await
    }
}

/// A remote MCP server, spoken to over HTTP.
///
/// This is MCP's streamable-HTTP shape: every message is a POST, and the answer
/// comes back either as a JSON body or as one frame of an event stream. A server
/// may also hand back a session id at the handshake, which is echoed on every
/// later request; a server that keeps no session simply does not send one.
pub struct HttpTransport {
    http: reqwest::Client,
    url: String,
    session: Option<String>,
    /// What the `mcp-protocol-version` header declares.
    ///
    /// Starts as the revision this client asks for, and becomes whatever the
    /// server answered during `initialize`. The header names the revision in
    /// use, so once the two differ it is the server's answer that belongs there
    /// — a strict server rejects a header naming a revision it never agreed to.
    version: String,
}

impl HttpTransport {
    /// A transport speaking JSON-RPC over HTTP POST to `url`.
    ///
    /// Returns an error if the HTTP client cannot be built. No overall request
    /// timeout — a tool call may run for minutes and the host bounds it — only a
    /// connect timeout, so a dead host fails fast.
    pub fn new(url: &str) -> Result<Self> {
        // No overall timeout: a tool call can legitimately run for minutes, and
        // the host bounds each call itself. This only stops a dead host from
        // hanging on the connect.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| {
                AgentError::internal(format!("Failed to build the MCP HTTP client: {error}"))
            })?;
        Ok(Self {
            http,
            url: url.to_string(),
            session: None,
            version: SERVER_PROTOCOL_VERSION.to_string(),
        })
    }

    /// POSTs one message and returns the response's content type and body.
    async fn post(&mut self, message: &Value) -> Result<(String, String)> {
        let mut request = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            // Both are offered and the server picks. An event stream is the
            // long-lived case, a JSON body the short one.
            .header("accept", "application/json, text/event-stream")
            // The spec asks for this on every request after the handshake.
            // Sending it on the handshake too is harmless, and cheaper than
            // carrying a "has the handshake happened yet" flag.
            .header("mcp-protocol-version", &self.version)
            .json(message);
        if let Some(session) = &self.session {
            request = request.header("mcp-session-id", session);
        }

        let response = request.send().await.map_err(|error| {
            AgentError::internal(format!(
                "Failed to reach the MCP server at {}: {error}",
                self.url
            ))
        })?;

        // Read the headers before the body: `text` consumes the response.
        let status = response.status();
        if let Some(session) = header(&response, "mcp-session-id") {
            self.session = Some(session);
        }
        let content_type = header(&response, "content-type").unwrap_or_default();
        let body = response.text().await.map_err(|error| {
            AgentError::internal(format!("Failed to read the MCP server's answer: {error}"))
        })?;

        if !status.is_success() {
            return Err(AgentError::internal(format!(
                "The MCP server at {} answered {status}: {body}",
                self.url
            )));
        }
        Ok((content_type, body))
    }
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[async_trait]
impl Transport for HttpTransport {
    async fn round_trip(&mut self, message: &Value) -> Result<Value> {
        let wanted = request_id(message);
        let (content_type, body) = self.post(message).await?;

        if content_type.contains("text/event-stream") {
            return event_stream_response(&body, wanted);
        }
        serde_json::from_str(&body).map_err(|error| {
            AgentError::internal(format!(
                "The MCP server at {} did not answer with a JSON-RPC message: {error}",
                self.url
            ))
        })
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        // A notification is answered with `202 Accepted` and no body, so the
        // status check inside `post` is the whole of what there is to verify.
        self.post(message).await.map(|_| ())
    }

    fn negotiated(&mut self, version: &str) {
        self.version = version.to_string();
    }
}

/// The JSON-RPC message a `text/event-stream` body carries.
///
/// Frames are `data: <json>` lines ending at a blank line, and the spec says a
/// payload split across several `data:` lines is joined with newlines. A server
/// may send events that are not the answer to this request, so the frame whose
/// id matches is the one returned.
fn event_stream_response(body: &str, wanted: Option<u64>) -> Result<Value> {
    // Normalised first: a server is as likely to frame with CRLF, and an event
    // boundary is a blank line either way.
    let body = body.replace("\r\n", "\n");

    for frame in body.split("\n\n") {
        let mut payload = String::new();
        for line in frame.lines() {
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(rest.trim_start());
        }

        let Ok(message) = serde_json::from_str::<Value>(&payload) else {
            continue;
        };
        if is_answer(&message, wanted) {
            return Ok(message);
        }
    }

    Err(AgentError::internal(
        "The MCP server's event stream carried no answer to the request",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, DuplexStream, ReadHalf, WriteHalf};
    use tokio::net::TcpListener;

    /// The framing transport over one end of a duplex pipe.
    type Piped = LineTransport<BufReader<ReadHalf<DuplexStream>>, WriteHalf<DuplexStream>>;

    /// A request numbered 1 — the shape [`super::super::McpClient`] sends.
    fn request() -> Value {
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}})
    }

    fn answer() -> Value {
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}})
    }

    /// A line of newline-delimited JSON.
    fn line_of(value: &Value) -> Vec<u8> {
        let mut text = serde_json::to_string(value).unwrap();
        text.push('\n');
        text.into_bytes()
    }

    /// A transport over one end of a pipe, with the other end scripted to read
    /// one line and then write `reply`.
    fn piped(reply: Vec<u8>) -> (Piped, tokio::task::JoinHandle<Value>) {
        let (client, server) = duplex(4096);
        let (read, write) = tokio::io::split(client);
        let (server_read, mut server_write) = tokio::io::split(server);

        let task = tokio::spawn(async move {
            let mut reader = BufReader::new(server_read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let seen = serde_json::from_str(line.trim()).unwrap();

            server_write.write_all(&reply).await.unwrap();
            seen
        });

        (LineTransport::new(BufReader::new(read), write), task)
    }

    #[tokio::test]
    async fn a_message_goes_out_as_one_line_and_its_answer_comes_back() {
        let (mut transport, server) = piped(line_of(&answer()));

        let got = transport.round_trip(&request()).await.unwrap();

        assert_eq!(got, answer());
        assert_eq!(
            server.await.unwrap()["method"],
            "tools/list",
            "the request reached the server intact"
        );
    }

    #[tokio::test]
    async fn an_unprompted_notification_is_not_mistaken_for_the_answer() {
        let mut reply = line_of(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {"level": "info"},
        }));
        reply.extend(line_of(&answer()));
        let (mut transport, _server) = piped(reply);

        let got = transport.round_trip(&request()).await.unwrap();

        assert_eq!(got, answer(), "the notification carries no id and is not ours");
    }

    #[tokio::test]
    async fn a_line_that_is_not_json_is_stepped_over() {
        // A server that logs to stdout by mistake should not cost the answer.
        let mut reply = b"starting up...\n".to_vec();
        reply.extend(line_of(&answer()));
        let (mut transport, _server) = piped(reply);

        let got = transport.round_trip(&request()).await.unwrap();

        assert_eq!(got, answer());
    }

    #[tokio::test]
    async fn a_reply_to_a_different_request_is_not_taken_as_the_answer() {
        let mut reply = line_of(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 99,
            "result": {"stale": true},
        }));
        reply.extend(line_of(&answer()));
        let (mut transport, _server) = piped(reply);

        let got = transport.round_trip(&request()).await.unwrap();

        assert_eq!(got, answer(), "only id 1 answers the request that asked");
    }

    #[tokio::test]
    async fn a_closed_pipe_is_an_error_rather_than_a_hang() {
        let (client, server) = duplex(4096);
        drop(server);
        let (read, _write) = tokio::io::split(client);

        let error = read_answer(&mut BufReader::new(read), Some(1))
            .await
            .unwrap_err();
        assert!(error.message.contains("closed its output"), "{}", error.message);
    }

    #[test]
    fn an_event_stream_frame_carries_the_answer() {
        let body = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";

        let got = event_stream_response(body, Some(1)).unwrap();
        assert_eq!(got["result"]["ok"], true);
    }

    #[test]
    fn a_crlf_event_stream_parses_the_same_way() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n\r\n";
        assert!(event_stream_response(body, Some(1)).is_ok());
    }

    #[test]
    fn an_event_stream_payload_split_across_data_lines_is_joined() {
        // The spec says to join them with newlines, which is how a
        // pretty-printed payload arrives.
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\n\
                    data: \"result\":{\"n\":2}}\n\n";

        let got = event_stream_response(body, Some(1)).unwrap();
        assert_eq!(got["result"]["n"], 2);
    }

    #[test]
    fn an_event_stream_without_this_requests_answer_is_an_error() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n";
        assert!(event_stream_response(body, Some(1)).is_err());
    }

    /// Serves exactly one request with `response`, and returns its URL and a
    /// handle yielding the request head it saw.
    ///
    /// A raw listener rather than a mock, for the reason the agent-loop tests
    /// give: what is interesting here is the bytes on the wire — the headers
    /// reqwest sends and the framing the server answers with — and a mock would
    /// not have any.
    async fn serve_once(response: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());

        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // Read the head a byte at a time, so nothing of the body is
            // swallowed with it, then exactly the length the body declares.
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read_exact(&mut byte).await.is_err() {
                    return String::new();
                }
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).to_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            let _ = socket.read_exact(&mut body).await;

            socket.write_all(response.as_bytes()).await.unwrap();
            head
        });

        (url, task)
    }

    #[tokio::test]
    async fn an_http_round_trip_reads_an_event_stream_and_keeps_the_session() {
        let frame = "event: message\n\
                     data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let (url, _served) = serve_once(format!(
            "HTTP/1.1 200 OK\r\n\
             content-type: text/event-stream\r\n\
             mcp-session-id: session-7\r\n\
             content-length: {}\r\n\r\n{frame}",
            frame.len()
        ))
        .await;

        let mut transport = HttpTransport::new(&url).unwrap();
        let got = transport.round_trip(&request()).await.unwrap();

        assert_eq!(got["result"]["tools"], serde_json::json!([]));
        assert_eq!(
            transport.session.as_deref(),
            Some("session-7"),
            "the id the server hands back is kept for the requests that follow"
        );
    }

    #[tokio::test]
    async fn a_request_declares_the_version_it_was_built_with() {
        let body = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
        let (url, served) = serve_once(format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await;

        HttpTransport::new(&url)
            .unwrap()
            .round_trip(&request())
            .await
            .unwrap();

        let head = served.await.unwrap();
        assert!(
            head.contains(&format!("mcp-protocol-version: {SERVER_PROTOCOL_VERSION}")),
            "before any negotiation the request declares the revision asked for: {head}"
        );
    }

    #[tokio::test]
    async fn the_negotiated_version_replaces_the_requested_one() {
        // A server that answers with an older revision is the case this guards:
        // the header has to name what was agreed, not what was asked for, or a
        // strict server is told a revision it never accepted.
        let body = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
        let (url, served) = serve_once(format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await;

        let mut transport = HttpTransport::new(&url).unwrap();
        transport.negotiated("2024-11-05");
        transport.round_trip(&request()).await.unwrap();

        let head = served.await.unwrap();
        assert!(
            head.contains("mcp-protocol-version: 2024-11-05"),
            "the negotiated revision is what later requests declare: {head}"
        );
    }

    #[tokio::test]
    async fn an_http_json_body_is_read_as_the_answer() {
        let body = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"t\"}]}}";
        let (url, _served) = serve_once(format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await;

        let mut transport = HttpTransport::new(&url).unwrap();
        let got = transport.round_trip(&request()).await.unwrap();

        assert_eq!(got["result"]["tools"][0]["name"], "t");
    }

    #[tokio::test]
    async fn an_http_error_status_is_reported_rather_than_parsed() {
        let body = "no such session";
        let (url, _served) = serve_once(format!(
            "HTTP/1.1 404 Not Found\r\ncontent-type: text/plain\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await;

        let mut transport = HttpTransport::new(&url).unwrap();
        let error = transport.round_trip(&request()).await.unwrap_err();

        assert!(error.message.contains("404"), "{}", error.message);
    }
}
