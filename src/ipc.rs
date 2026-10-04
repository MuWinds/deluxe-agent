//! The cross-thread protocol.
//!
//! This is the only vocabulary the GUI thread and the agent worker share. The
//! GUI owns its view state and folds `Event`s into it; the worker never touches
//! that state directly. Keeping it to one enum is what makes the two halves
//! independently testable.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::attachments::ImageRef;
use crate::config::{Config, InputModality};
use crate::context::ContextSettings;
pub use crate::harness::{AuditOutcome, HunkLines, RunId, RunState};
use crate::llm::{Message, ThinkingLevel, Usage, UserTurn};
use crate::plugins::ui_protocol::{PluginUiAction, PluginUiDocument, SurfaceRequest};
use crate::plugins::{PluginCatalogue, PluginSettings, Scope};
use crate::renderer::protocol::{Node, RenderKey, RenderKind, RenderMetrics, ToolRenderRequest};
use crate::session::Session;
use crate::tools::jobs::{JobSnapshot, JobStatus};
use crate::tools::ToolSettings;

/// Which model to talk to, and with what credentials.
///
/// Global worker state, not a per-run attachment: the GUI pushes it with
/// [`Cmd::SetLlmSettings`] at startup and whenever the settings are saved, and
/// every run uses whatever is current — so changing the key or the model takes
/// effect on the next run instead of needing a restart.
#[derive(Clone, PartialEq, Eq)]
pub struct LlmSettings {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub context: ContextSettings,
    /// The `max_tokens` sent with every request, or `None` to leave the
    /// provider's own ceiling in force.
    pub max_output_tokens: Option<u32>,
    /// Additional attempts made after a failed model request.
    pub retry_count: u32,
    /// Keep retrying failed model requests until the run is cancelled.
    pub retry_forever: bool,
    /// The model's input modalities. Part of the agent cache key because
    /// `image` decides whether `read_image` is registered and therefore what
    /// the system prompt advertises.
    pub input: Vec<InputModality>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wraps a secret so protocol debug output cannot reveal its contents.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Borrows the secret for the worker-side adapter.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("\"<redacted>\"")
    }
}

impl LlmSettings {
    /// Whether the configured model declares image input, which is what decides
    /// whether `read_image` is registered.
    pub fn supports_images(&self) -> bool {
        self.input.contains(&InputModality::Image)
    }
}

/// Redacted by hand: the derived form would put the API key in any log line or
/// panic message that happens to format a `Cmd`.
impl std::fmt::Debug for LlmSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmSettings")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("context", &self.context)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("retry_count", &self.retry_count)
            .field("retry_forever", &self.retry_forever)
            .field("input", &self.input)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// GUI → agent.
#[derive(Debug)]
pub enum Cmd {
    OpenPluginSurface(SurfaceRequest),
    PluginUiAction(PluginUiAction),
    ClosePluginSurface(SurfaceRequest),
    /// Imports one Wasmtime plugin directory into the managed cache.
    InstallPlugin {
        request_id: u64,
        component_path: PathBuf,
        scope: Scope,
        config: Box<Config>,
    },
    /// Re-discovers global and project-scoped Wasmtime plugins for the page.
    RefreshPlugins {
        request_id: u64,
        settings: PluginSettings,
    },
    Run {
        run_id: RunId,
        prompt: UserTurn,
        /// The conversation this prompt continues, oldest first and already in
        /// wire format; replayed between the system prompt and `prompt`. Empty
        /// when the run starts a new session.
        history: Vec<Message>,
        /// The context measurement the previous run of this conversation
        /// reported — provider prompt tokens and the message count they
        /// describe. Seeded into the fresh window so the first request of a
        /// long session is already guarded.
        carried: Option<(u64, usize)>,
        /// The directory this run is rooted at: the session's project. It
        /// overrides the global tool settings' working directory for the
        /// duration of the run, which is what lets two projects be worked on
        /// without a restart — and what makes it the agent cache key.
        project: PathBuf,
        /// The reasoning effort this run asks for, taken from the session it
        /// continues. Per-run rather than global because it is a property of the
        /// conversation, not the endpoint: two sessions on one model may think
        /// at different strengths, and a run sends whichever its session holds.
        thinking: Option<ThinkingLevel>,
    },
    Cancel {
        run_id: RunId,
    },
    /// Swaps the tool settings in, so a new working directory or a flipped guard
    /// applies immediately instead of at the next restart.
    SetToolSettings(Box<ToolSettings>),
    /// Swaps the model settings in. Global on purpose: pushed once, and every
    /// session and every run uses whatever is current.
    SetLlmSettings(Box<LlmSettings>),
    /// Persists a session snapshot on the worker thread.
    SaveSessions {
        revision: u64,
        sessions: Vec<Session>,
    },
    /// Persists a config snapshot without blocking the GUI thread.
    SaveConfig {
        request_id: u64,
        config: Box<Config>,
    },
    /// Persists a credential without blocking the GUI thread.
    SaveApiKey {
        request_id: u64,
        api_key: SecretValue,
    },
    /// Persists config and refreshes the plugin catalogue in the worker.
    ReloadPlugins {
        request_id: u64,
        config: Box<Config>,
    },
    /// Uninstalls a plugin's cached copy, persists config, then refreshes plugins.
    UninstallPlugin {
        request_id: u64,
        id: String,
        scope: Scope,
        config: Box<Config>,
    },
    /// Stops the worker after commands already queued have been handled.
    Shutdown,
    /// Asks for the background jobs of one project's agent, so the composer can
    /// show a live task list. Answered with [`Event::Jobs`], echoing the same
    /// `project` so a reply that arrives after the user switched projects can
    /// be discarded.
    ListJobs {
        project: PathBuf,
    },
    /// Stops one background job — a sub-agent or a background command — by id.
    ///
    /// Separate from [`Cmd::Cancel`], which names a run: a job outlives the run
    /// that started it, so the two are not the same handle and stopping one
    /// must not stop the other.
    KillJob {
        project: PathBuf,
        job_id: String,
    },
    /// Asks the worker to render a message body into a display list.
    ///
    /// `revision` is monotonic per `key`; the GUI discards a response whose
    /// revision is older than the one it last requested.
    RenderMessage {
        key: RenderKey,
        revision: u64,
        text: String,
        metrics: RenderMetrics,
    },
    /// Asks the worker to render a tool card body into a display list.
    RenderTool {
        key: RenderKey,
        revision: u64,
        request: ToolRenderRequest,
    },
}

/// agent → GUI.
#[derive(Debug, Clone)]
pub enum Event {
    PluginUiUpdated {
        request: SurfaceRequest,
        document: Arc<PluginUiDocument>,
    },
    PluginUiClosed {
        request: SurfaceRequest,
    },
    PluginUiFailed {
        request: SurfaceRequest,
        message: String,
    },
    /// A coalesced batch of streamed assistant text.
    AssistantDelta {
        run_id: RunId,
        text: String,
    },
    /// Discards the incomplete assistant and reasoning output from a failed
    /// streamed request before its retry begins.
    AssistantTurnReset {
        run_id: RunId,
    },
    /// A coalesced batch of streamed chain-of-thought text. It arrives before
    /// the answer it produced, and the UI folds it away by default.
    ReasoningDelta {
        run_id: RunId,
        text: String,
    },
    /// One assistant turn is complete. Whether more turns follow is visible from
    /// the `ToolStarted` events that arrive next, so the calls themselves are
    /// not repeated here.
    AssistantDone {
        run_id: RunId,
        content: String,
    },
    /// A host-authored notice for the transcript — not a model turn.
    ///
    /// The agent emits one when it has something to say that no tool produced,
    /// such as a background job that finished while the run was in flight and
    /// was injected into the model's next step.
    Notice {
        run_id: RunId,
        text: String,
    },
    ToolStarted {
        run_id: RunId,
        call_id: String,
        name: String,
        arguments: Value,
        /// The model's own JSON text for the call, stored verbatim so a replay
        /// sends the same bytes the live run did.
        raw_arguments: String,
    },
    /// One tool call is over.
    ///
    /// `outcome` carries both whether it worked and why it did not. It replaces
    /// what used to be a separate `ok: bool` plus a second `Event::Audit` — two
    /// events for one call, which the GUI could not even join up, because the
    /// audit entry carried the tool *name* and no call id.
    ToolFinished {
        run_id: RunId,
        call_id: String,
        outcome: AuditOutcome,
        output: String,
        /// Images the call produced, carried as durable references. Empty for
        /// every tool but `read_image`.
        images: Vec<ImageRef>,
        /// Real file line numbers an `apply_patch` call resolved its hunks to.
        /// Empty for every other tool; see [`crate::tools::HunkLines`].
        hunks: Vec<HunkLines>,
        duration_ms: u64,
    },
    /// A mid-run usage sample: the provider's prompt-token count for the
    /// request that just came back, while the run is still going. The context
    /// gauge reads this so it tracks a multi-turn run instead of jumping once
    /// at the end; it is only emitted when more turns follow, so a finished
    /// run's figure still arrives once, via [`Event::RunFinished`]. `None`
    /// clears the gauge — sent after a compaction, whose summary request
    /// invalidates the figure the earlier turns were measured on.
    UsageSampled {
        run_id: RunId,
        measurement: Option<(u64, usize)>,
        /// The turn's provider usage, when it reported one. The context gauge
        /// reads `measurement`, and the cache counters read this, so both move
        /// the moment the turn comes back rather than waiting for the run to
        /// end. `None` when the provider reported no usage — including the
        /// reset sent after a compaction.
        usage: Option<Usage>,
    },
    RunFinished {
        run_id: RunId,
        usage: Option<Usage>,
        /// The context measurement to carry into the next run of this
        /// conversation. `None` when the provider reported no usage.
        measurement: Option<(u64, usize)>,
    },
    /// Context compaction has begun: `dropping` older messages are about to be
    /// folded into one summary, while the recent tail stays verbatim.
    CompactionStarted {
        run_id: RunId,
        /// How many messages the summary replaces. No longer rendered — the
        /// composer's status line is gone — but kept so the event still
        /// describes what is happening, and read by the loop tests.
        #[allow(dead_code)]
        dropping: usize,
    },
    /// Context compaction is over. `summary` is what the model wrote about the
    /// folded turns, empty when no summary could be produced. `keep` is how
    /// many trailing messages survived verbatim.
    Compacted {
        run_id: RunId,
        summary: String,
        #[allow(dead_code)]
        keep: usize,
    },
    RunFailed {
        run_id: RunId,
        message: String,
    },
    /// The background jobs of one project, in registration order. A reply to
    /// [`Cmd::ListJobs`]; not tied to a run, so it is folded before the run
    /// guard in the window. `project` is echoed so the window can drop a reply
    /// for a project it is no longer showing.
    Jobs {
        project: PathBuf,
        jobs: Vec<JobView>,
    },
    /// Confirms that a session snapshot reached the store.
    SessionsSaved {
        revision: u64,
    },
    /// Reports a session-store failure while leaving the app's snapshot dirty.
    SessionSaveFailed {
        revision: u64,
        message: String,
    },
    /// Confirms that a config snapshot reached disk.
    ConfigSaved {
        request_id: u64,
    },
    /// Reports a config persistence failure.
    ConfigSaveFailed {
        request_id: u64,
        message: String,
    },
    /// Confirms that an API credential reached the operating-system store.
    ApiKeySaved {
        request_id: u64,
    },
    /// Reports an operating-system credential-store failure.
    ApiKeySaveFailed {
        request_id: u64,
        message: String,
    },
    /// Replaces the GUI's plugin catalogue after a worker refresh.
    PluginsUpdated {
        request_id: u64,
        catalogue: Arc<PluginCatalogue>,
    },
    /// Reports a plugin lifecycle operation failure.
    PluginOperationFailed {
        request_id: u64,
        message: String,
    },
    /// Confirms an imported Wasmtime plugin and replaces the catalogue/config.
    PluginInstalled {
        request_id: u64,
        config: Box<Config>,
        catalogue: Arc<PluginCatalogue>,
    },
    /// A delegated sub-agent has started: the role it runs and the brief it was
    /// handed.
    ///
    /// Sent once, before any of its work, so the window can show the whole
    /// prompt — the job label only carries its first line. This is the window's
    /// analogue of the harness's agent-spawn event, which likewise carries the
    /// initial prompt.
    SubagentStarted {
        job_id: String,
        /// The `task` role the sub-agent runs, e.g. `example-agent`.
        agent: String,
        /// The brief, verbatim and untruncated.
        prompt: String,
    },
    /// One event from a sub-agent's own run, forwarded live and tagged with the
    /// job that owns it.
    ///
    /// A sub-agent's run id is its own and means nothing to the window's `active`
    /// map, so its events ride in here instead: the window folds them into that
    /// sub-agent's transcript rather than into a session. Boxed because the
    /// inner event is the same large enum this one belongs to.
    Subagent {
        job_id: String,
        event: Box<Event>,
    },
    /// A message body was rendered. `nodes` is already decoded and validated.
    MessageRendered {
        key: RenderKey,
        revision: u64,
        nodes: Arc<Vec<Node>>,
    },
    /// A tool card was rendered. `nodes` is already decoded and validated.
    ToolRendered {
        key: RenderKey,
        revision: u64,
        nodes: Arc<Vec<Node>>,
    },
    /// A render request failed; the GUI falls back to plain text. The message
    /// is a machine code plus a short human half, never the body text.
    RenderFailed {
        key: RenderKey,
        revision: u64,
        kind: RenderKind,
        message: String,
    },
    /// The transcript-renderer plugin's availability changed. Emitted at worker
    /// start and whenever the plugin catalogue is reloaded. When `available` is
    /// false the GUI draws every body as plain text.
    RendererAvailability {
        available: bool,
    },
}

/// Where a background job is, as the window's task list shows it.
///
/// A mirror of [`crate::tools::jobs::JobStatus`] with no dependency on the tool
/// layer, so the composer can colour a row without reaching into the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    /// Cancellation was requested and the job is winding down.
    Stopping,
    Completed,
    Killed,
    Failed,
}

impl JobState {
    /// Whether the job has reached a terminal state.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Completed | Self::Killed | Self::Failed)
    }

    /// The Chinese word this state shows in a task row.
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "运行中",
            Self::Stopping => "正在停止",
            Self::Completed => "完成",
            Self::Killed => "已停止",
            Self::Failed => "失败",
        }
    }
}

/// One background job, as the composer's task list shows it.
///
/// An owned projection of [`JobSnapshot`]: the snapshot borrows its `kind` as a
/// `&'static str` and never leaves the worker, so what crosses the channel is
/// this plain, owned form.
#[derive(Debug, Clone)]
pub struct JobView {
    pub id: String,
    /// The job's tool name (`bash`, `exec`, …).
    /// `subagent` for a delegated child, otherwise the job's tool name (`bash`).
    pub kind: String,
    pub label: String,
    pub state: JobState,
    /// Kind-specific detail, e.g. `exit code: 3`.
    pub detail: Option<String>,
}

impl JobView {
    /// Whether this job is a delegated sub-agent rather than a shell command.
    pub fn is_subagent(&self) -> bool {
        self.kind == "subagent"
    }

    /// Whether the job has reached a terminal state.
    pub fn is_settled(&self) -> bool {
        self.state.is_settled()
    }
}

impl From<JobSnapshot> for JobView {
    /// Projects a worker-side snapshot onto the window's owned vocabulary.
    fn from(snapshot: JobSnapshot) -> Self {
        let state = match snapshot.status {
            JobStatus::Running => JobState::Running,
            JobStatus::Stopping => JobState::Stopping,
            JobStatus::Completed => JobState::Completed,
            JobStatus::Killed => JobState::Killed,
            JobStatus::Failed => JobState::Failed,
        };
        Self {
            id: snapshot.id,
            kind: snapshot.kind.to_string(),
            label: snapshot.label,
            state,
            detail: snapshot.detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(kind: &'static str, status: JobStatus) -> JobSnapshot {
        JobSnapshot {
            id: format!("{kind}-1"),
            kind,
            label: "a job".into(),
            status,
            detail: Some("exit code: 0".into()),
            exit_code: Some(0),
        }
    }

    #[test]
    fn a_job_view_carries_everything_a_row_reads() {
        let view = JobView::from(snapshot("subagent", JobStatus::Completed));

        assert_eq!(view.id, "subagent-1");
        assert_eq!(view.kind, "subagent");
        assert!(view.is_subagent(), "the kind is what marks a delegation");
        assert_eq!(view.state, JobState::Completed);
        assert!(view.is_settled(), "a completed job is settled");
        assert_eq!(view.detail.as_deref(), Some("exit code: 0"));
    }

    #[test]
    fn job_status_maps_onto_the_window_vocabulary() {
        // Every status has a state, and a running one is not settled — the
        // composer keeps polling while it is not.
        assert_eq!(
            JobView::from(snapshot("bash", JobStatus::Running)).state,
            JobState::Running
        );
        assert!(!JobView::from(snapshot("bash", JobStatus::Running)).is_settled());
        assert_eq!(
            JobView::from(snapshot("bash", JobStatus::Stopping)).state,
            JobState::Stopping
        );
        assert_eq!(
            JobView::from(snapshot("bash", JobStatus::Killed)).state,
            JobState::Killed
        );
        assert_eq!(
            JobView::from(snapshot("bash", JobStatus::Failed)).state,
            JobState::Failed
        );
        assert!(!JobView::from(snapshot("bash", JobStatus::Killed)).is_subagent());
    }
}
