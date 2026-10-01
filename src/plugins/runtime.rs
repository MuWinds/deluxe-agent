//! Surface lifecycle and validation, independent of GUI and component engines.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::{code, AgentError, Result};

use super::ui_protocol::{
    validate_action, validate_document, PluginUiAction, PluginUiDocument, SurfaceRequest,
};
use super::wasm_runtime::CALL_TIMEOUT;

#[async_trait::async_trait]
pub trait PluginUiExecutor: Send {
    /// Opens one surface, returning a snapshot or a runtime error.
    async fn open_surface(&mut self, request: &SurfaceRequest) -> Result<PluginUiDocument>;
    /// Applies a validated action, returning a newer snapshot or a runtime error.
    async fn handle_action(&mut self, action: &PluginUiAction) -> Result<PluginUiDocument>;
    /// Releases surface resources. Returns `Err` if guest cleanup fails.
    async fn close_surface(&mut self, request: &SurfaceRequest) -> Result<()>;
}

#[derive(Debug, Clone)]
pub enum PluginUiEvent {
    Updated {
        request: SurfaceRequest,
        document: Arc<PluginUiDocument>,
    },
    Failed {
        request: SurfaceRequest,
        message: String,
    },
    Closed {
        request: SurfaceRequest,
    },
}

pub struct SurfaceHandle {
    tx: mpsc::Sender<PluginUiAction>,
    cancel: CancellationToken,
}

impl Drop for SurfaceHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl SurfaceHandle {
    /// Queues an action without waiting. Returns `Err` for a full or closed surface.
    pub fn action(&self, action: PluginUiAction) -> Result<()> {
        self.tx.try_send(action).map_err(|_| {
            AgentError::new(
                code::PLUGIN_RESOURCE_LIMIT,
                "Surface is closed or its queue is full",
            )
        })
    }
}

/// Starts a surface on the worker; its handle cancels work when closed or reloaded.
///
/// `load` must resolve an enabled, project-scoped executor. All responses are
/// validated before `emit` is called, and failures never enter session transcripts.
pub fn spawn_surface<F>(
    request: SurfaceRequest,
    actions: Vec<String>,
    load: F,
    emit: Arc<dyn Fn(PluginUiEvent) + Send + Sync>,
) -> SurfaceHandle
where
    F: std::future::Future<Output = Result<Box<dyn PluginUiExecutor>>> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<PluginUiAction>(8);
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        let result = async {
            let mut executor = tokio::select! {
                biased;
                _ = token.cancelled() => return Ok(()),
                loaded = load => loaded?,
            };
            let mut opened = false;
            let mut result = async {
                let open_result = tokio::select! {
                    biased;
                    _ = token.cancelled() => None,
                    result = tokio::time::timeout(CALL_TIMEOUT, executor.open_surface(&request)) => {
                        Some(result)
                    },
                };
                let Some(open_result) = open_result else {
                    return Ok(());
                };
                let mut document = open_result.map_err(|_| timeout_error())??;
                opened = true;
                validate_document(&document, &request, &actions, None)?;
                emit(PluginUiEvent::Updated {
                    request: request.clone(),
                    document: Arc::new(document.clone()),
                });
                loop {
                    let action = tokio::select! {
                        biased;
                        _ = token.cancelled() => break,
                        action = rx.recv() => match action { Some(action) => action, None => break },
                    };
                    if action.surface != request {
                        continue;
                    }
                    if let Err(error) = validate_action(&document, &action) {
                        emit(PluginUiEvent::Failed {
                            request: request.clone(),
                            message: error.to_string(),
                        });
                        continue;
                    }
                    let updated = tokio::select! {
                        biased;
                        _ = token.cancelled() => break,
                        result = tokio::time::timeout(
                            CALL_TIMEOUT,
                            executor.handle_action(&action),
                        ) => result,
                    }
                    .map_err(|_| timeout_error())??;
                    validate_document(&updated, &request, &actions, Some(document.revision))?;
                    document = updated;
                    emit(PluginUiEvent::Updated {
                        request: request.clone(),
                        document: Arc::new(document.clone()),
                    });
                }
                Ok::<_, AgentError>(())
            }
            .await;
            if opened {
                let close_result = tokio::time::timeout(
                    CALL_TIMEOUT,
                    executor.close_surface(&request),
                )
                .await
                .map_err(|_| timeout_error())
                .and_then(|result| result);
                if result.is_ok() {
                    result = close_result;
                } else if let Err(error) = close_result {
                    tracing::warn!(%error, "plugin surface cleanup failed");
                }
            }
            result
        }
        .await;
        if !token.is_cancelled() {
            if let Err(error) = result {
                emit(PluginUiEvent::Failed {
                    request: request.clone(),
                    message: error.to_string(),
                });
            }
        }
        emit(PluginUiEvent::Closed { request });
    });
    SurfaceHandle { tx, cancel }
}

fn timeout_error() -> AgentError {
    AgentError::new(
        code::PLUGIN_TIMEOUT,
        "Plugin UI call exceeded its host time limit",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    use std::time::Duration;

    use crate::plugins::ui_protocol::{UiNode, UI_SCHEMA_VERSION};

    #[derive(Default)]
    struct FakeState {
        opens: usize,
        actions: usize,
        closes: usize,
        action_error: bool,
        disabled: bool,
    }

    struct FakeExecutor {
        state: Arc<Mutex<FakeState>>,
        request: SurfaceRequest,
    }

    #[async_trait::async_trait]
    impl PluginUiExecutor for FakeExecutor {
        async fn open_surface(&mut self, _request: &SurfaceRequest) -> Result<PluginUiDocument> {
            let mut state = self.state.lock().expect("fake state is not poisoned");
            state.opens += 1;
            Ok(document(&self.request, 1, !state.disabled))
        }

        async fn handle_action(&mut self, _action: &PluginUiAction) -> Result<PluginUiDocument> {
            let mut state = self.state.lock().expect("fake state is not poisoned");
            state.actions += 1;
            if state.action_error {
                return Err(AgentError::new(code::PLUGIN_TRAP, "fake action trap"));
            }
            Ok(document(&self.request, 2, !state.disabled))
        }

        async fn close_surface(&mut self, _request: &SurfaceRequest) -> Result<()> {
            self.state
                .lock()
                .expect("fake state is not poisoned")
                .closes += 1;
            Ok(())
        }
    }

    fn request(surface_id: &str) -> SurfaceRequest {
        SurfaceRequest {
            plugin_id: "echo@test".into(),
            project: "/project".into(),
            surface_id: surface_id.into(),
            request_id: 1,
        }
    }

    fn document(request: &SurfaceRequest, revision: u64, enabled: bool) -> PluginUiDocument {
        PluginUiDocument {
            schema_version: UI_SCHEMA_VERSION,
            plugin_id: request.plugin_id.clone(),
            surface_id: request.surface_id.clone(),
            revision,
            title: "Echo".into(),
            root: UiNode::Button {
                id: "go".into(),
                label: "Go".into(),
                action: "go".into(),
                enabled,
            },
        }
    }

    fn action(request: &SurfaceRequest, revision: u64) -> PluginUiAction {
        PluginUiAction {
            surface: request.clone(),
            revision,
            control_id: "go".into(),
            action: "go".into(),
            value: None,
        }
    }

    async fn next_event(rx: &mut mpsc::UnboundedReceiver<PluginUiEvent>) -> PluginUiEvent {
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("surface event arrives")
            .expect("surface event channel stays open")
    }

    fn spawn_fake(
        request: SurfaceRequest,
        state: Arc<Mutex<FakeState>>,
    ) -> (SurfaceHandle, mpsc::UnboundedReceiver<PluginUiEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let load_request = request.clone();
        let load_state = state.clone();
        let emit = Arc::new(move |event| {
            let _ = event_tx.send(event);
        });
        let handle = spawn_surface(
            request,
            vec!["go".into()],
            async move {
                Ok(Box::new(FakeExecutor {
                    state: load_state,
                    request: load_request,
                }) as Box<dyn PluginUiExecutor>)
            },
            emit,
        );
        (handle, event_rx)
    }

    #[tokio::test]
    async fn opening_and_handling_an_action_emits_monotonic_snapshots() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let request = request("main");
        let (handle, mut events) = spawn_fake(request.clone(), state.clone());

        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Updated { document, .. } if document.revision == 1
        ));
        handle
            .action(action(&request, 1))
            .expect("valid action queues");
        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Updated { document, .. } if document.revision == 2
        ));
        assert_eq!(state.lock().expect("fake state is not poisoned").actions, 1);

        drop(handle);
        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Closed { .. }
        ));
        assert_eq!(state.lock().expect("fake state is not poisoned").closes, 1);
    }

    #[tokio::test]
    async fn stale_and_disabled_actions_are_rejected_before_guest_execution() {
        let state = Arc::new(Mutex::new(FakeState {
            disabled: true,
            ..Default::default()
        }));
        let request = request("disabled");
        let (handle, mut events) = spawn_fake(request.clone(), state.clone());

        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Updated { .. }
        ));
        handle
            .action(action(&request, 0))
            .expect("stale action queues");
        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Failed { .. }
        ));
        assert_eq!(state.lock().expect("fake state is not poisoned").actions, 0);
        drop(handle);
        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Closed { .. }
        ));
    }

    #[tokio::test]
    async fn actions_for_another_surface_are_ignored_and_close_is_called_on_drop() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let surface_request = request("main");
        let (handle, mut events) = spawn_fake(surface_request.clone(), state.clone());

        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Updated { .. }
        ));
        handle
            .action(action(&request("other"), 1))
            .expect("mismatched action queues");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(state.lock().expect("fake state is not poisoned").actions, 0);
        drop(handle);
        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Closed { .. }
        ));
        assert_eq!(state.lock().expect("fake state is not poisoned").closes, 1);
    }

    #[tokio::test]
    async fn executor_failure_emits_failed_then_closed_and_still_cleans_up() {
        let state = Arc::new(Mutex::new(FakeState {
            action_error: true,
            ..Default::default()
        }));
        let request = request("failing");
        let (handle, mut events) = spawn_fake(request.clone(), state.clone());

        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Updated { .. }
        ));
        handle
            .action(action(&request, 1))
            .expect("valid action queues");
        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Failed { .. }
        ));
        assert!(matches!(
            next_event(&mut events).await,
            PluginUiEvent::Closed { .. }
        ));
        assert_eq!(state.lock().expect("fake state is not poisoned").closes, 1);
        drop(handle);
    }
}
