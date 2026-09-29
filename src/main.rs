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

use agent::{Agent, EventSink, RunRequest};
use app::{App, ChannelSink, Paths};
use ipc::{Cmd, Event, JobView, LlmSettings, RunId};
use llm::LlmClient;
use mcp::{tool::McpTool, McpClient};
use plugins::{AgentRole, LoadedPlugin, PluginCatalogue};
use tools::task::Task;
use tools::{ToolRegistry, ToolSettings};

mod agent;
mod app;
mod attachments;
mod code_view;
mod config;
mod context;
mod error;
mod fonts;
mod icons;
mod image_ops;
mod ipc;
mod llm;
mod markdown;
mod mcp;
mod plugins;
mod process;
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

    let config = config::load();
    // Resolved before the window exists and handed to it, so a save writes the
    // same file the load read rather than re-deriving it from the environment.
    let config_path = config::config_path();
    let api_key = config::resolve_api_key().unwrap_or_default();
    // Read before the window exists, so the sidebar shows the previous run's
    // sessions on the very first frame rather than flashing empty.
    let sessions = session::load();

    // Plugins are discovered once, here, for the same reason the config and the
    // sessions are: the first frame's agent already needs its skill catalogue,
    // and a marketplace file does not change while the window is open. The home
    // directory is passed in rather than looked up inside, so a test can point
    // discovery at a temp directory instead of the real `~/.agents`.
    let home = directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let projects: Vec<PathBuf> = config.projects.iter().map(PathBuf::from).collect();
    let plugins = Arc::new(plugins::discover(&home, &projects, &config.plugins));

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

    let settings = Arc::new(RwLock::new(config.tools.clone()));

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

    let worker = Worker { settings, plugins };

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
                    Paths { home, config_path },
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
        let mut agents: HashMap<PathBuf, Arc<Agent>> = HashMap::new();
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
                        agents.clear();
                    }
                    llm_settings = Some(settings);
                }

                // The plugins window enabled, disabled or uninstalled a plugin,
                // and the GUI re-ran discovery against the new config. Every
                // cached agent baked the old catalogue into its system prompt
                // and tool registry, so all of them are stale — the same reason
                // a model-settings push clears the cache.
                Cmd::SetPlugins(plugins) => {
                    worker.plugins = plugins;
                    agents.clear();
                }

                // The window's task list asking what is running in one project.
                // A project with no agent yet has no jobs — its agent is built
                // lazily on the first run, and a job cannot exist without one.
                Cmd::ListJobs { project } => {
                    let jobs = agents
                        .get(&project)
                        .map(|agent| agent.jobs().list())
                        .unwrap_or_default()
                        .into_iter()
                        .map(JobView::from)
                        .collect();
                    sink.emit(Event::Jobs { project, jobs });
                }

                // The window's stop button on a task row. The job settles as
                // killed once its work actually stops, and the next poll of the
                // list shows that — there is nothing to report back here.
                Cmd::KillJob { project, job_id } => {
                    let Some(agent) = agents.get(&project) else {
                        continue;
                    };
                    if let Err(error) = agent.jobs().kill(&job_id, Some("stopped from the window"))
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
                        sink.emit(Event::RunFailed {
                            run_id,
                            message: "模型设置尚未就绪".into(),
                        });
                        continue;
                    };

                    let agent = match agents.get(&project) {
                        Some(agent) => agent.clone(),
                        None => {
                            let client = match LlmClient::new(
                                &settings.base_url,
                                &settings.model,
                                &settings.api_key,
                                settings.max_output_tokens,
                            ) {
                                Ok(client) => client,
                                Err(error) => {
                                    sink.emit(Event::RunFailed {
                                        run_id,
                                        message: error.to_string(),
                                    });
                                    continue;
                                }
                            };

                            // The registry is rebuilt with the agent rather
                            // than held once: whether `read_image` exists is a
                            // property of the model's declared input modalities,
                            // so a modality change has to reach the tool
                            // catalogue and the system prompt that lists it.
                            let mut registry = if settings.supports_images() {
                                ToolRegistry::with_image_input()
                            } else {
                                ToolRegistry::with_builtins()
                            };

                            // Resolved per project: a repository's own plugins
                            // apply here and nowhere else, and so do the MCP
                            // servers they bring.
                            let plugins = worker.plugins.for_project(&project);
                            register_mcp_tools(&mut registry, &plugins).await;

                            // The sub-agent roles this project's plugins
                            // contribute. `task` is registered only when there is
                            // at least one: a tool that can only ever fail is
                            // worse than no tool, and its absence is also what
                            // keeps the prompt from advertising a dead end.
                            let roles: Vec<AgentRole> = plugins
                                .iter()
                                .flat_map(|plugin| plugin.agents.iter().cloned())
                                .collect();
                            if !roles.is_empty() {
                                // The registry *as it stands before `task` joins
                                // it*: the tools a sub-agent may use. Cloning
                                // before registering is what excludes `task`, and
                                // so what bounds delegation to one level.
                                let sub_registry = Arc::new(registry.clone());
                                // The same sink this run's own events go to, so a
                                // delegated sub-agent is observable live: it
                                // forwards its events tagged with its job id, and
                                // the window folds them into that job's
                                // transcript rather than into a session.
                                let forwarder: Arc<dyn EventSink> = Arc::new(sink.clone());
                                registry.register(Arc::new(Task::new(
                                    client.clone(),
                                    roles,
                                    sub_registry,
                                    worker.settings.clone(),
                                    project.clone(),
                                    settings.context,
                                    forwarder,
                                )));
                            }

                            let agent = Arc::new(Agent::new(
                                client,
                                Arc::new(registry),
                                worker.settings.clone(),
                                project.clone(),
                                settings.context,
                                &plugins,
                            ));
                            agents.insert(project.clone(), agent.clone());
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
                            sink.emit(Event::RunFailed {
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

/// Starts the MCP servers the project's plugins declare, and registers the
/// tools they offer.
///
/// A server that will not start, will not handshake, or needs OAuth is logged
/// and skipped. That is the rule discovery already follows, and for the same
/// reason: one broken plugin must not cost the user their agent.
///
/// Each server's client is owned by the tools it produced, which the registry
/// owns, which the agent owns — so a server lives exactly as long as its agent,
/// and a cached agent that is dropped takes its child processes with it.
async fn register_mcp_tools(registry: &mut ToolRegistry, plugins: &[&LoadedPlugin]) {
    for plugin in plugins {
        for (server, config) in &plugin.mcp_servers {
            let mut client = match McpClient::connect(server, config, &plugin.root).await {
                Ok(client) => client,
                Err(error) => {
                    tracing::warn!(server, plugin = %plugin.id, %error, "skipping an MCP server");
                    continue;
                }
            };

            let specs = match client.list_tools().await {
                Ok(specs) => specs,
                Err(error) => {
                    tracing::warn!(server, plugin = %plugin.id, %error, "skipping an MCP server");
                    continue;
                }
            };

            let client = mcp::shared(client);
            for spec in specs {
                let tool = McpTool::new(server, spec, client.clone());
                let name = tool.name().to_string();

                // Two servers are free to offer the same tool name. Keeping the
                // first and saying so beats letting the second quietly replace
                // it — the model would otherwise be told about a tool that is
                // no longer there.
                if registry.get(&name).is_some() {
                    tracing::warn!(
                        tool = %name,
                        server,
                        "another tool already has this name; keeping the first"
                    );
                    continue;
                }

                tracing::info!(tool = %name, server, plugin = %plugin.id, "registered an MCP tool");
                registry.register(Arc::new(tool));
            }
        }
    }
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
        // Last chance to write: `poll` flushes after every terminal event, so
        // this only matters if that save failed and the retry never ran.
        self.app.flush();

        // Shut the runtime down explicitly rather than letting the field drop,
        // so a tool call still in flight gets a bounded chance to finish
        // instead of being abandoned mid-write.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        }
        tracing::info!("window closed; the agent runtime has stopped");
    }
}
