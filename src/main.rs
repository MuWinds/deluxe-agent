//! Threading model, which the rest of the code assumes:
//!
//! * the egui window owns the main thread and never blocks;
//! * a multi-threaded tokio runtime lives in a worker thread and does all I/O;
//! * the two talk over two channels — `Cmd` down, `Event` up — and the agent
//!   wakes the window with `Context::request_repaint`.
//!
//! The runtime is parked inside [`AgentFrame`] rather than a local, because
//! dropping it at the end of `main` would stop the agent the moment the window
//! opens.

// Release builds are GUI programs: no console window on Windows. Debug builds
// keep the console so `tracing` output is visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use agent::RunRequest;
use app::{App, EventSink, GuiResources, Paths, RepaintSignal};
use harness::{
    AgentEvent, AgentEventSink, ConfigStore, NativeConfigStore, NativePluginManager,
    NativeSecretStore, PluginManager, SecretStore, SessionStore,
};
use ipc::{Cmd, Event, JobView, LlmSettings, RunId};
use plugins::runtime::{PluginUiEvent, PluginUiExecutor, SurfaceHandle};
use plugins::ui_protocol::SurfaceRequest;
use plugins::PluginCatalogue;
use runtime::project::{ProjectRuntime, ProjectRuntimeFactory, RuntimeModelSettings};
use tools::ToolSettings;

mod agent;
mod app;
mod attachments;
mod code_view;
mod config;
mod context;
mod error;
mod fonts;
mod harness;
mod icons;
mod image_ops;
mod ipc;
mod llm;
mod markdown;
mod plugins;
mod process;
mod runtime;
mod runtime_context;
mod session;
mod theme;
mod tools;

#[cfg(test)]
mod agent_loop_tests;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // A multi-threaded runtime because tool calls are concurrent: a slow shell
    // command must not block a filesystem read.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start the async runtime: {error}");
            return Ok(());
        }
    };

    let mut config = config::load();
    // Resolved before the window exists and handed to it, so a save writes the
    // same file the load read rather than re-deriving it from the environment.
    let config_path = config::config_path();
    let api_key = config::resolve_api_key().unwrap_or_default();
    // Read before the window exists, so the sidebar shows the previous run's
    // sessions on the very first frame rather than flashing empty.
    let session_store: Arc<dyn SessionStore> =
        Arc::new(session::JsonSessionStore::new(session::store_path()));
    let sessions = match runtime.block_on(session_store.load()) {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::warn!(%error, "failed to load the session store");
            Vec::new()
        }
    };

    // Plugins are discovered before the first frame so the first agent and the
    // first page have a catalogue immediately. The page can request another
    // discovery later when Codex installs or updates a component.
    let home = directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let plugin_manager: Arc<dyn PluginManager> = Arc::new(NativePluginManager::new(home.clone()));
    let config_store: Arc<dyn ConfigStore> = Arc::new(NativeConfigStore::new(config_path.clone()));
    let previous_plugins = config.plugins.clone();
    match runtime.block_on(plugin_manager.ensure_bundled_defaults(config.plugins.clone())) {
        Ok(plugins) => {
            config.plugins = plugins;
            if config.plugins != previous_plugins {
                if let Err(error) = runtime.block_on(config_store.save(&config)) {
                    tracing::warn!(%error, "failed to persist bundled plugin defaults");
                }
            }
        }
        Err(error) => tracing::warn!(%error, "failed to install bundled plugin defaults"),
    }
    let projects: Vec<PathBuf> = config.projects.iter().map(PathBuf::from).collect();
    let plugins = match runtime.block_on(plugin_manager.discover(projects, config.plugins.clone()))
    {
        Ok(plugins) => plugins,
        Err(error) => {
            tracing::warn!(%error, "failed to discover plugins");
            Arc::new(PluginCatalogue::default())
        }
    };
    let secret_store: Arc<dyn SecretStore> = Arc::new(NativeSecretStore::new());

    let settings = Arc::new(RwLock::new(config.tools.clone()));

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

    let worker = Worker {
        settings,
        plugins,
        home: home.clone(),
        session_store,
        config_store,
        secret_store,
        plugin_manager,
    };

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("Deluxe Agent"),
        ..Default::default()
    };

    let handle = runtime.handle().clone();

    eframe::run_native(
        "Deluxe Agent",
        options,
        Box::new(move |cc| {
            // Must run before the first frame: the default font set has no CJK
            // coverage, so without this every Chinese label renders as a box.
            fonts::install(&cc.egui_ctx);
            // The palette is a config value, so the first frame must already be
            // drawn with it; `draw_menu_bar` only re-applies it on a change.
            theme::apply(&cc.egui_ctx, config.theme);

            // Cloned before the worker takes ownership of it: the window needs
            // the catalogue too, to resolve the slash commands of whichever
            // conversation is open.
            let catalogue = worker.plugins.clone();

            let (sink, repaint_rx) = EventSink::new(event_tx.clone());
            RepaintSignal::spawn(cc.egui_ctx.clone(), repaint_rx);
            spawn_worker(&handle, worker, cmd_rx, sink);

            // The model settings are global worker state, so they are pushed
            // once here; every session and every run uses whatever is current.
            let _ = cmd_tx.send(Cmd::SetLlmSettings(Box::new(LlmSettings {
                base_url: config.llm.base_url.clone(),
                model: config.llm.model.clone(),
                context: config.context,
                max_output_tokens: config.llm.max_output_tokens,
                retry_count: config.llm.retry_count,
                retry_forever: config.llm.retry_forever,
                input: config.llm.input.clone(),
                api_key: api_key.clone(),
            })));

            Ok(Box::new(AgentFrame {
                app: App::new(
                    cmd_tx,
                    event_rx,
                    config,
                    api_key,
                    sessions,
                    catalogue,
                    Paths { config_path },
                ),
                resources: GuiResources::default(),
                runtime: Some(runtime),
            }))
        }),
    )
}

/// The pieces the worker needs, resolved before the window exists so the first
/// frame never shows a half-initialised state.
struct Worker {
    settings: Arc<RwLock<ToolSettings>>,
    home: PathBuf,
    /// Every plugin that loaded, in both scopes. The worker resolves the ones
    /// that apply to a run's project rather than receiving a flat list, because
    /// a project-scoped plugin must not reach another project's agent.
    plugins: Arc<PluginCatalogue>,
    session_store: Arc<dyn SessionStore>,
    config_store: Arc<dyn ConfigStore>,
    secret_store: Arc<dyn SecretStore>,
    plugin_manager: Arc<dyn PluginManager>,
}

fn spawn_worker(
    handle: &tokio::runtime::Handle,
    mut worker: Worker,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    sink: EventSink,
) {
    handle.spawn(async move {
        // Every run in flight, so a `Cancel` can reach whichever one it names.
        // Shared with the run tasks, which retire their own entry when they
        // finish — otherwise a long-lived window would pile up one dead token
        // per run. The critical sections are a get, an insert and a remove, so
        // a poisoned lock is recovered from rather than propagated.
        let active: Arc<Mutex<HashMap<RunId, CancellationToken>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // One agent per project. A single slot would be evicted by every run in
        // another project, rebuilding the HTTP client's connection pool each
        // time; keyed by project, concurrent runs each keep their own. The
        // project is the key because it shapes the agent's system prompt (its
        // context files are read from there) and every tool's working
        // directory.
        let mut runtimes: HashMap<PathBuf, Arc<ProjectRuntime>> = HashMap::new();
        let mut surfaces: HashMap<SurfaceRequest, SurfaceHandle> = HashMap::new();
        let factory = ProjectRuntimeFactory::new(
            worker.settings.clone(),
            Arc::new(sink.clone()),
            // A global Component's generic file capability is bound here, not to
            // the whole home directory: `~/.agents` holds plugin configuration
            // (`.mcp.json`, `.hooks.json`, the personal marketplace) and nothing
            // a plugin has no business reading.
            plugins::global_configuration_root(&worker.home),
        );
        // The model settings the GUI wants. Pushed before the first run and on
        // every settings save; `None` only until then.
        let mut llm_settings: Option<LlmSettings> = None;

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Cmd::OpenPluginSurface(request) => {
                    let declaration = worker
                        .plugins
                        .for_project(&request.project)
                        .into_iter()
                        .find(|plugin| plugin.id == request.plugin_id)
                        .and_then(|plugin| {
                            plugin.manifest.wasm_runtime().map(|manifest| {
                                (plugin.root.clone(), plugin.scope.clone(), manifest)
                            })
                        });
                    let Some((root, scope, manifest)) = declaration.filter(|(_, _, manifest)| {
                        manifest.ui.surfaces.contains(&request.surface_id)
                    }) else {
                        sink.emit_ui(Event::PluginUiFailed {
                            request,
                            message: "Plugin surface is disabled, out of scope, or undeclared"
                                .into(),
                        });
                        continue;
                    };
                    if surfaces.len() >= 16 {
                        sink.emit_ui(Event::PluginUiFailed {
                            request,
                            message: "Too many open plugin surfaces".into(),
                        });
                        continue;
                    }
                    let cached = factory.component(&request.project, &request.plugin_id);
                    let host_tools: Arc<dyn harness::ports::ToolRuntime> = runtimes
                        .get(&request.project)
                        .map(|runtime| runtime.host_tools.clone())
                        .unwrap_or_else(|| {
                            factory
                                .host_capabilities(
                                    &request.project,
                                    llm_settings
                                        .as_ref()
                                        .is_some_and(LlmSettings::supports_images),
                                )
                                .runtime
                        });
                    let hub = match plugins::capabilities::CapabilityHub::new(
                        request.project.clone(),
                        factory.configuration_root(&scope, &request.project),
                        manifest.permissions.clone(),
                        host_tools,
                    ) {
                        Ok(hub) => hub,
                        Err(error) => {
                            sink.emit_ui(Event::PluginUiFailed {
                                request,
                                message: error.to_string(),
                            });
                            continue;
                        }
                    };
                    let actions = manifest.ui.actions.clone();
                    // A surface must act on the same actor the agent's tools use,
                    // so a newly loaded one is registered in the shared cache and
                    // a later runtime build reuses it instead of loading a second.
                    let load_project = request.project.clone();
                    let load_plugin = request.plugin_id.clone();
                    let load_factory = factory.clone();
                    let load = async move {
                        let actor = match cached {
                            Some(actor) => actor,
                            None => {
                                let actor = plugins::wasm_runtime::ComponentActor::load(
                                    root, manifest, hub,
                                )
                                .await?;
                                load_factory.cache_component(
                                    &load_project,
                                    &load_plugin,
                                    actor.clone(),
                                );
                                actor
                            }
                        };
                        Ok(Box::new(plugins::wasm::WasmUiExecutor::new(actor))
                            as Box<dyn PluginUiExecutor>)
                    };
                    let event_sink = sink.clone();
                    let emit = Arc::new(move |event| {
                        event_sink.emit_ui(match event {
                            PluginUiEvent::Updated { request, document } => {
                                Event::PluginUiUpdated { request, document }
                            }
                            PluginUiEvent::Failed { request, message } => {
                                Event::PluginUiFailed { request, message }
                            }
                            PluginUiEvent::Closed { request } => Event::PluginUiClosed { request },
                        })
                    });
                    let handle =
                        plugins::runtime::spawn_surface(request.clone(), actions, load, emit);
                    surfaces.insert(request, handle);
                }
                Cmd::PluginUiAction(action) => {
                    if let Some(surface) = surfaces.get(&action.surface) {
                        let request = action.surface.clone();
                        if let Err(error) = surface.action(action) {
                            sink.emit_ui(Event::PluginUiFailed {
                                request,
                                message: error.to_string(),
                            });
                        }
                    }
                }
                Cmd::ClosePluginSurface(request) => {
                    surfaces.remove(&request);
                }
                Cmd::InstallPlugin {
                    request_id,
                    component_path,
                    scope,
                    config,
                } => {
                    let result = async {
                        let installed = worker.plugin_manager.install_local(component_path).await?;
                        let mut config = *config;
                        match &scope {
                            plugins::Scope::Global => {
                                config.plugins.set_enabled(&installed.id, true);
                            }
                            plugins::Scope::Project(project) => {
                                config
                                    .plugins
                                    .set_project_enabled(project, &installed.id, true);
                            }
                        }
                        config.plugins.normalize();
                        let projects: Vec<PathBuf> =
                            config.projects.iter().map(PathBuf::from).collect();
                        let plugins = match worker
                            .plugin_manager
                            .discover(projects, config.plugins.clone())
                            .await
                        {
                            Ok(plugins) => plugins,
                            Err(error) => {
                                if installed.copied {
                                    let _ = worker
                                        .plugin_manager
                                        .discard_install(installed.root.clone())
                                        .await;
                                }
                                return Err(error);
                            }
                        };
                        if let Err(error) = worker.config_store.save(&config).await {
                            if installed.copied {
                                let _ = worker.plugin_manager.discard_install(installed.root).await;
                            }
                            return Err(error);
                        }
                        Ok::<_, crate::error::AgentError>((config, plugins))
                    }
                    .await;

                    match result {
                        Ok((config, plugins)) => {
                            surfaces.clear();
                            invalidate_runtimes(&factory, &mut runtimes, &active).await;
                            worker.plugins = plugins.clone();
                            sink.emit_ui(Event::PluginInstalled {
                                request_id,
                                config: Box::new(config),
                                catalogue: plugins,
                            });
                        }
                        Err(error) => sink.emit_ui(Event::PluginOperationFailed {
                            request_id,
                            message: error.to_string(),
                        }),
                    }
                }
                Cmd::RefreshPlugins {
                    request_id,
                    projects,
                    settings,
                } => {
                    let result = worker.plugin_manager.discover(projects, settings).await;
                    match result {
                        Ok(plugins) => {
                            surfaces.clear();
                            invalidate_runtimes(&factory, &mut runtimes, &active).await;
                            worker.plugins = plugins.clone();
                            sink.emit_ui(Event::PluginsUpdated {
                                request_id,
                                catalogue: plugins,
                            });
                        }
                        Err(error) => sink.emit_ui(Event::PluginOperationFailed {
                            request_id,
                            message: error.to_string(),
                        }),
                    }
                }
                Cmd::SetToolSettings(settings) => {
                    *worker.settings.write().await = *settings;
                }

                // Global model state. Applied before the first run arrives, and
                // again whenever the settings window saves.
                Cmd::SetLlmSettings(settings) => {
                    let settings = *settings;
                    // Every cached agent was built for the old model, its
                    // modalities and its context settings, so all of them are
                    // stale. A settings push that changes nothing — the theme
                    // toggle saves too — keeps the cache instead.
                    if llm_settings.as_ref() != Some(&settings) {
                        invalidate_runtimes(&factory, &mut runtimes, &active).await;
                    }
                    llm_settings = Some(settings);
                }

                Cmd::SaveSessions { revision, sessions } => {
                    let event = match worker.session_store.save(&sessions).await {
                        Ok(()) => Event::SessionsSaved { revision },
                        Err(error) => Event::SessionSaveFailed {
                            revision,
                            message: error.to_string(),
                        },
                    };
                    sink.emit_ui(event);
                }

                Cmd::SaveConfig { request_id, config } => {
                    let event = match worker.config_store.save(&config).await {
                        Ok(()) => Event::ConfigSaved { request_id },
                        Err(error) => Event::ConfigSaveFailed {
                            request_id,
                            message: error.to_string(),
                        },
                    };
                    sink.emit_ui(event);
                }

                Cmd::SaveApiKey {
                    request_id,
                    api_key,
                } => {
                    let event = match worker.secret_store.save_api_key(api_key.expose()).await {
                        Ok(()) => Event::ApiKeySaved { request_id },
                        Err(error) => Event::ApiKeySaveFailed {
                            request_id,
                            message: error.to_string(),
                        },
                    };
                    sink.emit_ui(event);
                }

                Cmd::ReloadPlugins { request_id, config } => {
                    let result = async {
                        worker.config_store.save(&config).await?;
                        let projects: Vec<PathBuf> =
                            config.projects.iter().map(PathBuf::from).collect();
                        worker
                            .plugin_manager
                            .discover(projects, config.plugins.clone())
                            .await
                    }
                    .await;
                    match result {
                        Ok(plugins) => {
                            surfaces.clear();
                            invalidate_runtimes(&factory, &mut runtimes, &active).await;
                            worker.plugins = plugins.clone();
                            sink.emit_ui(Event::PluginsUpdated {
                                request_id,
                                catalogue: plugins,
                            });
                        }
                        Err(error) => sink.emit_ui(Event::PluginOperationFailed {
                            request_id,
                            message: error.to_string(),
                        }),
                    }
                }

                Cmd::UninstallPlugin {
                    request_id,
                    id,
                    scope,
                    config,
                } => {
                    surfaces.clear();
                    invalidate_runtimes(&factory, &mut runtimes, &active).await;
                    let result = async {
                        worker
                            .plugin_manager
                            .uninstall(&id, &scope, worker.plugins.clone())
                            .await?;
                        worker.config_store.save(&config).await?;
                        let projects: Vec<PathBuf> =
                            config.projects.iter().map(PathBuf::from).collect();
                        worker
                            .plugin_manager
                            .discover(projects, config.plugins.clone())
                            .await
                    }
                    .await;

                    match result {
                        Ok(plugins) => {
                            worker.plugins = plugins.clone();
                            sink.emit_ui(Event::PluginsUpdated {
                                request_id,
                                catalogue: plugins,
                            });
                        }
                        Err(error) => sink.emit_ui(Event::PluginOperationFailed {
                            request_id,
                            message: error.to_string(),
                        }),
                    }
                }

                Cmd::Shutdown => {
                    surfaces.clear();
                    invalidate_runtimes(&factory, &mut runtimes, &active).await;
                    break;
                }

                // The window's task list asking what is running in one project.
                // A project with no agent yet has no jobs — its agent is built
                // lazily on the first run, and a job cannot exist without one.
                Cmd::ListJobs { project } => {
                    let jobs = runtimes
                        .get(&project)
                        .map(|runtime| runtime.jobs.list())
                        .unwrap_or_default()
                        .into_iter()
                        .map(JobView::from)
                        .collect();
                    sink.emit_ui(Event::Jobs { project, jobs });
                }

                // The window's stop button on a task row. The job settles as
                // killed once its work actually stops, and the next poll of the
                // list shows that — there is nothing to report back here.
                Cmd::KillJob { project, job_id } => {
                    let Some(runtime) = runtimes.get(&project) else {
                        continue;
                    };
                    if let Err(error) = runtime
                        .jobs
                        .kill(&job_id, Some("stopped from the window"))
                        .await
                    {
                        tracing::warn!(job = %job_id, %error, "failed to stop a background job");
                    }
                }

                Cmd::Cancel { run_id } => {
                    // Only cancel the run this was meant for: a stale click must
                    // not kill a newer run. Each run has its own token, so the
                    // one named here is the only one stopped.
                    let guard = active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some(token) = guard.get(&run_id) {
                        token.cancel();
                    }
                }

                Cmd::Run {
                    run_id,
                    prompt,
                    history,
                    carried,
                    project,
                    thinking,
                } => {
                    let Some(settings) = llm_settings.clone() else {
                        sink.emit(AgentEvent::RunFailed {
                            run_id,
                            message: "模型设置尚未就绪".into(),
                        });
                        continue;
                    };

                    let agent = match runtimes.get(&project) {
                        Some(runtime) => runtime.agent.clone(),
                        None => {
                            let model = RuntimeModelSettings {
                                base_url: settings.base_url.clone(),
                                model: settings.model.clone(),
                                api_key: settings.api_key.clone(),
                                context: settings.context,
                                max_output_tokens: settings.max_output_tokens,
                                retry_count: if settings.retry_forever {
                                    None
                                } else {
                                    Some(settings.retry_count)
                                },
                                supports_images: settings.supports_images(),
                            };
                            let runtime = match factory
                                .build(&project, &model, worker.plugins.clone())
                                .await
                            {
                                Ok(runtime) => runtime,
                                Err(error) => {
                                    sink.emit(AgentEvent::RunFailed {
                                        run_id,
                                        message: error.to_string(),
                                    });
                                    continue;
                                }
                            };
                            let agent = runtime.agent.clone();
                            runtimes.insert(project.clone(), runtime);
                            agent
                        }
                    };

                    let cancel = CancellationToken::new();
                    active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(run_id, cancel.clone());

                    let sink = sink.clone();
                    let active = active.clone();
                    tokio::spawn(async move {
                        // The measurement the run hands back is already on the
                        // `RunFinished` event, which is what the GUI folds onto
                        // the session; nothing here needs it.
                        if let Err(error) = agent
                            .run(
                                run_id,
                                prompt,
                                RunRequest {
                                    history: &history,
                                    thinking,
                                    carried,
                                },
                                cancel,
                                &sink,
                            )
                            .await
                        {
                            sink.emit(AgentEvent::RunFailed {
                                run_id,
                                message: error.to_string(),
                            });
                        }
                        // Retired here rather than by the worker loop, which has
                        // no other way to learn the run is over.
                        active
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&run_id);
                    });
                }
            }
        }
        surfaces.clear();
        invalidate_runtimes(&factory, &mut runtimes, &active).await;
    });
}

async fn invalidate_runtimes(
    factory: &ProjectRuntimeFactory,
    runtimes: &mut HashMap<PathBuf, Arc<ProjectRuntime>>,
    active: &Arc<Mutex<HashMap<RunId, CancellationToken>>>,
) {
    {
        let runs = active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for cancel in runs.values() {
            cancel.cancel();
        }
    }
    for runtime in runtimes.values() {
        runtime.shutdown().await;
    }
    runtimes.clear();
    // The cache outlives the runtimes it fed; a shut-down actor must not be
    // handed back to the next surface or runtime build.
    factory.clear_components();
}

/// Wraps the app so `eframe` can drive it, and so the runtime outlives the
/// window instead of being dropped at the end of `main`.
struct AgentFrame {
    app: App,
    resources: GuiResources,
    /// Held for the process lifetime. Dropping it at the end of `main` would
    /// stop the agent the moment the window opens, so it lives here instead.
    runtime: Option<tokio::runtime::Runtime>,
}

impl eframe::App for AgentFrame {
    fn logic(&mut self, _ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        // `logic` runs before every frame *and* while the window is hidden, so
        // a run that finishes while minimised is still folded into the view.
        self.app.poll();
    }

    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame, &mut self.resources);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.app.shutdown();

        // Shut the runtime down explicitly rather than letting the field drop,
        // so a tool call still in flight gets a bounded chance to finish
        // instead of being abandoned mid-write.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        }
        tracing::info!("window closed; the agent runtime has stopped");
    }
}
