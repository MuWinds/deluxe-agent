//! Component actors own their stores and service bounded calls on worker threads.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use wasmtime::component::{Accessor, Component, HasData, Instance as ComponentInstance, Linker};
use wasmtime::{Config, Engine, Store, Trap};

use crate::error::{code, AgentError, Result};

use super::capabilities::CapabilityHub;
use super::wasm_manifest::WasmManifest;

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "harness-plugin",
        imports: { default: async },
        exports: { default: async },
    });
}

// A second, optional view of the same Component: only the transcript renderer
// exports `renderer`, so a binding that fails here means "this Component does
// not render". Keeping it separate from the harness binding leaves every other
// plugin untouched.
mod renderer_bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "renderer-plugin",
        imports: { default: async },
        exports: { default: async },
    });
}

// A third optional view: only a prompt provider exports `prompt`, so a binding
// that fails here means "this Component contributes no prompt text".
mod prompt_bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "prompt-plugin",
        imports: { default: async },
        exports: { default: async },
    });
}

const CALL_FUEL: u64 = 10_000_000;

/// Compiles a component file without instantiating it or granting capabilities.
///
/// Returns `Err` when the file cannot be read or is not a valid Wasmtime
/// Component. Call from a blocking worker.
pub fn validate_component_file(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path)
        .map_err(|error| AgentError::from_io("Read Wasmtime component", error))?;
    compile_component(&bytes).map(|_| ())
}

/// Compiles embedded Component bytes without instantiating or granting them
/// capabilities.
pub fn validate_component_bytes(bytes: &[u8]) -> Result<()> {
    compile_component(bytes).map(|_| ())
}

fn compile_component(bytes: &[u8]) -> Result<(Engine, Component)> {
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

struct StoreState {
    capabilities: CapabilityHub,
    cancel: CancellationToken,
}

impl bindings::deluxe::harness::host::Host for StoreState {}

struct HostState;

impl HasData for HostState {
    type Data<'a> = &'a mut StoreState;
}

impl bindings::deluxe::harness::host::HostWithStore for HostState {
    fn read_plugin_file<T>(
        accessor: &Accessor<T, Self>,
        path: String,
    ) -> impl Future<Output = std::result::Result<Vec<u8>, String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("capability call cancelled".into()),
                result = capabilities.read_plugin_file(&path) => {
                    result.map_err(|error| format!("{}: {}", error.code, error.message))
                }
            }
        }
    }

    fn write_plugin_file<T>(
        accessor: &Accessor<T, Self>,
        path: String,
        contents: Vec<u8>,
    ) -> impl Future<Output = std::result::Result<(), String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("capability call cancelled".into()),
                result = capabilities.write_plugin_file(&path, &contents) => {
                    result.map_err(|error| format!("{}: {}", error.code, error.message))
                }
            }
        }
    }

    fn list_plugin_files<T>(
        accessor: &Accessor<T, Self>,
        path: String,
    ) -> impl Future<Output = std::result::Result<Vec<String>, String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("capability call cancelled".into()),
                result = capabilities.list_plugin_files(&path) => {
                    result.map_err(|error| format!("{}: {}", error.code, error.message))
                }
            }
        }
    }

    fn configuration_root<T>(accessor: &Accessor<T, Self>) -> impl Future<Output = String> + Send {
        let root = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            state
                .capabilities
                .configuration_root
                .to_string_lossy()
                .into_owned()
        });
        std::future::ready(root)
    }

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

    fn run_agent<T>(
        accessor: &Accessor<T, Self>,
        request_json: String,
    ) -> impl Future<Output = std::result::Result<String, String>> + Send {
        let (capabilities, cancel) = accessor.with(|mut access| {
            let state: &mut StoreState = access.get();
            (state.capabilities.clone(), state.cancel.clone())
        });
        async move {
            capabilities
                .run_agent(&request_json, &cancel)
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
    ListEventHandlers,
    HandleEvent {
        id: String,
        event: String,
    },
    Open(String),
    Action(String),
    Close(String),
    /// Renders a message request JSON into display-list response JSON. Only a
    /// Component that exports the renderer interface can serve it.
    RenderMessage(String),
    /// Renders a tool request JSON into display-list response JSON.
    RenderTool(String),
    /// Asks the Component for the prompt text it contributes for its bound
    /// scope. Only a Component that exports the prompt interface can serve it.
    PromptSections,
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
    /// Returns `Err` for escaped entries, compilation or ABI errors.
    /// No WASI imports or deserialized native code are accepted.
    pub async fn load(
        root: std::path::PathBuf,
        manifest: WasmManifest,
        capabilities: CapabilityHub,
    ) -> Result<Arc<Self>> {
        let entry = manifest.resolve_entry(&root)?;
        let bytes = tokio::fs::read(&entry)
            .await
            .map_err(|error| AgentError::from_io("Read component", error))?;
        Self::load_bytes(&bytes, capabilities).await
    }

    /// Compiles and starts an embedded component, used for trusted bundled
    /// plugins that are shipped inside the host executable.
    pub async fn load_bytes(bytes: &[u8], capabilities: CapabilityHub) -> Result<Arc<Self>> {
        let bytes = bytes.to_vec();
        let (engine, component) = tokio::task::spawn_blocking(move || compile_component(&bytes))
            .await
            .map_err(|error| AgentError::internal(format!("Component loader failed: {error}")))??;
        let mut instance = instantiate(&engine, &component, capabilities.clone()).await?;
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
                // Fuel is the only budget a call has: a runaway guest burns it
                // and traps, and a caller that goes away closes the reply.
                let result = tokio::select! {
                    biased;
                    _ = lifetime.cancelled() => Err(cancelled_error()),
                    _ = request.reply.closed() => Err(cancelled_error()),
                    result = call(&mut instance, request.operation) => result,
                };
                cancel.cancel();
                // A guest that returns an error result is normal control flow and
                // leaves the Store usable. Only a trap or a cancelled call can
                // leave it poisoned. Rebuilding on a guest error would silently
                // discard the plugin's own memory state — for example the MCP
                // provider's stopped servers.
                let recover = result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.code != code::PLUGIN_INVALID_OUTPUT);
                let _ = request.reply.send(result);
                if lifetime.is_cancelled() {
                    break;
                }
                if recover {
                    capabilities.shutdown().await;
                    match instantiate(&engine, &component, capabilities.clone()).await {
                        Ok(fresh) => instance = fresh,
                        _ => break,
                    }
                }
            }
            capabilities.shutdown().await;
        });
        Ok(actor)
    }

    /// Enqueues a bounded call, waiting for room in the actor's queue. Dropping
    /// this future cancels its actor request.
    ///
    /// The wait is bounded by the actor's lifetime, so a shutdown unblocks every
    /// waiter. Returns `Err` for shutdown, trap, or invalid output.
    pub async fn call(&self, operation: Operation) -> Result<String> {
        let (reply, response) = oneshot::channel();
        // Wait for room instead of failing when the queue is full. The GUI
        // dispatches one render request per transcript item in a single frame,
        // so a non-blocking send would drop everything past the queue capacity
        // and the render cache would mark those items as permanently failed.
        tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => return Err(cancelled_error()),
            sent = self.tx.send(Request { operation, reply }) => {
                sent.map_err(|_| {
                    AgentError::new(
                        code::PLUGIN_RESOURCE_LIMIT,
                        "Component queue is unavailable",
                    )
                })?;
            }
        }
        tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => Err(cancelled_error()),
            result = response => result.map_err(|_| cancelled_error())?,
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
    // Present only when the Component exports the renderer interface.
    Option<renderer_bindings::RendererPlugin>,
    // Present only when the Component exports the prompt interface.
    Option<prompt_bindings::PromptPlugin>,
    ComponentInstance,
);

async fn instantiate(
    engine: &Engine,
    component: &Component,
    capabilities: CapabilityHub,
) -> Result<Instance> {
    let mut linker = Linker::new(engine);
    bindings::HarnessPlugin::add_to_linker::<_, HostState>(&mut linker, |state| state)
        .map_err(load_error)?;
    // Fuel is the only resource limit a Component gets: no memory, table, or
    // instance caps, and no wall-clock deadline.
    let mut store = Store::new(
        engine,
        StoreState {
            capabilities,
            cancel: CancellationToken::new(),
        },
    );
    store.set_fuel(CALL_FUEL).map_err(runtime_error)?;
    store
        .fuel_async_yield_interval(Some(50_000))
        .map_err(runtime_error)?;
    let instance = linker
        .instantiate_async(&mut store, component)
        .await
        .map_err(load_error)?;
    let bindings = bindings::HarnessPlugin::new(&mut store, &instance).map_err(load_error)?;
    // A Component that also exports the renderer interface gets a second,
    // optional binding. A failure here is not an error: it only means this
    // Component is not a renderer.
    let renderer = renderer_bindings::RendererPlugin::new(&mut store, &instance).ok();
    // Same optional shape for the prompt provider: a failure only means this
    // Component contributes no prompt text.
    let prompt = prompt_bindings::PromptPlugin::new(&mut store, &instance).ok();
    let mut instance = (store, bindings, renderer, prompt, instance);
    configure(&mut instance).await?;
    Ok(instance)
}

async fn configure(instance: &mut Instance) -> Result<()> {
    let (store, bindings, _renderer, _prompt, component_instance) = instance;
    store.set_fuel(CALL_FUEL).map_err(runtime_error)?;
    let plugin = bindings.deluxe_harness_plugin();
    component_instance
        .run_concurrent(&mut *store, async move |accessor| {
            plugin.call_configure(accessor).await
        })
        .await
        .map_err(runtime_error)?
        .map_err(runtime_error)?
        .map_err(guest_error)
}

async fn call(
    (store, bindings, renderer, prompt, instance): &mut Instance,
    operation: Operation,
) -> Result<String> {
    if renderer.is_none()
        && matches!(
            operation,
            Operation::RenderMessage(_) | Operation::RenderTool(_)
        )
    {
        return Err(AgentError::new(
            code::PLUGIN_LOAD_FAILED,
            "Component does not implement the renderer ABI",
        ));
    }
    if prompt.is_none() && matches!(operation, Operation::PromptSections) {
        return Err(AgentError::new(
            code::PLUGIN_LOAD_FAILED,
            "Component does not implement the prompt ABI",
        ));
    }
    store.set_fuel(CALL_FUEL).map_err(runtime_error)?;
    let plugin = bindings.deluxe_harness_plugin();
    let renderer = renderer
        .as_ref()
        .map(|bindings| bindings.deluxe_harness_renderer());
    let prompt = prompt
        .as_ref()
        .map(|bindings| bindings.deluxe_harness_prompt());
    let output = instance
        .run_concurrent(&mut *store, async move |accessor| match operation {
            Operation::ListTools => plugin
                .call_list_tools(accessor)
                .await
                .map(Ok::<String, String>),
            Operation::Execute { name, arguments } => {
                plugin.call_execute_tool(accessor, name, arguments).await
            }
            Operation::ListEventHandlers => plugin
                .call_list_event_handlers(accessor)
                .await
                .map(Ok::<String, String>),
            Operation::HandleEvent { id, event } => {
                plugin.call_handle_event(accessor, id, event).await
            }
            Operation::Open(request) => plugin.call_open_surface(accessor, request).await,
            Operation::Action(action) => plugin.call_handle_action(accessor, action).await,
            Operation::Close(surface) => plugin
                .call_close_surface(accessor, surface)
                .await
                .map(|_| Ok::<String, String>(String::new())),
            Operation::RenderMessage(request) => match renderer {
                Some(renderer) => renderer.call_render_message(accessor, request).await,
                None => Ok(Err("Component does not implement the renderer ABI".into())),
            },
            Operation::RenderTool(request) => match renderer {
                Some(renderer) => renderer.call_render_tool(accessor, request).await,
                None => Ok(Err("Component does not implement the renderer ABI".into())),
            },
            Operation::PromptSections => match prompt {
                Some(prompt) => prompt.call_prompt_sections(accessor).await,
                None => Ok(Err("Component does not implement the prompt ABI".into())),
            },
        })
        .await
        .map_err(runtime_error)?
        .map_err(runtime_error)?
        .map_err(guest_error)?;
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
    let code = if matches!(error.downcast_ref::<Trap>(), Some(Trap::OutOfFuel)) {
        code::PLUGIN_RESOURCE_LIMIT
    } else {
        code::PLUGIN_TRAP
    };
    AgentError::new(code, "Component execution trapped or ran out of fuel")
}

fn guest_error(message: String) -> AgentError {
    AgentError::new(code::PLUGIN_INVALID_OUTPUT, super::cap_chars(&message, 512))
}

fn cancelled_error() -> AgentError {
    AgentError::new(code::PLUGIN_CANCELLED, "Component call was cancelled")
}
