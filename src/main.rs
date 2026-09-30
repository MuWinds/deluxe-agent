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
use app::{App, ChannelSink, Paths};
use harness::{
    AgentEvent, AgentEventSink, ConfigStore, NativeConfigStore, NativePluginManager,
    NativeSecretStore, PluginManager, SecretStore, SessionStore,
};
use ipc::{Cmd, Event, JobView, LlmSettings, RunId};
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
mod mcp;
mod plugins;
mod process;
mod runtime;
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

    let config = config::load();
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

    // Plugins are discovered once, here, for the same reason the config and the
    // sessions are: the first frame's agent already needs its skill catalogue,
    // and a marketplace file does not change while the window is open. The home
    // directory is passed in rather than looked up inside, so a test can point
    // discovery at a temp directory instead of the real `~/.agents`.
    let home = directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let projects: Vec<PathBuf> = config.projects.iter().map(PathBuf::from).collect();
    let plugin_manager: Arc<dyn PluginManager> = Arc::new(NativePluginManager::new(home.clone()));
    let plugins = match runtime.block_on(plugin_manager.discover(projects, config.plugins.clone()))
    {
        Ok(plugins) => plugins,
        Err(error) => {
            tracing::warn!(%error, "failed to discover plugins");
            Arc::new(PluginCatalogue::default())
        }
    };
    let config_store: Arc<dyn ConfigStore> = Arc::new(NativeConfigStore::new(config_path.clone()));
    let secret_store: Arc<dyn SecretStore> = Arc::new(NativeSecretStore::new());

    let settings = Arc::new(RwLock::new(config.tools.clone()));

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

    let worker = Worker {
        settings,
        plugins,
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

            spawn_worker(
                &handle,
                worker,
                cmd_rx,
                ChannelSink::new(event_tx.clone(), cc.egui_ctx.clone()),
            );

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
                runtime: Some(runtime),
            }))
        }),
    )
}

/// The pieces the worker needs, resolved before the window exists so the first
/// frame never shows a half-initialised state.
struct Worker {
    settings: Arc<RwLock<ToolSettings>>,
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
    sink: ChannelSink,
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
        let factory = ProjectRuntimeFactory::new(worker.settings.clone(), Arc::new(sink.clone()));
        // The model settings the GUI wants. Pushed before the first run and on
        // every settings save; `None` only until then.
        let mut llm_settings: Option<LlmSettings> = None;

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
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
                        runtimes.clear();
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
                            worker.plugins = plugins.clone();
                            runtimes.clear();
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
                    config,
                } => {
                    let result = async {
                        worker
                            .plugin_manager
                            .uninstall(&id, worker.plugins.clone())
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
                            runtimes.clear();
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

                Cmd::Shutdown => break,

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
    });
}

/// Wraps the app so `eframe` can drive it, and so the runtime outlives the
/// window instead of being dropped at the end of `main`.
struct AgentFrame {
    app: App,
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
        self.app.ui(ui, frame);
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
