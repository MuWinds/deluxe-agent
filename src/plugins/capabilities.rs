//! Explicit host imports; components never receive ambient filesystem access.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
use crate::harness::ports::{NestedAgentRuntime, ToolRuntime};
use crate::llm::{FunctionCall, ToolCall};

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
#[derive(Clone)]
pub struct PluginFileHost {
    configuration_root: PathBuf,
}

impl PluginFileHost {
    /// Binds generic file reads to one global or project configuration root.
    pub fn new(configuration_root: PathBuf) -> Self {
        Self { configuration_root }
    }

    /// Reads raw bytes from a relative file below the bound configuration root.
    ///
    /// Returns `Err` for absolute paths, parent traversal, symlinks that leave
    /// the root, missing files, or directories. The host does not interpret the
    /// file name or its contents.
    pub async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let relative = relative_plugin_path(path)?;
        let root = self.canonical_root().await?;
        let path = tokio::fs::canonicalize(root.join(relative))
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    AgentError::new(
                        code::PLUGIN_FILE_NOT_FOUND,
                        "Plugin file does not exist under the configuration root",
                    )
                } else {
                    AgentError::from_io("Resolve plugin file", error)
                }
            })?;
        if !path.starts_with(&root) {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Plugin file access must stay inside the configuration root",
            ));
        }
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| AgentError::from_io("Read plugin file metadata", error))?;
        if !metadata.is_file() {
            return Err(AgentError::invalid_params("Plugin file path is not a file"));
        }
        tokio::fs::read(path)
            .await
            .map_err(|error| AgentError::from_io("Read plugin file", error))
    }

    /// Lists the regular files below a relative directory under the bound
    /// configuration root.
    ///
    /// Paths are returned relative to the root, `/`-separated, and sorted, so a
    /// caller that turns them into prompt text stays byte-stable across runs.
    /// Returns `Err` for absolute paths, parent traversal, a missing root or
    /// directory, or a target that is not a directory. Symlinks are never
    /// followed, so a link cannot lead the listing outside the root.
    pub async fn list(&self, path: &str) -> Result<Vec<String>> {
        let relative = relative_plugin_path(path)?;
        let root = self.canonical_root().await?;
        let target = tokio::fs::canonicalize(root.join(relative))
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    AgentError::new(
                        code::PLUGIN_FILE_NOT_FOUND,
                        "Plugin directory does not exist under the configuration root",
                    )
                } else {
                    AgentError::from_io("Resolve plugin directory", error)
                }
            })?;
        if !target.starts_with(&root) {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Plugin file access must stay inside the configuration root",
            ));
        }
        let metadata = tokio::fs::metadata(&target)
            .await
            .map_err(|error| AgentError::from_io("Read plugin directory metadata", error))?;
        if !metadata.is_dir() {
            return Err(AgentError::invalid_params("Plugin path is not a directory"));
        }
        tokio::task::spawn_blocking(move || walk_files(&root, &target))
            .await
            .map_err(|error| {
                AgentError::internal(format!("Plugin directory walk failed: {error}"))
            })?
    }

    /// Replaces a relative file below the bound configuration root.
    ///
    /// Returns `Err` for absolute paths, parent traversal, a missing root, an
    /// oversized payload, or a target that leaves the root through a symlinked
    /// parent. The file is created when absent and committed with a same-directory
    /// rename, so a reader never observes a half-written config. The root must
    /// already exist: a write is not what should create a scope directory.
    pub async fn write(&self, path: &str, contents: &[u8]) -> Result<()> {
        let relative = relative_plugin_path(path)?;

        let root = self.canonical_root().await?;
        let target = root.join(&relative);
        // The file itself need not exist yet, so the *parent* is what gets
        // canonicalised — which is what refuses a symlinked directory that
        // leaves the scope, since the symlink is followed before the check.
        let name = target
            .file_name()
            .ok_or_else(|| AgentError::invalid_params("Plugin file needs a file name"))?;
        let parent = target
            .parent()
            .ok_or_else(|| AgentError::invalid_params("Plugin file needs a parent directory"))?;
        let parent = tokio::fs::canonicalize(parent)
            .await
            .map_err(|error| AgentError::from_io("Resolve plugin file parent", error))?;
        if !parent.starts_with(&root) {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Plugin file access must stay inside the configuration root",
            ));
        }
        let target = parent.join(name);
        match tokio::fs::symlink_metadata(&target).await {
            Ok(metadata) if metadata.is_file() => {}
            // A symlink or a directory is never overwritten: following it would
            // make the write land outside the check above.
            Ok(_) => {
                return Err(AgentError::new(
                    code::PLUGIN_PERMISSION_DENIED,
                    "Plugin file target is not a regular file",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(AgentError::from_io("Inspect plugin file", error)),
        }
        let temp = parent.join(format!(".plugin-write-{}.tmp", uuid::Uuid::new_v4()));
        if let Err(error) = tokio::fs::write(&temp, contents).await {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(AgentError::from_io("Write plugin file", error));
        }
        if let Err(error) = tokio::fs::rename(&temp, &target).await {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(AgentError::from_io("Commit plugin file", error));
        }
        Ok(())
    }

    async fn canonical_root(&self) -> Result<PathBuf> {
        match tokio::fs::canonicalize(&self.configuration_root).await {
            Ok(root) => Ok(root),
            // The scope root itself is allowed to be absent — a fresh install
            // has no `~/.deluxe-agents` yet — and a Component asking for its
            // optional config wants a not-found, not an opaque resolve failure.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(AgentError::new(
                code::PLUGIN_FILE_NOT_FOUND,
                "Plugin configuration root does not exist",
            )),
            Err(error) => Err(AgentError::from_io(
                "Resolve plugin configuration root",
                error,
            )),
        }
    }
}

/// Accepts a relative path that names a file below a plugin configuration root.
///
/// Returns `Err` for a blank path or any absolute or parent component, which is
/// the check both the read and the write capability share.
fn relative_plugin_path(path: &str) -> Result<PathBuf> {
    let relative = PathBuf::from(path);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(AgentError::new(
            code::PLUGIN_PERMISSION_DENIED,
            "Plugin file access requires a relative path inside the configuration root",
        ));
    }
    Ok(relative)
}

/// Walks `dir` for regular files, skipping symlinks.
///
/// Blocking on purpose: it runs inside `spawn_blocking`, where the recursive
/// `read_dir` is cheaper than hopping the async runtime for every entry.
fn walk_files(root: &Path, dir: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|error| AgentError::from_io("Read plugin directory", error))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| AgentError::from_io("Read plugin directory entry", error))?;
            // `symlink_metadata` never follows a link, so a symlinked directory
            // is skipped rather than walked into — the canonical check only
            // covered the directory the walk started from.
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| AgentError::from_io("Read plugin entry metadata", error))?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                found.push(relative_slash_path(root, &entry.path()));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Renders `path` relative to `root` with forward slashes, for a portable ABI.
fn relative_slash_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

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
    pub configuration_root: PathBuf,
    pub permissions: Permissions,
    pub tools: Arc<dyn ToolRuntime>,
    /// Present only for a Component that may delegate work to a nested agent.
    agent: Option<Arc<dyn NestedAgentRuntime>>,
    files: PluginFileHost,
    raw: RawHost,
}

impl CapabilityHub {
    /// Creates the capabilities available to one project-scoped component.
    pub fn new(
        project: PathBuf,
        configuration_root: PathBuf,
        permissions: Permissions,
        tools: Arc<dyn ToolRuntime>,
    ) -> Result<Self> {
        Ok(Self {
            project,
            files: PluginFileHost::new(configuration_root.clone()),
            configuration_root,
            permissions,
            tools,
            agent: None,
            raw: RawHost::new()?,
        })
    }

    /// Grants the nested-agent capability a Component reaches through
    /// `run-agent`.
    ///
    /// Off by default: only a Component whose whole job is delegation is given
    /// a model loop it can spend.
    pub fn with_nested_agent(mut self, agent: Arc<dyn NestedAgentRuntime>) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Reads raw bytes from the current global/project configuration root.
    pub async fn read_plugin_file(&self, path: &str) -> Result<Vec<u8>> {
        self.files.read(path).await
    }

    /// Lists the files below a relative directory in the current configuration
    /// root, as `/`-separated paths relative to that root.
    ///
    /// A read-shaped capability, so it needs no permission beyond the bound
    /// root. Returns `Err` for every reason [`PluginFileHost::list`] reports.
    pub async fn list_plugin_files(&self, path: &str) -> Result<Vec<String>> {
        self.files.list(path).await
    }

    /// Replaces a file in the current configuration root, when the plugin
    /// declared the `writePluginFiles` permission.
    ///
    /// Returns `Err` when the permission was not granted, in addition to every
    /// reason [`PluginFileHost::write`] reports. The host still parses nothing:
    /// what a Component stores in its own scope config is its business.
    pub async fn write_plugin_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        if !self.permissions.write_plugin_files {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "This plugin file write capability was not granted",
            ));
        }
        self.files.write(path, contents).await
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
    /// A host tool is not confined to the project directory: the model has full
    /// access to the machine, so a path outside the project is passed through
    /// like any other. Relative paths are resolved by the tool itself, against
    /// the project's working directory. `Err` reports denied capabilities,
    /// malformed JSON, cancellation, or tool failures.
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
        let arguments: Value = serde_json::from_str(arguments)
            .map_err(|_| AgentError::invalid_params("Host tool arguments must be JSON"))?;
        let call = ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.to_string(),
            },
        };
        let context = self.tools.context(&self.project).await;
        let execution = self.tools.execute(&call, &context, cancel).await?;
        let json = serde_json::to_string(&execution.output)
            .map_err(|error| AgentError::internal(format!("Encode host tool output: {error}")))?;
        Ok(json)
    }

    /// Runs one nested agent for a Component that exports a delegating tool.
    ///
    /// Returns `Err` when this Component was not granted the capability, when
    /// the request is malformed, or when the nested agent fails.
    pub async fn run_agent(
        &self,
        request_json: &str,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let Some(agent) = &self.agent else {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "This host capability was not granted",
            ));
        };
        agent.run(request_json, cancel).await
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
        if !self
            .permissions
            .process_commands
            .iter()
            .any(|allowed| allowed == "*" || allowed == command)
        {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "This process capability was not granted",
            ));
        }
        let cwd = if cwd.is_empty() {
            self.configuration_root.clone()
        } else {
            self.resolve_scope_path(cwd).await?
        };
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
        let max_bytes = read_size(max_bytes)?;
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
            .any(|allowed| allowed == "*" || allowed == host)
        {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "HTTP transport host was not granted",
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
        let max_bytes = read_size(max_bytes)?;
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

    async fn resolve_scope_path(&self, relative: &str) -> Result<PathBuf> {
        let relative = PathBuf::from(relative);
        if relative.is_absolute()
            || relative.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Process working directory must be a relative scope path",
            ));
        }
        let root = tokio::fs::canonicalize(&self.configuration_root)
            .await
            .map_err(|error| AgentError::from_io("Resolve process configuration root", error))?;
        let path = tokio::fs::canonicalize(root.join(relative))
            .await
            .map_err(|error| AgentError::from_io("Resolve plugin transport directory", error))?;
        if !path.starts_with(&root) {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Process working directory must stay inside the configuration root",
            ));
        }
        Ok(path)
    }
}

/// Converts a transport read size, refusing only a zero-byte request.
///
/// There is no upper bound: a plugin's transport reads are limited by nothing
/// but its own fuel budget.
fn read_size(max_bytes: u32) -> Result<usize> {
    let max_bytes = usize::try_from(max_bytes)
        .map_err(|_| AgentError::invalid_params("Read size is invalid"))?;
    if max_bytes == 0 {
        return Err(AgentError::invalid_params("Read size must be non-zero"));
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
                max_output_chars: 256 * 1024,
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
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions {
                network_hosts: vec!["127.0.0.1".into()],
                ..Default::default()
            },
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
            .http_read(handle, 256 * 1024)
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
    async fn project_read_capability_forwards_paths_outside_the_project() {
        // There is no sandbox: a host read is not confined to the project, so a
        // parent-relative path reaches the tool runtime instead of being denied.
        let project = tempfile::tempdir().expect("a temporary project is available");
        let runtime = Arc::new(FakeRuntime {
            calls: AtomicUsize::new(0),
            output: ToolOutput::text("outside"),
        });
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            permissions(&["read_file"]),
            runtime.clone(),
        )
        .expect("raw host");

        let output = hub
            .invoke(
                "read_file",
                &serde_json::json!({ "path": "../elsewhere.txt" }).to_string(),
                &CancellationToken::new(),
            )
            .await
            .expect("a host read outside the project is allowed");
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        assert!(
            output.contains("outside"),
            "the runtime's result should be returned: {output}"
        );
    }

    #[tokio::test]
    async fn plugin_file_reads_return_raw_bytes_from_the_bound_scope_root() {
        let global = tempfile::tempdir().expect("a temporary global root is available");
        let project = tempfile::tempdir().expect("a temporary project root is available");
        let raw = [0xff, 0x00, b'x'];
        std::fs::write(global.path().join(".mcp.json"), raw)
            .expect("the global config fixture is writable");
        std::fs::write(project.path().join(".hooks.json"), b"project hooks")
            .expect("the project config fixture is writable");
        let runtime = Arc::new(FakeRuntime {
            calls: AtomicUsize::new(0),
            output: ToolOutput::text("unused"),
        });
        let global_hub = CapabilityHub::new(
            project.path().to_path_buf(),
            global.path().to_path_buf(),
            Permissions::default(),
            runtime.clone(),
        )
        .expect("global scope host");
        let project_hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions::default(),
            runtime,
        )
        .expect("project scope host");

        assert_eq!(
            global_hub
                .read_plugin_file(".mcp.json")
                .await
                .expect("the global file is readable"),
            raw,
            "file reads preserve arbitrary bytes without decoding"
        );
        assert_eq!(
            project_hub
                .read_plugin_file(".hooks.json")
                .await
                .expect("the project file is readable"),
            b"project hooks",
            "project-scoped components read from the project root"
        );
        let missing = project_hub
            .read_plugin_file(".mcp.json")
            .await
            .expect_err("a missing scoped config is distinguishable");
        assert_eq!(missing.code, code::PLUGIN_FILE_NOT_FOUND);
    }

    #[tokio::test]
    async fn plugin_file_reads_reject_absolute_and_parent_paths() {
        let project = tempfile::tempdir().expect("a temporary project root is available");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions::default(),
            Arc::new(FakeRuntime {
                calls: AtomicUsize::new(0),
                output: ToolOutput::text("unused"),
            }),
        )
        .expect("scope host");

        for path in ["../outside", project.path().to_string_lossy().as_ref()] {
            let error = hub
                .read_plugin_file(path)
                .await
                .expect_err("plugin file access must stay relative to its scope root");
            assert_eq!(
                error.code,
                code::PLUGIN_PERMISSION_DENIED,
                "expected `{path}` to be denied"
            );
        }
    }

    #[tokio::test]
    async fn plugin_directory_listing_returns_sorted_relative_paths() {
        let project = tempfile::tempdir().expect("a temporary project root is available");
        let config = project.path().join("config");
        std::fs::create_dir_all(config.join("alpha")).expect("the fixture is writable");
        std::fs::create_dir_all(config.join("beta/references")).expect("the fixture is writable");
        std::fs::write(config.join("alpha/entry.md"), "a").expect("the fixture is writable");
        std::fs::write(config.join("beta/entry.md"), "b").expect("the fixture is writable");
        std::fs::write(config.join("beta/references/notes.md"), "c")
            .expect("the fixture is writable");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions::default(),
            Arc::new(FakeRuntime {
                calls: AtomicUsize::new(0),
                output: ToolOutput::text("unused"),
            }),
        )
        .expect("scope host");

        assert_eq!(
            hub.list_plugin_files("config")
                .await
                .expect("the directory is listable"),
            vec![
                "config/alpha/entry.md",
                "config/beta/entry.md",
                "config/beta/references/notes.md",
            ],
            "paths are relative to the root, `/`-separated, and sorted"
        );
    }

    #[tokio::test]
    async fn plugin_directory_listing_distinguishes_missing_and_non_directory_targets() {
        let project = tempfile::tempdir().expect("a temporary project root is available");
        std::fs::write(project.path().join("note.txt"), "x").expect("the fixture is writable");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions::default(),
            Arc::new(FakeRuntime {
                calls: AtomicUsize::new(0),
                output: ToolOutput::text("unused"),
            }),
        )
        .expect("scope host");

        let missing = hub
            .list_plugin_files("config")
            .await
            .expect_err("a missing directory is reported, so a caller can tell it from a failure");
        assert_eq!(missing.code, code::PLUGIN_FILE_NOT_FOUND);

        let not_a_directory = hub
            .list_plugin_files("note.txt")
            .await
            .expect_err("a file is not a directory");
        assert_eq!(not_a_directory.code, code::INVALID_PARAMS);

        for path in ["../outside", "/etc"] {
            let error = hub
                .list_plugin_files(path)
                .await
                .expect_err("listing must stay relative to its scope root");
            assert_eq!(error.code, code::PLUGIN_PERMISSION_DENIED);
        }
    }

    /// A symlinked directory must not be walked into: it could point anywhere,
    /// and the canonical check only covered the directory the walk started at.
    #[cfg(unix)]
    #[tokio::test]
    async fn plugin_directory_listing_does_not_follow_symlinks() {
        let project = tempfile::tempdir().expect("a temporary project root is available");
        let outside = tempfile::tempdir().expect("a temporary outside root is available");
        std::fs::write(outside.path().join("secret.md"), "x").expect("the fixture is writable");
        let config = project.path().join("config");
        std::fs::create_dir_all(&config).expect("the fixture is writable");
        std::fs::write(config.join("real.md"), "x").expect("the fixture is writable");
        std::os::unix::fs::symlink(outside.path(), config.join("escape"))
            .expect("the fixture symlink is created");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions::default(),
            Arc::new(FakeRuntime {
                calls: AtomicUsize::new(0),
                output: ToolOutput::text("unused"),
            }),
        )
        .expect("scope host");

        assert_eq!(
            hub.list_plugin_files("config")
                .await
                .expect("the directory is listable"),
            vec!["config/real.md"],
            "a symlink is skipped rather than followed out of the root"
        );
    }

    #[tokio::test]
    async fn plugin_file_writes_round_trip_and_stay_inside_the_scope_root() {
        let global = tempfile::tempdir().expect("a temporary global root is available");
        let project = tempfile::tempdir().expect("a temporary project root is available");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            global.path().to_path_buf(),
            Permissions {
                write_plugin_files: true,
                ..Default::default()
            },
            Arc::new(FakeRuntime {
                calls: AtomicUsize::new(0),
                output: ToolOutput::text("unused"),
            }),
        )
        .expect("scope host");

        hub.write_plugin_file(".mcp.json", br#"{"mcpServers":{}}"#)
            .await
            .expect("a declared write reaches the bound scope root");
        assert_eq!(
            hub.read_plugin_file(".mcp.json")
                .await
                .expect("the written file is readable"),
            br#"{"mcpServers":{}}"#,
            "a write is committed to the configuration root the hub was bound to"
        );
        assert!(
            global.path().join(".mcp.json").is_file(),
            "the file lands in the scope root, not in the plugin's own directory"
        );

        // A second write replaces the file rather than appending to it.
        hub.write_plugin_file(".mcp.json", b"{}")
            .await
            .expect("an existing file is replaced");
        assert_eq!(
            std::fs::read(global.path().join(".mcp.json")).expect("the file is readable"),
            b"{}"
        );
        assert!(
            std::fs::read_dir(global.path())
                .expect("the scope root is readable")
                .all(|entry| !entry
                    .expect("the entry is readable")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".plugin-write-")),
            "the temporary file used to commit the write is not left behind"
        );

        for path in ["../outside.json", "/etc/passwd"] {
            let error = hub
                .write_plugin_file(path, b"nope")
                .await
                .expect_err("a write must stay inside the configuration root");
            assert_eq!(error.code, code::PLUGIN_PERMISSION_DENIED);
        }
    }

    #[tokio::test]
    async fn plugin_file_writes_need_an_explicit_permission() {
        let project = tempfile::tempdir().expect("a temporary project root is available");
        let hub = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions::default(),
            Arc::new(FakeRuntime {
                calls: AtomicUsize::new(0),
                output: ToolOutput::text("unused"),
            }),
        )
        .expect("scope host");

        let error = hub
            .write_plugin_file(".mcp.json", b"{}")
            .await
            .expect_err("a component that did not declare the permission cannot write");
        assert_eq!(error.code, code::PLUGIN_PERMISSION_DENIED);
        assert!(
            !project.path().join(".mcp.json").exists(),
            "a denied write leaves no file behind"
        );

        let permitted = CapabilityHub::new(
            project.path().to_path_buf(),
            project.path().to_path_buf(),
            Permissions {
                write_plugin_files: true,
                ..Default::default()
            },
            Arc::new(FakeRuntime {
                calls: AtomicUsize::new(0),
                output: ToolOutput::text("unused"),
            }),
        )
        .expect("scope host");
        permitted
            .write_plugin_file(".mcp.json", b"{}")
            .await
            .expect("a declared write is allowed");
        assert_eq!(
            permitted
                .read_plugin_file(".mcp.json")
                .await
                .expect("the written file is readable"),
            b"{}"
        );
    }
}
