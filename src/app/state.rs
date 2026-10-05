//! Domain state owned by the application controller.
//!
//! This module deliberately contains no egui types. The renderer consumes this
//! state through the controller methods in the parent module.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::attachments::Attachment;
use crate::config::Config;
use crate::ipc::{Cmd, Event, JobView, RunId};
use crate::llm::ThinkingLevel;
use crate::plugins::descriptor::PluginDescriptor;
use crate::plugins::PluginCatalogue;
use crate::session::{Session, Step};

/// One run in flight, and the session it writes into.
#[derive(Debug, Clone, Copy)]
pub(super) struct ActiveRun {
    pub(super) session: Uuid,
}

/// One delegated sub-agent's own transcript, as its window shows it.
#[derive(Debug, Clone)]
pub(super) struct SubagentRun {
    /// The background job these steps belong to.
    pub(super) job_id: String,
    /// The `task` role the sub-agent runs.
    pub(super) agent: String,
    /// Salt for the renderer's scroll areas.
    pub(super) salt: Uuid,
    pub(super) steps: Vec<Step>,
}

impl SubagentRun {
    /// Creates an empty transcript for a background job.
    pub(super) fn new(job_id: impl Into<String>) -> Self {
        Self {
            job_id: job_id.into(),
            agent: String::new(),
            salt: Uuid::new_v4(),
            steps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ConfigSurface {
    Sidebar,
    Settings,
}

/// Paths resolved before the window opens.
pub struct Paths {
    /// The config file, or `None` when the system offers no config directory.
    pub config_path: Option<PathBuf>,
}

/// Application state that can be folded and tested without a GUI context.
pub struct App {
    pub(super) plugin_surface: Option<super::view_model::PluginSurfaceView>,
    /// The plugin-contributed inline control on the composer row, if any. It is
    /// adopted from the worker's snapshots rather than opened by the GUI, so it
    /// has no separate request bookkeeping.
    pub(super) composer: Option<super::view_model::PluginSurfaceView>,
    pub(super) cmd_tx: mpsc::UnboundedSender<Cmd>,
    pub(super) events: mpsc::UnboundedReceiver<Event>,

    pub(super) config: Config,
    /// The model provider plugin's self-description, read from the plugin. The
    /// host owns neither the model name nor the key, so it only reads the
    /// context window the gauge shows and whether `read_image` is offered.
    pub(super) llm_descriptor: PluginDescriptor,
    /// Whether the model-provider plugin loaded. False means no run can reach a
    /// model, and the composer says so instead of offering to send.
    pub(super) llm_available: bool,
    pub(super) catalogue: Arc<PluginCatalogue>,
    pub(super) paths: Paths,

    pub(super) sessions: Vec<Session>,
    pub(super) selected: Option<Uuid>,
    pub(super) active_project: Option<String>,
    pub(super) active: HashMap<RunId, ActiveRun>,
    pub(super) next_run_id: RunId,
    pub(super) dirty: bool,
    pub(super) session_revision: u64,
    pub(super) save_pending: Option<u64>,
    pub(super) next_config_request_id: u64,
    pub(super) pending_config_saves: HashMap<u64, ConfigSurface>,
    pub(super) config_save_requests: HashMap<ConfigSurface, u64>,
    pub(super) pending_plugin_request: Option<u64>,

    pub(super) prompt: String,
    pub(super) pending_attachments: Vec<Attachment>,
    pub(super) thinking: Option<ThinkingLevel>,
    pub(super) search: String,
    pub(super) expanded_reasoning: HashSet<Uuid>,
    pub(super) stick_to_bottom: bool,
    pub(super) jobs: Vec<JobView>,
    pub(super) jobs_project: Option<PathBuf>,
    pub(super) jobs_last_poll: Option<Instant>,
    pub(super) subagent_runs: Vec<SubagentRun>,
    pub(super) open_subagent: Option<String>,

    pub(super) show_settings: bool,
    pub(super) show_about: bool,
    pub(super) show_plugins: bool,
    pub(super) pending_uninstall: Option<(String, crate::plugins::Scope)>,
    pub(super) show_sidebar: bool,
    pub(super) settings_error: Option<String>,
    pub(super) plugins_error: Option<String>,
    pub(super) sidebar_error: Option<String>,

    /// The renderer's IR, in memory only. See [`super::render_cache`].
    pub(super) render_cache: super::render_cache::RenderCache,
    /// Cleared once the worker reports the renderer could not load, after which
    /// the GUI stops sending render requests and draws the native parser.
    pub(super) renderer_available: bool,
}
