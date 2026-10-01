//! Component actors own their stores and service bounded calls on worker threads.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use wasmtime::component::{Accessor, Component, HasData, Instance as ComponentInstance, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};

use crate::error::{code, AgentError, Result};

use super::capabilities::CapabilityHub;
use super::ui_protocol::MAX_PAYLOAD_BYTES;
use super::wasm_manifest::WasmManifest;

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "harness-plugin",
        imports: { default: async },
        exports: { default: async },
    });
}

const CALL_FUEL: u64 = 10_000_000;
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_COMPONENT_BYTES: u64 = 16 * 1024 * 1024;

/// Compiles a component file without instantiating it or granting capabilities.
///
/// Returns `Err` when the file exceeds the component size limit, cannot be
/// read, or is not a valid Wasmtime Component. Call from a blocking worker.
pub fn validate_component_file(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| AgentError::from_io("Read Wasmtime component metadata", error))?;
    if metadata.len() > MAX_COMPONENT_BYTES {
        return Err(AgentError::new(
            code::PLUGIN_RESOURCE_LIMIT,
            "Component exceeds its byte limit",
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| AgentError::from_io("Read Wasmtime component", error))?;
    compile_component(&bytes).map(|_| ())
}

fn compile_component(bytes: &[u8]) -> Result<(Engine, Component)> {
    if bytes.len() as u64 > MAX_COMPONENT_BYTES {
        return Err(AgentError::new(
            code::PLUGIN_RESOURCE_LIMIT,
            "Component exceeds its byte limit",
        ));
    }
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .wasm_component_model_async(true)
        .wasm_component_model_async_stackful(true)
        .async_support(true)
        .consume_fuel(true);
    let engine = Engine::new(&config).map_err(load_error)?;
    let component = Component::from_binary(&engine, bytes).map_err(load_error)?;
    Ok((engine, component))
}

/// Raw companion configuration supplied to a Wasm provider.
///
/// The host preserves the original file contents. The provider parses
/// `hooks.json` and `.mcp.json` and owns their matching, protocol, and
/// transport semantics.
#[derive(Debug, Clone, Default)]
pub struct ProviderInputs {
    pub hooks_json: Option<String>,
    pub mcp_json: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderConfig<'a> {
    manifest: &'a WasmManifest,
    plugin_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hooks_json: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_json: Option<&'a str>,
}

struct StoreState {
    limits: StoreLimits,
    capabilities: CapabilityHub,
    cancel: CancellationToken,
}

impl bindings::deluxe::harness::host::Host for StoreState {}

struct HostState;

impl HasData for HostState {
    type Data<'a> = &'a mut StoreState;
}

impl bindings::deluxe::harness::host::HostWithStore for HostState {
    fn list_tools<T>(accessor: &Accessor<T, Self>) -> impl Future<Output = String> + Send {
        let capabilities = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            state.capabilities.clone()
        });
        async move {
            capabilities
                .list_tools()
                .unwrap_or_else(|error| format!(r#"{{"error":"{}"}}"#, error.message))
        }
    }

    fn invoke_tool<T>(
        accessor: &Accessor<T, Self>,
        name: String,
        arguments_json: String,
    ) -> impl Future<Output = std::result::Result<String, String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            capabilities
                .invoke(&name, &arguments_json, &cancel)
                .await
                .map_err(|error| format!("{}: {}", error.code, error.message))
        }
    }

    fn spawn_process<T>(
        accessor: &Accessor<T, Self>,
        command: String,
        arguments_json: String,
        cwd: String,
        environment_json: String,
    ) -> impl Future<Output = std::result::Result<u64, String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("capability call cancelled".into()),
                result = capabilities.spawn_process(
                    &command,
                    &arguments_json,
                    &cwd,
                    &environment_json,
                ) => result.map_err(|error| format!("{}: {}", error.code, error.message)),
            }
        }
    }

    fn process_write<T>(
        accessor: &Accessor<T, Self>,
        handle: u64,
        data: Vec<u8>,
    ) -> impl Future<Output = std::result::Result<(), String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("capability call cancelled".into()),
                result = capabilities.process_write(handle, &data) => {
                    result.map_err(|error| format!("{}: {}", error.code, error.message))
                }
            }
        }
    }

    fn process_read<T>(
        accessor: &Accessor<T, Self>,
        handle: u64,
        max_bytes: u32,
    ) -> impl Future<Output = std::result::Result<Vec<u8>, String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("capability call cancelled".into()),
                result = capabilities.process_read(handle, max_bytes) => {
                    result.map_err(|error| format!("{}: {}", error.code, error.message))
                }
            }
        }
    }

    fn process_close<T>(
        accessor: &Accessor<T, Self>,
        handle: u64,
    ) -> impl Future<Output = ()> + Send {
        let capabilities = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            state.capabilities.clone()
        });
        async move {
            capabilities.process_close(handle).await;
        }
    }

    fn http_request<T>(
        accessor: &Accessor<T, Self>,
        method: String,
        url: String,
        headers_json: String,
        body: String,
    ) -> impl Future<Output = std::result::Result<String, String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("capability call cancelled".into()),
                result = capabilities.http_request(&method, &url, &headers_json, &body) => {
                    result.map_err(|error| format!("{}: {}", error.code, error.message))
                }
            }
        }
    }

    fn http_read<T>(
        accessor: &Accessor<T, Self>,
        handle: u64,
        max_bytes: u32,
    ) -> impl Future<Output = std::result::Result<Vec<u8>, String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("capability call cancelled".into()),
                result = capabilities.http_read(handle, max_bytes) => {
                    result.map_err(|error| format!("{}: {}", error.code, error.message))
                }
            }
        }
    }

    fn http_close<T>(accessor: &Accessor<T, Self>, handle: u64) -> impl Future<Output = ()> + Send {
        let capabilities = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            state.capabilities.clone()
        });
        async move {
            capabilities.http_close(handle).await;
        }
    }
}

pub enum Operation {
    ListTools,
    Execute {
        name: String,
        arguments: String,
    },
    ListHooks,
    InvokeHook {
        id: String,
        event: String,
    },
    ListMcpServers,
    ListMcpTools {
        server: String,
    },
    InvokeMcp {
        server: String,
        name: String,
        arguments: String,
    },
    Open(String),
    Action(String),
    Close(String),
}

struct Request {
    operation: Operation,
    reply: oneshot::Sender<Result<String>>,
}

pub struct ComponentActor {
    tx: mpsc::Sender<Request>,
    lifetime: CancellationToken,
}

impl Drop for ComponentActor {
    fn drop(&mut self) {
        self.lifetime.cancel();
    }
}

impl ComponentActor {
    /// Compiles and starts an isolated component actor for one project.
    ///
    /// Returns `Err` for escaped entries, oversized binaries, compilation or ABI errors.
    /// No WASI imports or deserialized native code are accepted.
    pub async fn load(
        root: std::path::PathBuf,
        manifest: WasmManifest,
        inputs: ProviderInputs,
        capabilities: CapabilityHub,
    ) -> Result<Arc<Self>> {
        let entry = manifest.resolve_entry(&root)?;
        let bytes = tokio::fs::read(&entry)
            .await
            .map_err(|error| AgentError::from_io("Read component", error))?;
        Self::load_bytes(&bytes, root, manifest, inputs, capabilities).await
    }

    /// Compiles and starts an embedded component, used for trusted bundled
    /// providers that are shipped inside the host executable.
    pub async fn load_bytes(
        bytes: &[u8],
        plugin_root: PathBuf,
        manifest: WasmManifest,
        inputs: ProviderInputs,
        capabilities: CapabilityHub,
    ) -> Result<Arc<Self>> {
        let bytes = bytes.to_vec();
        let config = ProviderConfig {
            manifest: &manifest,
            plugin_root: plugin_root.to_string_lossy().into_owned(),
            hooks_json: inputs.hooks_json.as_deref(),
            mcp_json: inputs.mcp_json.as_deref(),
        };
        let config_json = serde_json::to_string(&config).map_err(|error| {
            AgentError::internal(format!("Encode plugin configuration: {error}"))
        })?;
        let (engine, component) = tokio::task::spawn_blocking(move || compile_component(&bytes))
            .await
            .map_err(|error| AgentError::internal(format!("Component loader failed: {error}")))??;
        let mut instance = tokio::time::timeout(
            CALL_TIMEOUT,
            instantiate(
                &engine,
                &component,
                capabilities.clone(),
                config_json.clone(),
            ),
        )
        .await
        .map_err(|_| timeout_error())??;
        let (tx, mut rx) = mpsc::channel::<Request>(32);
        let lifetime = CancellationToken::new();
        let actor = Arc::new(Self {
            tx,
            lifetime: lifetime.clone(),
        });
        tokio::spawn(async move {
            while let Some(mut request) = tokio::select! {
                biased;
                _ = lifetime.cancelled() => None,
                request = rx.recv() => request,
            } {
                if request.reply.is_closed() {
                    continue;
                }
                let cancel = CancellationToken::new();
                instance.0.data_mut().cancel = cancel.clone();
                let result = tokio::select! {
                    biased;
                    _ = lifetime.cancelled() => Err(cancelled_error()),
                    _ = request.reply.closed() => Err(cancelled_error()),
                    result = tokio::time::timeout(CALL_TIMEOUT, call(&mut instance, request.operation)) => {
                        result.unwrap_or_else(|_| Err(timeout_error()))
                    }
                };
                cancel.cancel();
                let failed = result.is_err();
                let _ = request.reply.send(result);
                if lifetime.is_cancelled() {
                    break;
                }
                if failed {
                    capabilities.shutdown().await;
                    match tokio::time::timeout(
                        CALL_TIMEOUT,
                        instantiate(
                            &engine,
                            &component,
                            capabilities.clone(),
                            config_json.clone(),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(fresh)) => instance = fresh,
                        _ => break,
                    }
                }
            }
            capabilities.shutdown().await;
        });
        Ok(actor)
    }

    /// Enqueues a bounded call. Dropping this future cancels its actor request.
    ///
    /// Returns `Err` for shutdown, queue saturation, timeout, trap, or invalid output.
    pub async fn call(&self, operation: Operation) -> Result<String> {
        let (reply, response) = oneshot::channel();
        self.tx
            .try_send(Request { operation, reply })
            .map_err(|_| {
                AgentError::new(
                    code::PLUGIN_RESOURCE_LIMIT,
                    "Component queue is unavailable or full",
                )
            })?;
        tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => Err(cancelled_error()),
            result = tokio::time::timeout(CALL_TIMEOUT + Duration::from_secs(1), response) => {
                result.map_err(|_| timeout_error())?
                    .map_err(|_| cancelled_error())?
            }
        }
    }

    /// Cancels current work and drops the isolated Store without waiting on the GUI.
    pub fn shutdown(&self) {
        self.lifetime.cancel();
    }
}

type Instance = (
    Store<StoreState>,
    bindings::HarnessPlugin,
    ComponentInstance,
);

async fn instantiate(
    engine: &Engine,
    component: &Component,
    capabilities: CapabilityHub,
    config_json: String,
) -> Result<Instance> {
    let mut linker = Linker::new(engine);
    bindings::HarnessPlugin::add_to_linker::<_, HostState>(&mut linker, |state| state)
        .map_err(load_error)?;
    let limits = StoreLimitsBuilder::new()
        .memory_size(32 * 1024 * 1024)
        .table_elements(10_000)
        .instances(32)
        .memories(4)
        .tables(4)
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(
        engine,
        StoreState {
            limits,
            capabilities,
            cancel: CancellationToken::new(),
        },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(CALL_FUEL).map_err(runtime_error)?;
    store
        .fuel_async_yield_interval(Some(50_000))
        .map_err(runtime_error)?;
    let instance = linker
        .instantiate_async(&mut store, component)
        .await
        .map_err(load_error)?;
    let bindings = bindings::HarnessPlugin::new(&mut store, &instance).map_err(load_error)?;
    let mut instance = (store, bindings, instance);
    configure(&mut instance, config_json).await?;
    Ok(instance)
}

async fn configure(instance: &mut Instance, config_json: String) -> Result<()> {
    let (store, bindings, component_instance) = instance;
    store.set_fuel(CALL_FUEL).map_err(runtime_error)?;
    let plugin = bindings.deluxe_harness_plugin();
    component_instance
        .run_concurrent(&mut *store, async move |accessor| {
            plugin.call_configure(accessor, config_json).await
        })
        .await
        .map_err(runtime_error)?
        .map_err(runtime_error)?
        .map_err(guest_error)
}

async fn call((store, bindings, instance): &mut Instance, operation: Operation) -> Result<String> {
    store.set_fuel(CALL_FUEL).map_err(runtime_error)?;
    let plugin = bindings.deluxe_harness_plugin();
    let output = instance
        .run_concurrent(&mut *store, async move |accessor| match operation {
            Operation::ListTools => plugin
                .call_list_tools(accessor)
                .await
                .map(Ok::<String, String>),
            Operation::Execute { name, arguments } => {
                plugin.call_execute_tool(accessor, name, arguments).await
            }
            Operation::ListHooks => plugin
                .call_list_hooks(accessor)
                .await
                .map(Ok::<String, String>),
            Operation::InvokeHook { id, event } => {
                plugin.call_invoke_hook(accessor, id, event).await
            }
            Operation::ListMcpServers => plugin
                .call_list_mcp_servers(accessor)
                .await
                .map(Ok::<String, String>),
            Operation::ListMcpTools { server } => plugin
                .call_list_mcp_tools(accessor, server)
                .await
                .map(Ok::<String, String>),
            Operation::InvokeMcp {
                server,
                name,
                arguments,
            } => {
                plugin
                    .call_invoke_mcp_tool(accessor, server, name, arguments)
                    .await
            }
            Operation::Open(request) => plugin.call_open_surface(accessor, request).await,
            Operation::Action(action) => plugin.call_handle_action(accessor, action).await,
            Operation::Close(surface) => plugin
                .call_close_surface(accessor, surface)
                .await
                .map(|_| Ok::<String, String>(String::new())),
        })
        .await
        .map_err(runtime_error)?
        .map_err(runtime_error)?
        .map_err(guest_error)?;
    if output.len() > MAX_PAYLOAD_BYTES {
        return Err(AgentError::new(
            code::PLUGIN_INVALID_OUTPUT,
            "Component response exceeds its payload limit",
        ));
    }
    Ok(output)
}

fn load_error(error: wasmtime::Error) -> AgentError {
    tracing::warn!(?error, "component load failed");
    AgentError::new(
        code::PLUGIN_LOAD_FAILED,
        "Component could not be loaded or does not implement the harness ABI",
    )
}

fn runtime_error(error: wasmtime::Error) -> AgentError {
    tracing::warn!(%error, "component call failed");
    let code = if matches!(error.downcast_ref::<Trap>(), Some(Trap::OutOfFuel))
        || error.to_string().contains("memory")
        || error.to_string().contains("table")
    {
        code::PLUGIN_RESOURCE_LIMIT
    } else {
        code::PLUGIN_TRAP
    };
    AgentError::new(
        code,
        "Component execution trapped or exceeded its resource budget",
    )
}

fn guest_error(message: String) -> AgentError {
    AgentError::new(code::PLUGIN_INVALID_OUTPUT, super::cap_chars(&message, 512))
}

fn timeout_error() -> AgentError {
    AgentError::new(
        code::PLUGIN_TIMEOUT,
        "Component exceeded its host time limit",
    )
}

fn cancelled_error() -> AgentError {
    AgentError::new(code::PLUGIN_CANCELLED, "Component call was cancelled")
}
