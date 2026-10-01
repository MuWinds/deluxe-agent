//! Explicit host imports; components never receive ambient filesystem access.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::{code, AgentError, Result};
use crate::harness::ports::ToolRuntime;
use crate::llm::{FunctionCall, ToolCall};

use super::ui_protocol::MAX_PAYLOAD_BYTES;
use super::wasm_manifest::Permissions;

struct ProcessHandle {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

struct HttpHandle {
    response: reqwest::Response,
    pending: Vec<u8>,
}

#[derive(Clone)]
struct RawHost {
    next_process: Arc<AtomicU64>,
    processes: Arc<Mutex<BTreeMap<u64, Arc<Mutex<ProcessHandle>>>>>,
    next_http: Arc<AtomicU64>,
    responses: Arc<Mutex<BTreeMap<u64, Arc<Mutex<HttpHandle>>>>>,
    http: reqwest::Client,
}

const MAX_TRANSPORT_HANDLES: usize = 64;

impl RawHost {
    fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| AgentError::internal(format!("Build plugin HTTP client: {error}")))?;
        Ok(Self {
            next_process: Arc::new(AtomicU64::new(1)),
            processes: Arc::new(Mutex::new(BTreeMap::new())),
            next_http: Arc::new(AtomicU64::new(1)),
            responses: Arc::new(Mutex::new(BTreeMap::new())),
            http,
        })
    }
}

#[derive(Clone)]
pub struct CapabilityHub {
    pub project: PathBuf,
    pub plugin_root: PathBuf,
    pub permissions: Permissions,
    pub mcp_servers: BTreeMap<String, super::manifest::McpServerConfig>,
    pub tools: Arc<dyn ToolRuntime>,
    raw: RawHost,
}

impl CapabilityHub {
    /// Creates the capabilities available to one project-scoped component.
    pub fn new(
        project: PathBuf,
        plugin_root: PathBuf,
        permissions: Permissions,
        mcp_servers: BTreeMap<String, super::manifest::McpServerConfig>,
        tools: Arc<dyn ToolRuntime>,
    ) -> Result<Self> {
        Ok(Self {
            project,
            plugin_root,
            permissions,
            mcp_servers,
            tools,
            raw: RawHost::new()?,
        })
    }

    /// Stops every process and releases every HTTP response owned by this component.
    pub async fn shutdown(&self) {
        let processes = std::mem::take(&mut *self.raw.processes.lock().await);
        for (_, process) in processes {
            let mut process = process.lock().await;
            let _ = process.child.start_kill();
            let _ = process.child.wait().await;
        }
        self.raw.responses.lock().await.clear();
    }

    /// Returns the host tool descriptors visible to a provider.
    pub fn list_tools(&self) -> Result<String> {
        let descriptors: Vec<_> = self
            .tools
            .descriptors()
            .into_iter()
            .filter(|descriptor| {
                self.permissions
                    .invoke_tools
                    .iter()
                    .any(|name| name == &descriptor.name)
            })
            .collect();
        serde_json::to_string(&descriptors)
            .map_err(|error| AgentError::internal(format!("Encode host tool descriptors: {error}")))
    }

    /// Invokes an explicitly allowed host tool through normal dispatch.
    ///
    /// `Err` reports denied capabilities, escaped paths, malformed JSON,
    /// cancellation, or tool failures.
    pub async fn invoke(
        &self,
        name: &str,
        arguments: &str,
        cancel: &CancellationToken,
    ) -> Result<String> {
        if !self
            .permissions
            .invoke_tools
            .iter()
            .any(|allowed| allowed == name)
        {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "This host capability was not granted",
            ));
        }
        if arguments.len() > MAX_PAYLOAD_BYTES {
            return Err(AgentError::invalid_params(
                "Host tool arguments exceed the payload limit",
            ));
        }
        let mut arguments: Value = serde_json::from_str(arguments)
            .map_err(|_| AgentError::invalid_params("Host tool arguments must be JSON"))?;
        if matches!(name, "read_file" | "list_dir") {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| AgentError::invalid_params("A project-relative path is required"))?;
            let root = tokio::fs::canonicalize(&self.project)
                .await
                .map_err(|error| AgentError::from_io("Resolve capability project", error))?;
            let path = tokio::fs::canonicalize(root.join(path))
                .await
                .map_err(|error| AgentError::from_io("Resolve capability path", error))?;
            if !path.starts_with(&root) {
                return Err(AgentError::new(
                    code::PLUGIN_PERMISSION_DENIED,
                    "Host file access must stay inside the project",
                ));
            }
            arguments["path"] = Value::String(path.to_string_lossy().into_owned());
        }
        let call = ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.to_string(),
            },
        };
        let mut context = self.tools.context(&self.project).await;
        context.max_output_chars = context.max_output_chars.min(MAX_PAYLOAD_BYTES / 2);
        let execution = self.tools.execute(&call, &context, cancel).await?;
        let json = serde_json::to_string(&execution.output)
            .map_err(|error| AgentError::internal(format!("Encode host tool output: {error}")))?;
        if json.len() > MAX_PAYLOAD_BYTES {
            return Err(AgentError::new(
                code::PLUGIN_RESOURCE_LIMIT,
                "Host tool output exceeds the payload limit",
            ));
        }
        Ok(json)
    }

    /// Starts a provider-owned process transport after checking its declaration.
    ///
    /// MCP JSON-RPC and framing decisions remain in the Wasm provider. The
    /// host only owns the child process and exposes bounded byte I/O.
    pub async fn spawn_process(
        &self,
        command: &str,
        arguments_json: &str,
        cwd: &str,
        environment_json: &str,
    ) -> Result<u64> {
        let arguments: Vec<String> = serde_json::from_str(arguments_json)
            .map_err(|_| AgentError::invalid_params("Process arguments must be a JSON array"))?;
        let environment: BTreeMap<String, String> = serde_json::from_str(environment_json)
            .map_err(|_| AgentError::invalid_params("Process environment must be a JSON object"))?;
        if command.is_empty() || arguments.len() > 128 || environment.len() > 128 {
            return Err(AgentError::invalid_params("Invalid process declaration"));
        }
        let Some(config) = self.permissions.mcp_servers.iter().find_map(|id| {
            self.mcp_servers
                .get(id)
                .filter(|config| !config.is_http())
                .filter(|config| config.command.as_deref() == Some(command))
        }) else {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Process transport was not declared by this provider",
            ));
        };
        if config.args != arguments || config.env != environment {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Process transport arguments or environment were not declared by this provider",
            ));
        }
        let declared_cwd = config.cwd.as_deref().unwrap_or("");
        let cwd = if cwd.is_empty() {
            if declared_cwd.is_empty() {
                self.plugin_root.clone()
            } else {
                self.resolve_plugin_path(declared_cwd).await?
            }
        } else {
            if cwd != declared_cwd {
                return Err(AgentError::new(
                    code::PLUGIN_PERMISSION_DENIED,
                    "Process transport working directory was not declared by this provider",
                ));
            }
            self.resolve_plugin_path(cwd).await?
        };
        if self.raw.processes.lock().await.len() >= MAX_TRANSPORT_HANDLES {
            return Err(AgentError::new(
                code::PLUGIN_RESOURCE_LIMIT,
                "Too many process transport handles",
            ));
        }
        let mut process = tokio::process::Command::new(command);
        process
            .args(arguments)
            .current_dir(cwd)
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        crate::process::hide_console(&mut process);
        let mut child = process
            .spawn()
            .map_err(|error| AgentError::from_io("Start plugin process transport", error))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.start_kill();
            return Err(AgentError::internal(
                "Plugin process transport did not provide pipes",
            ));
        };
        let handle = self.raw.next_process.fetch_add(1, Ordering::Relaxed);
        self.raw.processes.lock().await.insert(
            handle,
            Arc::new(Mutex::new(ProcessHandle {
                child,
                stdin,
                stdout,
            })),
        );
        Ok(handle)
    }

    /// Writes raw bytes to one provider-owned process transport.
    pub async fn process_write(&self, handle: u64, data: &[u8]) -> Result<()> {
        if data.len() > MAX_PAYLOAD_BYTES {
            return Err(AgentError::new(
                code::PLUGIN_RESOURCE_LIMIT,
                "Process transport write exceeds the payload limit",
            ));
        }
        let process = self
            .raw
            .processes
            .lock()
            .await
            .get(&handle)
            .cloned()
            .ok_or_else(|| AgentError::invalid_params("Unknown process transport handle"))?;
        let mut process = process.lock().await;
        process
            .stdin
            .write_all(data)
            .await
            .map_err(|error| AgentError::from_io("Write plugin process transport", error))?;
        process
            .stdin
            .flush()
            .await
            .map_err(|error| AgentError::from_io("Flush plugin process transport", error))
    }

    /// Reads raw bytes from one provider-owned process transport.
    pub async fn process_read(&self, handle: u64, max_bytes: u32) -> Result<Vec<u8>> {
        let max_bytes = bounded_read_size(max_bytes)?;
        let process = self
            .raw
            .processes
            .lock()
            .await
            .get(&handle)
            .cloned()
            .ok_or_else(|| AgentError::invalid_params("Unknown process transport handle"))?;
        let mut process = process.lock().await;
        let mut data = vec![0; max_bytes];
        let bytes = process
            .stdout
            .read(&mut data)
            .await
            .map_err(|error| AgentError::from_io("Read plugin process transport", error))?;
        if bytes == 0 {
            return Ok(Vec::new());
        }
        data.truncate(bytes);
        Ok(data)
    }

    /// Stops one provider-owned process transport.
    pub async fn process_close(&self, handle: u64) {
        if let Some(process) = self.raw.processes.lock().await.remove(&handle) {
            let mut process = process.lock().await;
            let _ = process.child.start_kill();
            let _ = process.child.wait().await;
        }
    }

    /// Starts a raw HTTP request; response framing remains in the provider.
    pub async fn http_request(
        &self,
        method: &str,
        url: &str,
        headers_json: &str,
        body: &str,
    ) -> Result<String> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|_| AgentError::invalid_params("HTTP transport URL is invalid"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(AgentError::invalid_params(
                "HTTP transport URL must use http or https",
            ));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| AgentError::invalid_params("HTTP transport URL has no host"))?;
        if !self
            .permissions
            .network_hosts
            .iter()
            .any(|allowed| allowed == host)
        {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "HTTP transport host was not granted",
            ));
        }
        if !self.permissions.mcp_servers.iter().any(|id| {
            self.mcp_servers
                .get(id)
                .filter(|config| config.is_http())
                .and_then(|config| config.url.as_deref())
                == Some(url)
        }) {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "HTTP transport URL was not declared by this provider",
            ));
        }
        if headers_json.len() > MAX_PAYLOAD_BYTES || body.len() > MAX_PAYLOAD_BYTES {
            return Err(AgentError::new(
                code::PLUGIN_RESOURCE_LIMIT,
                "HTTP transport request exceeds the payload limit",
            ));
        }
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| AgentError::invalid_params("HTTP method is invalid"))?;
        let headers: BTreeMap<String, String> = serde_json::from_str(headers_json)
            .map_err(|_| AgentError::invalid_params("HTTP headers must be a JSON object"))?;
        let mut header_map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::try_from(name)
                .map_err(|_| AgentError::invalid_params("Invalid HTTP header name"))?;
            let value = reqwest::header::HeaderValue::try_from(value)
                .map_err(|_| AgentError::invalid_params("Invalid HTTP header value"))?;
            header_map.insert(name, value);
        }
        let response = self
            .raw
            .http
            .request(method, url)
            .headers(header_map)
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| {
                AgentError::internal(format!("Plugin HTTP transport failed: {error}"))
            })?;
        let status = response.status().as_u16();
        let response_headers: BTreeMap<String, String> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
            .collect();
        let encoded_headers = serde_json::to_string(&response_headers).map_err(|error| {
            AgentError::internal(format!("Encode HTTP response headers: {error}"))
        })?;
        if encoded_headers.len() > MAX_PAYLOAD_BYTES {
            return Err(AgentError::new(
                code::PLUGIN_RESOURCE_LIMIT,
                "HTTP transport response headers exceed the payload limit",
            ));
        }
        if self.raw.responses.lock().await.len() >= MAX_TRANSPORT_HANDLES {
            return Err(AgentError::new(
                code::PLUGIN_RESOURCE_LIMIT,
                "Too many HTTP response handles",
            ));
        }
        let handle = self.raw.next_http.fetch_add(1, Ordering::Relaxed);
        self.raw.responses.lock().await.insert(
            handle,
            Arc::new(Mutex::new(HttpHandle {
                response,
                pending: Vec::new(),
            })),
        );
        serde_json::to_string(&serde_json::json!({
            "status": status,
            "headers": response_headers,
            "handle": handle,
        }))
        .map_err(|error| AgentError::internal(format!("Encode plugin HTTP response: {error}")))
    }

    /// Reads raw response bytes from a provider-owned HTTP response.
    pub async fn http_read(&self, handle: u64, max_bytes: u32) -> Result<Vec<u8>> {
        let max_bytes = bounded_read_size(max_bytes)?;
        let response = self
            .raw
            .responses
            .lock()
            .await
            .get(&handle)
            .cloned()
            .ok_or_else(|| AgentError::invalid_params("Unknown HTTP response handle"))?;
        let mut response = response.lock().await;
        if !response.pending.is_empty() {
            let take = response.pending.len().min(max_bytes);
            return Ok(response.pending.drain(..take).collect());
        }
        let Some(chunk) =
            response.response.chunk().await.map_err(|error| {
                AgentError::internal(format!("Read plugin HTTP response: {error}"))
            })?
        else {
            return Ok(Vec::new());
        };
        if chunk.len() <= max_bytes {
            return Ok(chunk.to_vec());
        }
        let (head, tail) = chunk.split_at(max_bytes);
        response.pending.extend_from_slice(tail);
        Ok(head.to_vec())
    }

    /// Releases a provider-owned HTTP response.
    pub async fn http_close(&self, handle: u64) {
        self.raw.responses.lock().await.remove(&handle);
    }

    async fn resolve_plugin_path(&self, relative: &str) -> Result<PathBuf> {
        let root = tokio::fs::canonicalize(&self.plugin_root)
            .await
            .map_err(|error| AgentError::from_io("Resolve plugin transport root", error))?;
        let path = tokio::fs::canonicalize(root.join(relative))
            .await
            .map_err(|error| AgentError::from_io("Resolve plugin transport directory", error))?;
        if !path.starts_with(&root) {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Process working directory must stay inside the plugin",
            ));
        }
        Ok(path)
    }
}

fn bounded_read_size(max_bytes: u32) -> Result<usize> {
    let max_bytes = usize::try_from(max_bytes)
        .map_err(|_| AgentError::invalid_params("Read size is invalid"))?;
    if max_bytes == 0 || max_bytes > MAX_PAYLOAD_BYTES {
        return Err(AgentError::invalid_params(
            "Read size exceeds the payload limit",
        ));
    }
    Ok(max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::RwLock;

    use crate::harness::ports::{ToolContext, ToolExecution};
    use crate::harness::AuditOutcome;
    use crate::tools::{ToolDescriptor, ToolOutput, ToolRegistry, ToolSettings};

    struct FakeRuntime {
        calls: AtomicUsize,
        output: ToolOutput,
    }

    #[async_trait]
    impl ToolRuntime for FakeRuntime {
        fn descriptors(&self) -> Vec<ToolDescriptor> {
            Vec::new()
        }

        async fn context(&self, project: &std::path::Path) -> ToolContext {
            ToolContext {
                project: project.to_path_buf(),
                working_directory: project.to_path_buf(),
                timeout: Duration::from_secs(1),
                max_output_chars: MAX_PAYLOAD_BYTES,
            }
        }

        async fn execute(
            &self,
            _call: &ToolCall,
            _context: &ToolContext,
            _cancel: &CancellationToken,
        ) -> Result<ToolExecution> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolExecution {
                output: self.output.clone(),
                outcome: AuditOutcome::Executed,
            })
        }
    }

    fn permissions(names: &[&str]) -> Permissions {
        Permissions {
            invoke_tools: names.iter().map(|name| (*name).to_string()).collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn unlisted_tools_and_undeclared_transports_are_denied_before_dispatch() {
        let runtime = Arc::new(FakeRuntime {
            calls: AtomicUsize::new(0),
            output: ToolOutput::text("unexpected"),
        });
        let project = tempfile::tempdir().expect("a temporary project is available");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            permissions(&["read_file"]),
            Default::default(),
            runtime.clone(),
        )
        .expect("raw host");
        let cancel = CancellationToken::new();

        let error = hub
            .invoke("exec", "{}", &cancel)
            .await
            .expect_err("an unlisted tool capability is denied");
        assert_eq!(error.code, code::PLUGIN_PERMISSION_DENIED);
        assert_eq!(
            runtime.calls.load(Ordering::SeqCst),
            0,
            "denied tool capabilities must not reach normal tool dispatch"
        );

        let error = hub
            .spawn_process("python", "[]", ".", "{}")
            .await
            .expect_err("an undeclared process transport is denied");
        assert_eq!(error.code, code::PLUGIN_PERMISSION_DENIED);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);

        let error = hub
            .http_request("POST", "https://mcp.example.test/mcp", "{}", "{}")
            .await
            .expect_err("an undeclared HTTP transport is denied");
        assert_eq!(error.code, code::PLUGIN_PERMISSION_DENIED);
    }

    #[tokio::test]
    async fn project_read_capability_uses_the_normal_tool_runtime() {
        let project = tempfile::tempdir().expect("a temporary project is available");
        std::fs::write(project.path().join("note.txt"), "hello from the project")
            .expect("the project fixture is writable");
        let settings = Arc::new(RwLock::new(ToolSettings {
            working_directory: project.path().to_path_buf(),
            ..Default::default()
        }));
        let runtime: Arc<dyn ToolRuntime> =
            Arc::new(crate::harness::services::RegistryToolRuntime::new(
                Arc::new(ToolRegistry::with_builtins()),
                settings,
            ));
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            permissions(&["read_file"]),
            Default::default(),
            runtime,
        )
        .expect("raw host");

        let output = hub
            .invoke(
                "read_file",
                r#"{"path":"note.txt","lineNumbers":false}"#,
                &CancellationToken::new(),
            )
            .await
            .expect("an allowed project read succeeds");
        assert!(
            output.contains("hello from the project"),
            "the capability should return the native read_file result"
        );
    }

    #[tokio::test]
    async fn http_transport_returns_raw_response_bytes_for_provider_framing() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the loopback listener is available");
        let url = format!(
            "http://{}/mcp",
            listener.local_addr().expect("the listener has an address")
        );
        let response_body = b"data: {\"jsonrpc\":\"2.0\"}\n\n";
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("the provider connects");
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            let header_end = loop {
                let read = socket
                    .read(&mut chunk)
                    .await
                    .expect("the request can be read");
                if read == 0 {
                    break None;
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break Some(end + 4);
                }
            };
            if let Some(header_end) = header_end {
                let content_length = String::from_utf8_lossy(&request[..header_end])
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let mut remaining = content_length.saturating_sub(request.len() - header_end);
                while remaining > 0 {
                    let read = socket
                        .read(&mut chunk)
                        .await
                        .expect("the request body can be read");
                    if read == 0 {
                        break;
                    }
                    remaining = remaining.saturating_sub(read);
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("the response head is written");
            socket
                .write_all(response_body)
                .await
                .expect("the response body is written");
        });
        let project = tempfile::tempdir().expect("a temporary project is available");
        let mcp_servers = BTreeMap::from([(
            "fixture".to_string(),
            super::super::manifest::McpServerConfig {
                transport: Some("http".into()),
                url: Some(url.clone()),
                oauth_resource: None,
                command: None,
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                startup_timeout_sec: None,
            },
        )]);
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions {
                mcp_servers: vec!["fixture".into()],
                network_hosts: vec!["127.0.0.1".into()],
                ..Default::default()
            },
            mcp_servers,
            Arc::new(FakeRuntime {
                calls: AtomicUsize::new(0),
                output: ToolOutput::text("unused"),
            }),
        )
        .expect("raw host");

        let metadata = hub
            .http_request("POST", &url, r#"{"content-type":"application/json"}"#, "{}")
            .await
            .expect("the declared HTTP transport starts");
        let handle = serde_json::from_str::<Value>(&metadata)
            .expect("the response metadata is JSON")
            .get("handle")
            .and_then(Value::as_u64)
            .expect("the response metadata contains a handle");
        let bytes = hub
            .http_read(handle, MAX_PAYLOAD_BYTES as u32)
            .await
            .expect("the response bytes are readable");
        assert_eq!(
            bytes, response_body,
            "SSE framing must reach the provider without host parsing"
        );
        hub.http_close(handle).await;
        server.await.expect("the loopback server exits");
    }

    #[tokio::test]
    async fn project_read_capability_rejects_parent_paths() {
        let project = tempfile::tempdir().expect("a temporary project is available");
        let outside = tempfile::NamedTempFile::new_in(
            project
                .path()
                .parent()
                .expect("the temporary project has a parent"),
        )
        .expect("an outside fixture is writable");
        std::fs::write(outside.path(), "outside").expect("the outside fixture is writable");
        let runtime = Arc::new(FakeRuntime {
            calls: AtomicUsize::new(0),
            output: ToolOutput::text("unexpected"),
        });
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            permissions(&["read_file"]),
            Default::default(),
            runtime.clone(),
        )
        .expect("raw host");
        let name = outside
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("the temporary filename is UTF-8");
        let path = format!("../{name}");

        let error = hub
            .invoke(
                "read_file",
                &serde_json::json!({ "path": path }).to_string(),
                &CancellationToken::new(),
            )
            .await
            .expect_err("a capability path must stay inside the project");
        assert_eq!(error.code, code::PLUGIN_PERMISSION_DENIED);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn host_output_limit_applies_after_normal_dispatch() {
        let runtime = Arc::new(FakeRuntime {
            calls: AtomicUsize::new(0),
            output: ToolOutput::text("x".repeat(MAX_PAYLOAD_BYTES)),
        });
        let project = tempfile::tempdir().expect("a temporary project is available");
        std::fs::write(project.path().join("unused.txt"), "fixture")
            .expect("the capability path exists");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            permissions(&["read_file"]),
            Default::default(),
            runtime.clone(),
        )
        .expect("raw host");

        let error = hub
            .invoke(
                "read_file",
                r#"{"path":"unused.txt"}"#,
                &CancellationToken::new(),
            )
            .await
            .expect_err("oversized host output is refused");
        assert_eq!(error.code, code::PLUGIN_RESOURCE_LIMIT);
        assert_eq!(
            runtime.calls.load(Ordering::SeqCst),
            1,
            "the output bound applies after the normal dispatch contract"
        );
    }
}
