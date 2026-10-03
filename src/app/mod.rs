//! The control panel's state, and the vocabulary it shares with the agent.
//!
//! The window owns no agent state: it sends [`Cmd`]s down one channel and folds
//! [`Event`]s coming back up the other into its own view state. That split is
//! what lets a run be tested without a window, and it is why this module holds
//! no widgets — the egui rendering lives in the `ui` child module, so the state
//! half stays testable without an egui context.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::attachments::{self, ImageRef};
use crate::config::{Config, API_KEY_ENV};
use crate::image_ops;
use crate::ipc::{Cmd, Event, JobState, JobView, LlmSettings, RunId, RunState, SecretValue};
use crate::llm::UserTurn;
use crate::plugins::{self, PluginCatalogue};
use crate::session::{self, Session, Step, ToolResult};

mod controller;
mod events;
mod intents;
mod plugin_ui;
mod render_cache;
mod resources;
mod state;
mod ui;
mod view_model;

use intents::{UiEffects, UiIntent};

pub use events::{EventSink, RepaintSignal};
pub use resources::GuiResources;
use state::{ActiveRun, ConfigSurface, SubagentRun};
pub use state::{App, Paths};

/// How often the composer asks the worker for its project's background jobs.
///
/// The window never learns about a job directly — the registry lives in the
/// worker — so the task list is a poll. Half a second is fast enough to read as
/// live and cheap enough that a long run does not flood the channel.
const JOBS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

/// The most sub-agent transcripts the window keeps at once.
///
/// A sub-agent's events arrive whether or not its window is open — that is what
/// makes opening one after the fact useful — so without a cap a window left up
/// for days would hold one transcript per delegation ever made. Past the cap the
/// oldest is dropped, never the one on screen.
const MAX_SUBAGENT_RUNS: usize = 16;

/// A token count for the composer, in the shortest unit that fits.
///
/// The gauge label is a percentage, so this appears only in the hover text —
/// but a raw `131072` in a tooltip is exactly the kind of counting the user
/// should not have to do. Under a thousand is shown exactly; anything larger
/// drops to `k` / `M` with one decimal, trailing `.0` trimmed.
fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    let (value, unit) = if tokens < 1_000_000 {
        (tokens as f32 / 1_000.0, "k")
    } else {
        (tokens as f32 / 1_000_000.0, "M")
    };
    let text = format!("{value:.1}");
    let text = text.strip_suffix(".0").unwrap_or(&text);
    format!("{text}{unit}")
}

/// Reads a token count a user typed, `1M` / `8k` / `8KiB` / `8192` alike.
///
/// The settings field is a text edit, so the shorthand people already use for
/// context windows should parse rather than bounce off `f64::from_str`. `k`
/// means 1024 and `M` means 1024², matching how a model's context window is
/// conventionally quoted; `KiB` / `MiB` spellings are accepted as the same
/// numbers. A plain integer is read as tokens, not kibibytes. The result is
/// clamped to [`u64::MAX`] so an absurd figure saturates instead of erroring;
/// `None` is left to the caller to interpret, which keeps this decoupled from
/// any one field's idea of an empty value.
fn parse_token_count(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    // Split at the first character that is not a digit — `find` + `split_at`
    // rather than `split_once`, which treats the match as a delimiter and eats
    // it: `"1M"` would come back as `("1", "")` and the unit would be lost,
    // silently turning every shorthand into its bare number.
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let unit = unit.trim();

    let count: u64 = digits.parse().ok()?;
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "" => 1,
        "k" | "kib" => 1024,
        "m" | "mib" => 1024 * 1024,
        _ => return None,
    };
    Some(count.saturating_mul(multiplier))
}

impl App {
    /// Builds the window from the state `main` resolved before it opened.
    ///
    /// Every input is passed in rather than looked up here, so a first frame
    /// shows a fully-initialised app and a test can point the paths at a temp
    /// directory.
    pub fn new(
        cmd_tx: mpsc::UnboundedSender<Cmd>,
        events: mpsc::UnboundedReceiver<Event>,
        config: Config,
        api_key: String,
        sessions: Vec<Session>,
        catalogue: Arc<PluginCatalogue>,
        paths: Paths,
    ) -> Self {
        // The first project is where the window opens, so a prompt typed before
        // anything is clicked still lands somewhere sensible.
        let active_project = config.projects.first().cloned();
        let selected = sessions.first().map(|s| s.id);

        Self {
            plugin_surface: None,
            cmd_tx,
            events,
            config,
            api_key,
            catalogue,
            paths,
            sessions,
            selected,
            active_project,
            active: HashMap::new(),
            next_run_id: 1,
            dirty: false,
            session_revision: 0,
            save_pending: None,
            next_config_request_id: 1,
            pending_config_saves: HashMap::new(),
            config_save_requests: HashMap::new(),
            pending_api_key_save: None,
            pending_plugin_request: None,
            prompt: String::new(),
            pending_images: Vec::new(),
            thinking: None,
            search: String::new(),
            context_limit_text: String::new(),
            max_output_tokens_text: String::new(),
            expanded_reasoning: HashSet::new(),
            stick_to_bottom: true,
            jobs: Vec::new(),
            jobs_project: None,
            jobs_last_poll: None,
            subagent_runs: Vec::new(),
            open_subagent: None,
            show_settings: false,
            show_about: false,
            show_plugins: false,
            pending_uninstall: None,
            show_sidebar: true,
            settings_error: None,
            plugins_error: None,
            sidebar_error: None,
            render_cache: render_cache::RenderCache::default(),
            renderer_available: true,
        }
    }

    /// Drains every pending worker event and folds it into the view, then
    /// flushes any durable change.
    ///
    /// Called from `logic`, which eframe runs even while the window is hidden,
    /// so a run that finishes while minimised is not lost.
    pub fn poll(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.apply(event);
        }
        self.flush();
    }

    /// Queues the session store if anything durable changed.
    ///
    /// File access stays on the worker. A failed write leaves `dirty` set for a
    /// later poll to retry rather than silently dropping the change.
    pub fn flush(&mut self) {
        if !self.dirty || self.save_pending.is_some() {
            return;
        }
        let revision = self.session_revision;
        if self
            .cmd_tx
            .send(Cmd::SaveSessions {
                revision,
                sessions: self.sessions.clone(),
            })
            .is_ok()
        {
            self.save_pending = Some(revision);
        } else {
            tracing::warn!("agent worker is gone; the session snapshot was not queued");
        }
    }

    /// Queues the latest snapshot before asking the worker to stop.
    pub fn shutdown(&mut self) {
        if self.dirty {
            let revision = self.session_revision;
            if self
                .cmd_tx
                .send(Cmd::SaveSessions {
                    revision,
                    sessions: self.sessions.clone(),
                })
                .is_err()
            {
                tracing::warn!("agent worker is gone; the final session snapshot was not queued");
            }
        }
        if self.cmd_tx.send(Cmd::Shutdown).is_err() {
            tracing::warn!("agent worker is gone; shutdown could not be requested");
        }
    }

    /// Folds one worker event into the session its run writes into.
    ///
    /// `Jobs` and sub-agent events are handled before the run guard, since they
    /// are not scoped to a run this window started.
    fn apply(&mut self, event: Event) {
        if self.fold_plugin_ui(&event) {
            return;
        }
        // Jobs are not tied to a run, so they are folded before the run guard
        // below — there is no `active` entry for them to match, and dropping
        // them there would leave the task list permanently empty. A reply for a
        // project the window is no longer showing is discarded.
        //
        // A sub-agent's events are here for the same reason: they carry the
        // sub-agent's own run id, which no run the window started can match.
        // They belong to a job, not a session, so they fold into that job's
        // transcript instead.
        let event = match event {
            Event::ApiKeySaved { request_id } => {
                if self.pending_api_key_save == Some(request_id) {
                    self.pending_api_key_save = None;
                    self.settings_error = None;
                    self.finish_save_settings();
                }
                return;
            }
            Event::ApiKeySaveFailed {
                request_id,
                message,
            } => {
                if self.pending_api_key_save == Some(request_id) {
                    self.pending_api_key_save = None;
                    self.settings_error = Some(format!("存储 API Key 失败：{message}"));
                }
                return;
            }
            Event::ConfigSaved { request_id } => {
                if let Some(surface) = self.pending_config_saves.remove(&request_id) {
                    if self.config_save_requests.get(&surface) == Some(&request_id) {
                        match surface {
                            ConfigSurface::Sidebar => self.sidebar_error = None,
                            ConfigSurface::Settings => self.settings_error = None,
                        }
                    }
                }
                return;
            }
            Event::ConfigSaveFailed {
                request_id,
                message,
            } => {
                if let Some(surface) = self.pending_config_saves.remove(&request_id) {
                    if self.config_save_requests.get(&surface) == Some(&request_id) {
                        let error = format!("保存配置失败：{message}");
                        match surface {
                            ConfigSurface::Sidebar => self.sidebar_error = Some(error),
                            ConfigSurface::Settings => self.settings_error = Some(error),
                        }
                    }
                }
                return;
            }
            Event::PluginsUpdated {
                request_id,
                catalogue,
            } => {
                if self.pending_plugin_request == Some(request_id) {
                    self.close_plugin_surface();
                    self.catalogue = catalogue;
                    self.pending_plugin_request = None;
                    self.plugins_error = None;
                }
                return;
            }
            Event::PluginInstalled {
                request_id,
                config,
                catalogue,
            } => {
                if self.pending_plugin_request == Some(request_id) {
                    self.close_plugin_surface();
                    self.config = *config;
                    self.catalogue = catalogue;
                    self.pending_plugin_request = None;
                    self.plugins_error = None;
                }
                return;
            }
            Event::PluginOperationFailed {
                request_id,
                message,
            } => {
                if self.pending_plugin_request == Some(request_id) {
                    self.pending_plugin_request = None;
                    self.plugins_error = Some(message);
                }
                return;
            }
            Event::SessionsSaved { revision } => {
                if self.save_pending == Some(revision) {
                    self.save_pending = None;
                    if self.session_revision == revision {
                        self.dirty = false;
                    }
                }
                return;
            }
            Event::SessionSaveFailed { revision, message } => {
                if self.save_pending == Some(revision) {
                    self.save_pending = None;
                }
                tracing::warn!(%message, revision, "failed to save the session store");
                return;
            }
            Event::Jobs { project, jobs } => {
                if self.jobs_project.as_ref() == Some(&project) {
                    self.jobs = jobs;
                }
                return;
            }
            Event::SubagentStarted {
                job_id,
                agent,
                prompt,
            } => {
                let run = self.subagent_run_mut(&job_id);
                run.agent = agent;
                // The brief as its own first step, so the transcript reads the
                // way a conversation does — and shows the whole prompt, which
                // the job label's first line cannot.
                run.steps.push(Step::User {
                    text: prompt,
                    images: Vec::new(),
                });
                return;
            }
            Event::Subagent { job_id, event } => {
                self.apply_subagent(&job_id, *event);
                return;
            }
            // Render results are not tied to a run: they land in the cache and
            // the draw loop picks them up by key.
            Event::MessageRendered {
                key,
                revision,
                nodes,
            }
            | Event::ToolRendered {
                key,
                revision,
                nodes,
            } => {
                self.render_cache.store(&key, revision, nodes);
                return;
            }
            Event::RenderFailed {
                key,
                revision,
                kind,
                message,
            } => {
                tracing::warn!(?kind, %message, "renderer request failed");
                self.render_cache.fail(&key, revision);
                return;
            }
            Event::RendererAvailability { available } => {
                if available && !self.renderer_available {
                    // Failed renders are never retried by `collect_rendered`, so
                    // a renderer that comes back would otherwise leave the
                    // transcript stuck on the fallback. Dropping the cache makes
                    // every visible step ask again.
                    self.render_cache.clear();
                }
                if !available {
                    tracing::warn!("transcript renderer unavailable; drawing plain text");
                }
                self.renderer_available = available;
                return;
            }
            other => other,
        };

        // A run this app never started has nowhere to write — a late fragment
        // from a cancelled run, or an event for a run whose terminal event has
        // already been folded in. Dropping it here is what keeps one run's
        // events from landing in whichever session happens to be open.
        let run_id = event_run_id(&event);
        let Some(session_id) = self.active.get(&run_id).map(|run| run.session) else {
            return;
        };

        // Computed before the `match` below, which consumes `event`: a terminal
        // event is also the last event its run can emit, so the run is retired
        // here rather than in each of the two branches.
        let terminal = matches!(event, Event::RunFinished { .. } | Event::RunFailed { .. });

        match event {
            Event::AssistantDelta { text, .. } => {
                if let Some(session) = self.session_mut(session_id) {
                    session.push_assistant(&text);
                }
            }

            Event::AssistantTurnReset { .. } => {
                if let Some(session) = self.session_mut(session_id) {
                    session::reset_turn(&mut session.steps);
                }
            }

            Event::ReasoningDelta { text, .. } => {
                if let Some(session) = self.session_mut(session_id) {
                    session.push_reasoning(&text);
                }
            }

            // The deltas above already built the text. This only matters for a
            // provider that answers in one piece with no streaming chunks.
            Event::AssistantDone { content, .. } => {
                if let Some(session) = self.session_mut(session_id) {
                    session::push_answer(&mut session.steps, &content);
                }
            }

            // A host-authored message — a background job that finished mid-run,
            // for instance. It reached the model as a user turn, so it is
            // recorded as one the replay will reproduce; the transcript shows it
            // the same way it shows a notice.
            Event::Notice { text, .. } => {
                if let Some(session) = self.session_mut(session_id) {
                    session.steps.push(Step::HostMessage { text });
                }
            }

            Event::ToolStarted {
                call_id,
                name,
                arguments,
                raw_arguments,
                ..
            } => {
                if let Some(session) = self.session_mut(session_id) {
                    session.push_tool(call_id, name, arguments, Some(raw_arguments));
                }
            }

            Event::ToolFinished {
                call_id,
                outcome,
                output,
                images,
                hunks,
                duration_ms,
                ..
            } => {
                if let Some(session) = self.session_mut(session_id) {
                    session.finish_tool(
                        &call_id,
                        ToolResult {
                            outcome,
                            output,
                            images,
                            hunks,
                            duration_ms,
                        },
                    );
                }
            }

            Event::RunFinished {
                usage, measurement, ..
            } => {
                if let Some(session) = self.session_mut(session_id) {
                    session.state = RunState::Finished;
                    session.record_cache_usage(usage.as_ref());
                    if usage.is_some() {
                        session.usage = usage;
                    }
                    // Carried into the next run of this conversation, so a long
                    // session's first request is already guarded.
                    session.context_measurement = measurement;
                }
                self.mark_dirty();
            }

            // A compaction is announced before the summary request goes out and
            // confirmed afterwards. Marking it here keeps the transcript honest
            // about why the older turns stopped appearing.
            Event::CompactionStarted { .. } => {}

            Event::Compacted { summary, keep, .. } => {
                if let Some(session) = self.session_mut(session_id) {
                    // The marker goes in at the tail boundary, not at the end:
                    // everything before it is folded, everything after it is
                    // the tail the compacting run kept verbatim.
                    session.record_compaction(summary.clone(), keep);
                }
                self.mark_dirty();
            }

            // A mid-run usage sample lands the moment its turn came back, so
            // the gauge tracks the run instead of jumping once at the end. The
            // value is the same one the agent's own compaction check reads, so
            // the indicator can never run ahead of the mechanism.
            Event::UsageSampled {
                measurement, usage, ..
            } => {
                if let Some(session) = self.session_mut(session_id) {
                    session.context_measurement = measurement;
                    // The cache counters ride the same live sample: the turn's
                    // figures fold into the conversation totals, and the most
                    // recent request's usage replaces the last one — so the
                    // tooltip tracks a multi-turn run instead of jumping at the
                    // end. A `None` sample (the post-compaction reset) carries
                    // no figures and leaves both untouched.
                    session.record_cache_usage(usage.as_ref());
                    if usage.is_some() {
                        session.usage = usage;
                    }
                }
            }

            Event::RunFailed { message, .. } => {
                if let Some(session) = self.session_mut(session_id) {
                    session.state = RunState::Failed;
                    session.steps.push(Step::Notice {
                        text: message.clone(),
                    });
                }
                self.mark_dirty();
            }

            // Already folded in before the run guard above; unreachable here,
            // and listed only so this match stays exhaustive. A sub-agent's
            // events are the same story — they belong to a job, not a session.
            Event::Jobs { .. }
            | Event::SubagentStarted { .. }
            | Event::Subagent { .. }
            | Event::SessionsSaved { .. }
            | Event::SessionSaveFailed { .. }
            | Event::ApiKeySaved { .. }
            | Event::ApiKeySaveFailed { .. }
            | Event::ConfigSaved { .. }
            | Event::ConfigSaveFailed { .. }
            | Event::PluginsUpdated { .. }
            | Event::PluginInstalled { .. }
            | Event::PluginUiUpdated { .. }
            | Event::PluginUiClosed { .. }
            | Event::PluginUiFailed { .. }
            | Event::PluginOperationFailed { .. }
            | Event::MessageRendered { .. }
            | Event::ToolRendered { .. }
            | Event::RenderFailed { .. }
            | Event::RendererAvailability { .. } => {}
        }

        // A terminal event is also the last event its run can emit, so the run
        // is retired here rather than in both branches above.
        if terminal {
            self.active.remove(&run_id);
        }
    }

    /// The session with `id`, mutably.
    fn session_mut(&mut self, id: Uuid) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|session| session.id == id)
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.session_revision = self.session_revision.wrapping_add(1);
    }

    /// Folds one event a sub-agent forwarded into that sub-agent's transcript.
    ///
    /// The same folding a session gets, over a bare step list: a sub-agent's
    /// turns arrive as the same deltas and tool events a run's do, and only the
    /// place they land differs. Events that describe a run's bookkeeping —
    /// usage, compaction — are dropped: a sub-agent has no gauge and no session
    /// to compact, and its window shows the conversation, not the accounting.
    fn apply_subagent(&mut self, job_id: &str, event: Event) {
        let run = self.subagent_run_mut(job_id);
        match event {
            Event::AssistantDelta { text, .. } => session::push_assistant(&mut run.steps, &text),
            Event::AssistantTurnReset { .. } => session::reset_turn(&mut run.steps),
            Event::ReasoningDelta { text, .. } => session::push_reasoning(&mut run.steps, &text),
            Event::AssistantDone { content, .. } => session::push_answer(&mut run.steps, &content),
            Event::Notice { text, .. } => run.steps.push(Step::HostMessage { text }),
            Event::ToolStarted {
                call_id,
                name,
                arguments,
                raw_arguments,
                ..
            } => session::push_tool(
                &mut run.steps,
                call_id,
                name,
                arguments,
                Some(raw_arguments),
            ),
            Event::ToolFinished {
                call_id,
                outcome,
                output,
                images,
                hunks,
                duration_ms,
                ..
            } => session::finish_tool(
                &mut run.steps,
                &call_id,
                ToolResult {
                    outcome,
                    output,
                    images,
                    hunks,
                    duration_ms,
                },
            ),
            // A delegation that failed says so where its answer would have
            // been, rather than leaving the transcript hanging mid-turn.
            Event::RunFailed { message, .. } => run.steps.push(Step::Notice { text: message }),
            _ => {}
        }
    }

    /// The transcript for `job_id`, starting one on first sight.
    ///
    /// A sub-agent's events can arrive before its window is ever opened, which
    /// is what makes opening one after the fact worthwhile — so the transcript
    /// is kept from the first event, not from the first click. Past
    /// [`MAX_SUBAGENT_RUNS`] the oldest is dropped, skipping the one on screen:
    /// a window the user is reading must not vanish under them.
    fn subagent_run_mut(&mut self, job_id: &str) -> &mut SubagentRun {
        if !self.subagent_runs.iter().any(|run| run.job_id == job_id) {
            if self.subagent_runs.len() >= MAX_SUBAGENT_RUNS {
                let oldest = self
                    .subagent_runs
                    .iter()
                    .position(|run| Some(run.job_id.as_str()) != self.open_subagent.as_deref());
                if let Some(oldest) = oldest {
                    self.subagent_runs.remove(oldest);
                }
            }
            self.subagent_runs.push(SubagentRun::new(job_id));
        }

        self.subagent_runs
            .iter_mut()
            .find(|run| run.job_id == job_id)
            .expect("the run was just added")
    }

    /// Shows an error to the user the way the transcript does: as a notice
    /// bubble in the open conversation — the same red bubble a failed run uses.
    ///
    /// The composer's old status line is gone, so this is where a send-time
    /// error goes now. `session` names the conversation the error belongs to
    /// when the caller knows it (a run that failed to dispatch); `None` falls
    /// back to whichever conversation is open. With none open there is no
    /// bubble to draw in, so the error is only logged.
    fn notice_error(&mut self, session: Option<Uuid>, text: impl Into<String>) {
        let text = text.into();
        let Some(id) = session.or(self.selected) else {
            tracing::warn!(error = %text, "an error had no open conversation to show in");
            return;
        };
        let Some(session) = self.session_mut(id) else {
            tracing::warn!(error = %text, "an error's conversation is gone");
            return;
        };
        session.steps.push(Step::Notice { text });
        self.mark_dirty();
    }

    /// The run writing into `session`, if one is.
    fn run_for(&self, session: Uuid) -> Option<RunId> {
        self.active
            .iter()
            .find(|(_, run)| run.session == session)
            .map(|(run_id, _)| *run_id)
    }

    /// The session with `id`, immutably.
    fn session(&self, id: Uuid) -> Option<&Session> {
        self.sessions.iter().find(|session| session.id == id)
    }

    /// The session the window currently has open.
    fn selected_session(&self) -> Option<&Session> {
        self.selected.and_then(|id| self.session(id))
    }

    /// Whether the composer holds something worth sending.
    ///
    /// Whether the session it would go to is *already* running is a separate
    /// question, asked where the button is drawn: the composer is a Stop button
    /// in that state, and one session running must not disable another.
    fn can_send(&self) -> bool {
        !self.prompt.trim().is_empty() || !self.pending_images.is_empty()
    }

    /// Opens a fresh, empty session and shows the empty state.
    ///
    /// The active project is deliberately kept: a new chat belongs where the
    /// user is looking, which is what makes the sidebar's project selection
    /// meaningful. Any run already in flight is left alone — starting a second
    /// conversation beside it is the point.
    fn new_session(&mut self) {
        self.selected = None;
        self.search.clear();
        // A fresh chat starts from the default: no parameter at all, so it is
        // compatible with any endpoint until the user asks for more.
        self.thinking = None;
        self.stick_to_bottom = true;
    }

    /// The project a send from the composer belongs to.
    ///
    /// The open conversation's own project, or — with no conversation open —
    /// the one the sidebar points at. This is the same lookup `start_run` uses
    /// to pick the session it appends to, so the recorded turn and the run
    /// cannot disagree about which project they are in.
    fn sending_project(&self) -> String {
        self.selected
            .and_then(|id| {
                self.sessions
                    .iter()
                    .find(|session| session.id == id && session.state != RunState::Running)
            })
            .map(|session| session.project.clone())
            .or_else(|| self.active_project.clone())
            .or_else(|| self.config.projects.first().cloned())
            .unwrap_or_default()
    }

    /// The project whose background jobs the composer's task list shows.
    ///
    /// The open conversation's own project, else the sidebar's active project.
    /// Unlike [`App::sending_project`] this does *not* skip a running session —
    /// the task list belongs to whatever is on screen, running or not.
    fn displayed_project(&self) -> String {
        self.selected
            .and_then(|id| self.session(id))
            .map(|session| session.project.clone())
            .or_else(|| self.active_project.clone())
            .or_else(|| self.config.projects.first().cloned())
            .unwrap_or_default()
    }

    /// Refreshes the composer's background-job list from the worker.
    ///
    /// The registry lives in the worker, so the list is a poll: this asks for
    /// the displayed project's jobs at most every [`JOBS_POLL_INTERVAL`], and
    /// schedules the next frame only while something is still live — a run in
    /// flight or an unfinished job — so an idle window does not spin.
    fn poll_jobs_intent(&mut self) -> Option<UiIntent> {
        let project = PathBuf::from(self.displayed_project());
        // Switching conversations switches projects: drop the old list at once
        // so another project's jobs are never shown, and ask again immediately
        // rather than waiting out the throttle.
        if self.jobs_project.as_ref() != Some(&project) {
            self.jobs.clear();
            self.jobs_project = Some(project.clone());
            self.jobs_last_poll = None;
        }

        let due = self
            .jobs_last_poll
            .is_none_or(|at| at.elapsed() >= JOBS_POLL_INTERVAL);
        if due && !project.as_os_str().is_empty() {
            self.jobs_last_poll = Some(Instant::now());
            return Some(UiIntent::PollJobs(project));
        }
        None
    }

    /// Turns the composer's contents into a dispatched run.
    ///
    /// Continues the open session when there is one and starts a fresh session
    /// otherwise; either way the recorded turn and the payload are built once, so
    /// they cannot drift.
    fn start_run(&mut self) {
        // The composer belongs to the session that is open. Sending while that
        // session already has a run in flight would fork its conversation, and
        // the send button is a Stop button in that state — so a stray Enter is
        // dropped rather than opening a second session beside it.
        if let Some(id) = self.selected {
            if self.run_for(id).is_some() {
                return;
            }
        }

        // Which project this send belongs to. The match below needs the same
        // answer, so it is worked out once here and handed to it rather than
        // derived twice and left to drift.
        let project = self.sending_project();

        let text = self.prompt.trim().to_string();
        if text.is_empty() && self.pending_images.is_empty() {
            return;
        }
        // The turn is built once here so the one recorded in the transcript and
        // the one sent to the worker cannot drift apart.
        let turn = UserTurn {
            text,
            images: std::mem::take(&mut self.pending_images),
        };
        if self.api_key.trim().is_empty() {
            self.show_settings = true;
            self.notice_error(
                None,
                format!("请先在「设置」里填写 API Key，或设置环境变量 {API_KEY_ENV}"),
            );
            return;
        }

        // Sending while a session is open continues *that* conversation — its
        // transcript is replayed to the model ahead of the new prompt — and the
        // new turn is appended to the session the user is looking at. With
        // nothing selected a fresh session starts, rooted at the active
        // project. Replaying a session that is already running would fork the
        // conversation, which the guard above refuses.
        let selected = self.selected;
        // Captured before the borrow of `self.sessions` below, and stamped onto
        // whichever session this send belongs to.
        let thinking = self.thinking;
        let (session_id, project, history, carried) = match self
            .sessions
            .iter_mut()
            .find(|session| Some(session.id) == selected && session.state != RunState::Running)
        {
            Some(session) => {
                // The transcript is captured before the new turn is appended:
                // `run` replays the history and then sends `prompt` as its own
                // user message, so the prompt must not appear in both.
                let history = session.to_messages();
                // The measurement the last run of this conversation reported,
                // handed back so the new run's window starts already guarded.
                let carried = session.context_measurement;
                // Recording the prompt as a step is what makes a follow-up
                // actually show up in the transcript — and survive a restart.
                session.steps.push(Step::User {
                    text: turn.text.clone(),
                    images: turn.images.clone(),
                });
                // So the sidebar marks the session as running for the whole
                // follow-up, not just for a session's first prompt.
                session.state = RunState::Running;
                // The composer's choice is authoritative and is stamped onto the
                // session here, so a level picked before this send is what both
                // this run and every later reopen of the session will use.
                session.thinking = thinking;
                (session.id, project, history, carried)
            }
            None => {
                // Rooted at the project `sending_project` resolved: the one the
                // user is looking at, or the first one, since `config.projects`
                // is never empty and a fresh install always has somewhere to run.
                let mut fresh = Session::new(project.clone());
                fresh.thinking = thinking;
                fresh.steps.push(Step::User {
                    text: turn.text.clone(),
                    images: turn.images.clone(),
                });
                let id = fresh.id;
                self.sessions.push(fresh);
                (id, project, Vec::new(), None)
            }
        };

        let run_id = self.next_run_id;
        self.next_run_id += 1;

        self.selected = Some(session_id);
        self.active.insert(
            run_id,
            ActiveRun {
                session: session_id,
            },
        );
        self.prompt.clear();
        self.pending_images.clear();
        self.stick_to_bottom = true;
        self.mark_dirty();

        match self.cmd_tx.send(Cmd::Run {
            run_id,
            prompt: turn,
            history,
            carried,
            project: PathBuf::from(project),
            thinking,
        }) {
            Ok(()) => {}
            Err(_) => {
                self.active.remove(&run_id);
                self.notice_error(Some(session_id), "agent 线程已退出，请重启程序");
            }
        }
    }

    /// Stops the run writing into the session the composer is showing.
    ///
    /// Only that session's run: with several in flight, a Stop button that
    /// cancelled whichever run the worker happened to know about would kill the
    /// wrong conversation.
    fn cancel_run(&mut self) {
        let Some(session) = self.selected else { return };
        let Some(run_id) = self.run_for(session) else {
            return;
        };
        let _ = self.cmd_tx.send(Cmd::Cancel { run_id });
    }

    /// Removes a session and its transcript from the store.
    fn delete_session(&mut self, id: Uuid) {
        self.sessions.retain(|session| session.id != id);
        // The render cache is keyed by session, so a deleted session's entries
        // would otherwise stay resident for the life of the process.
        let live: HashSet<Uuid> = self.sessions.iter().map(|session| session.id).collect();
        self.render_cache.retain_sessions(&live);
        if self.selected == Some(id) {
            self.selected = None;
        }
        self.mark_dirty();
    }

    /// Makes `project` the active one and shows it.
    ///
    /// Selecting the project's newest session is what the user is most likely
    /// after; an empty project instead leaves nothing selected, which shows the
    /// empty state — where the first prompt in it will start.
    fn open_project(&mut self, project: String) {
        self.active_project = Some(project.clone());
        let newest = self
            .sessions
            .iter()
            .rev()
            .find(|session| session.project == project)
            .map(|session| (session.id, session.thinking));
        self.selected = newest.map(|(id, _)| id);
        // Follow the conversation the project opens onto, or fall back to the
        // default when the project has no session yet.
        self.thinking = newest.and_then(|(_, thinking)| thinking);
        self.stick_to_bottom = true;
    }

    /// Picks a folder and adds it to the project list.
    ///
    /// Only the list is persisted — no session is created, so the project shows
    /// up in the sidebar immediately and the first prompt in it starts a chat
    /// there.
    fn add_project(&mut self) {
        let Some(folder) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let project = folder.display().to_string();
        if self.config.projects.contains(&project) {
            self.sidebar_error = None;
            self.open_project(project);
            return;
        }

        self.config.projects.insert(0, project.clone());
        self.open_project(project);
        self.queue_config_save(ConfigSurface::Sidebar);
    }

    /// Drops a project from the list, keeping its sessions.
    ///
    /// The transcripts are not deleted: they stay reachable under 最近 and
    /// through search, so removing a project tidies the sidebar without
    /// destroying history.
    fn remove_project(&mut self, project: &str) {
        // There must always be somewhere for a new chat to run, which is also
        // what `Config::normalize` assumes when it refills an empty list.
        if self.config.projects.len() <= 1 {
            return;
        }

        self.config
            .projects
            .retain(|candidate| candidate != project);
        if self.active_project.as_deref() == Some(project) {
            self.active_project = self.config.projects.first().cloned();
            self.selected = None;
            self.stick_to_bottom = true;
        }
        self.queue_config_save(ConfigSurface::Sidebar);
    }

    /// Persists the settings and pushes them to the worker.
    ///
    /// A failed push means the worker is gone, which is reported on the settings
    /// page rather than here.
    fn save_settings(&mut self) {
        if !self.api_key.trim().is_empty() {
            let request_id = self.next_config_request_id;
            self.next_config_request_id = self.next_config_request_id.wrapping_add(1);
            self.pending_api_key_save = Some(request_id);
            if self
                .cmd_tx
                .send(Cmd::SaveApiKey {
                    request_id,
                    api_key: SecretValue::new(self.api_key.clone()),
                })
                .is_err()
            {
                self.pending_api_key_save = None;
                self.settings_error = Some("agent 线程已退出，API Key 未保存".into());
            }
            return;
        }

        self.finish_save_settings();
    }

    fn finish_save_settings(&mut self) {
        // The tool settings are pushed to the worker too, so a new working
        // directory or a flipped guard takes effect on the next call rather than
        // needing a restart.
        if self
            .cmd_tx
            .send(Cmd::SetToolSettings(Box::new(self.config.tools.clone())))
            .is_err()
        {
            self.settings_error = Some("agent 线程已退出，设置未生效".into());
            return;
        }

        // The model settings are global worker state as well: the next run picks
        // up a new base URL, model or key without a restart, and no session
        // carries settings of its own.
        if self
            .cmd_tx
            .send(Cmd::SetLlmSettings(Box::new(LlmSettings {
                base_url: self.config.llm.base_url.clone(),
                model: self.config.llm.model.clone(),
                context: self.config.context,
                max_output_tokens: self.config.llm.max_output_tokens,
                retry_count: self.config.llm.retry_count,
                retry_forever: self.config.llm.retry_forever,
                input: self.config.llm.input.clone(),
                api_key: self.api_key.clone(),
            })))
            .is_err()
        {
            self.settings_error = Some("agent 线程已退出，设置未生效".into());
            return;
        }

        self.queue_config_save(ConfigSurface::Settings);
    }

    fn queue_config_save(&mut self, surface: ConfigSurface) {
        let request_id = self.next_config_request_id;
        self.next_config_request_id = self.next_config_request_id.wrapping_add(1);
        self.pending_config_saves.insert(request_id, surface);
        self.config_save_requests.insert(surface, request_id);
        if self
            .cmd_tx
            .send(Cmd::SaveConfig {
                request_id,
                config: Box::new(self.config.clone()),
            })
            .is_err()
        {
            self.pending_config_saves.remove(&request_id);
            self.config_save_requests.remove(&surface);
            let error = "agent 线程已退出，配置未保存".to_string();
            match surface {
                ConfigSurface::Sidebar => self.sidebar_error = Some(error),
                ConfigSurface::Settings => self.settings_error = Some(error),
            }
        }
    }

    /// Re-runs discovery and hands the result to the worker.
    ///
    /// Discovery is cheap and pure, so a change simply re-derives the whole
    /// catalogue rather than patching the one entry that moved — the catalogue
    /// is a function of the config and the marketplaces on disk, and re-deriving
    /// it is what keeps the two from drifting. The worker is told too, because
    /// every cached agent baked the old catalogue into its system prompt and
    /// tool registry.
    fn reload_plugins(&mut self) {
        let request_id = self.next_config_request_id;
        self.next_config_request_id = self.next_config_request_id.wrapping_add(1);
        self.pending_plugin_request = Some(request_id);
        if self
            .cmd_tx
            .send(Cmd::ReloadPlugins {
                request_id,
                config: Box::new(self.config.clone()),
            })
            .is_err()
        {
            self.plugins_error = Some("agent 线程已退出，插件改动未生效".into());
            self.pending_plugin_request = None;
        }
    }

    /// Re-discovers installed Wasmtime plugins without changing configuration.
    ///
    /// The worker owns filesystem access and replaces its catalogue only after
    /// discovery succeeds. Existing project runtimes are invalidated then, so
    /// the next surface or agent build reads the current component bytes.
    fn refresh_plugins(&mut self) {
        if self.pending_plugin_request.is_some() {
            return;
        }
        let request_id = self.next_config_request_id;
        self.next_config_request_id = self.next_config_request_id.wrapping_add(1);
        self.pending_plugin_request = Some(request_id);
        let projects = self.config.projects.iter().map(PathBuf::from).collect();
        if self
            .cmd_tx
            .send(Cmd::RefreshPlugins {
                request_id,
                projects,
                settings: self.config.plugins.clone(),
            })
            .is_err()
        {
            self.plugins_error = Some("agent 线程已退出，插件未刷新".into());
            self.pending_plugin_request = None;
        }
    }

    /// Opens a Wasmtime component picker and asks the worker to import it.
    ///
    /// The selected component is matched to its plugin manifest in the worker;
    /// the worker owns validation, the cache copy, and the config update.
    fn add_plugin(&mut self, scope: plugins::Scope) {
        if self.pending_plugin_request.is_some() {
            return;
        }
        let Some(component_path) = rfd::FileDialog::new()
            .set_title("选择 Wasmtime 插件组件")
            .add_filter("Wasmtime Component", &["wasm"])
            .pick_file()
        else {
            return;
        };
        let request_id = self.next_config_request_id;
        self.next_config_request_id = self.next_config_request_id.wrapping_add(1);
        self.pending_plugin_request = Some(request_id);
        self.plugins_error = None;
        if self
            .cmd_tx
            .send(Cmd::InstallPlugin {
                request_id,
                component_path,
                scope,
                config: Box::new(self.config.clone()),
            })
            .is_err()
        {
            self.plugins_error = Some("agent 线程已退出，插件未添加".into());
            self.pending_plugin_request = None;
        }
    }

    /// Turns one plugin on or off, persists it, and reloads.
    ///
    /// Normalised before the save, because switching a plugin off also strikes
    /// it from every project list — so the file written here is the repaired
    /// one, not the one the user typed.
    fn set_plugin_enabled(&mut self, id: &str, scope: &plugins::Scope, enabled: bool) {
        if !enabled {
            self.close_plugin_surface();
        }
        match scope {
            plugins::Scope::Global => self.config.plugins.set_enabled(id, enabled),
            plugins::Scope::Project(project) => {
                self.config
                    .plugins
                    .set_project_enabled(project, id, enabled);
            }
        }
        self.config.plugins.normalize();
        self.reload_plugins();
    }

    /// Removes a plugin's installed copy and switches it off.
    ///
    /// Only the copy Codex cached is deleted. A bundled marketplace's copy
    /// ships with Codex and a local source is a developer's working tree, so
    /// removing either would destroy something the user never asked us to
    /// touch; those are switched off and left in place, with the path reported
    /// so the user can remove it themselves.
    fn uninstall_plugin(&mut self, id: &str, scope: &plugins::Scope) {
        self.close_plugin_surface();
        match scope {
            plugins::Scope::Global => self.config.plugins.set_enabled(id, false),
            plugins::Scope::Project(project) => {
                self.config.plugins.set_project_enabled(project, id, false);
            }
        }
        self.config.plugins.normalize();
        let request_id = self.next_config_request_id;
        self.next_config_request_id = self.next_config_request_id.wrapping_add(1);
        self.pending_plugin_request = Some(request_id);
        if self
            .cmd_tx
            .send(Cmd::UninstallPlugin {
                request_id,
                id: id.to_string(),
                scope: scope.clone(),
                config: Box::new(self.config.clone()),
            })
            .is_err()
        {
            self.pending_plugin_request = None;
            self.plugins_error = Some("agent 线程已退出，插件未卸载".into());
        }
    }

    /// Applies controller inputs after the renderer releases its widget borrows.
    ///
    /// No egui value is required here. GUI-only effects such as closing the
    /// viewport or copying text are returned to the renderer after state and
    /// IPC side effects have been applied.
    fn apply_intents(&mut self, intents: Vec<UiIntent>) -> UiEffects {
        let mut effects = UiEffects::default();
        for intent in intents {
            match intent {
                UiIntent::OpenPluginSurface {
                    plugin_id,
                    surface_id,
                } => self.open_plugin_surface(plugin_id, surface_id),
                UiIntent::PluginUiAction(action) => self.plugin_ui_action(action),
                UiIntent::ClosePluginSurface => self.close_plugin_surface(),
                UiIntent::AddPlugin { scope } => self.add_plugin(scope),
                UiIntent::Quit => effects.close = true,
                UiIntent::NewSession => self.new_session(),
                UiIntent::SelectSession(id) => {
                    self.selected = Some(id);
                    if let Some(session) = self.sessions.iter().find(|session| session.id == id) {
                        self.active_project = Some(session.project.clone());
                        self.thinking = session.thinking;
                    }
                    self.stick_to_bottom = true;
                }
                UiIntent::SelectProject(project) => self.open_project(project),
                UiIntent::DeleteSession(id) => self.delete_session(id),
                UiIntent::RemoveProject(project) => self.remove_project(&project),
                UiIntent::OpenSettings => {
                    self.show_settings = true;
                    self.context_limit_text = self.config.context.context_limit.to_string();
                    self.max_output_tokens_text = self
                        .config
                        .llm
                        .max_output_tokens
                        .map(|tokens| tokens.to_string())
                        .unwrap_or_default();
                }
                UiIntent::OpenAbout => self.show_about = true,
                UiIntent::OpenPlugins => {
                    if !self.show_plugins {
                        self.show_plugins = true;
                        self.refresh_plugins();
                    }
                }
                UiIntent::RefreshPlugins => self.refresh_plugins(),
                UiIntent::SetTheme(theme) => {
                    if self.config.theme != theme {
                        self.config.theme = theme;
                        self.save_settings();
                        effects.theme = Some(theme);
                    }
                }
                UiIntent::SetSidebarVisible(visible) => self.show_sidebar = visible,
                UiIntent::CopyTranscript => {
                    effects.clipboard_text = self.selected_session().map(Session::as_text);
                }
                UiIntent::SetPluginEnabled { id, scope, enabled } => {
                    self.set_plugin_enabled(&id, &scope, enabled);
                }
                UiIntent::UninstallPlugin { id, scope } => self.uninstall_plugin(&id, &scope),
                UiIntent::AddProject => self.add_project(),
                UiIntent::RemovePendingImage(id) => {
                    self.pending_images.retain(|image| image.id != id);
                }
                UiIntent::SaveSettings => self.save_settings(),
                UiIntent::CancelRun => self.cancel_run(),
                UiIntent::KillJob(job_id) => self.kill_job(&job_id),
                UiIntent::PollJobs(project) => {
                    let _ = self.cmd_tx.send(Cmd::ListJobs { project });
                }
                UiIntent::ToggleSubagent(job_id) => {
                    let current = self.open_subagent.as_deref();
                    self.open_subagent = (current != Some(job_id.as_str())).then_some(job_id);
                }
                UiIntent::CloseSubagent => self.open_subagent = None,
                UiIntent::SendPrompt => self.start_run(),
            }
        }
        self.reconcile_plugin_surface();
        effects.repaint_after =
            self.jobs.iter().any(|job| !job.is_settled()) || !self.active.is_empty();
        effects
    }

    /// Ctrl+V when the clipboard holds an image — the one paste egui-winit
    /// cannot serve.
    ///
    /// egui-winit owns Ctrl+V and forwards egui only the clipboard's *text*
    /// format. With an image on the clipboard — a screenshot, or a file copied
    /// in Explorer / Finder — that read fails, and upstream then swallows the
    /// key and pushes nothing, which is the `arboard paste error` in the log.
    /// The vendored copy of egui-winit (see `[patch.crates-io]` in the root
    /// `Cargo.toml`) drops that swallow, so the press arrives here as an
    /// ordinary `Event::Key` — carrying the modifiers that were actually held —
    /// and the image is read from this side. Without that patch this method
    /// never fires at all.
    ///
    /// Matching the *press* is the whole point. The release carries whatever
    /// modifiers are held at that moment, so letting go of Ctrl before V — or
    /// the two together, in whichever order the OS reports them — used to leave
    /// the release with no `command` and lose the chord. `command` is the same
    /// test egui-winit's own `is_paste_command` uses, so the press that arrives
    /// here is exactly the one it would otherwise have swallowed.
    ///
    /// `repeat` is filtered out because the swallow that used to sit in front of
    /// this also absorbed the auto-repeat: without the filter, holding the chord
    /// down would queue the image once per repeat. egui fills the flag in from
    /// its own `keys_down` set, so `false` here means "the press that started
    /// this chord" and not "winit said not-a-repeat".
    fn intake_pasted_images(&mut self, ctx: &egui::Context) {
        let chord = ctx.input(|input| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } if modifiers.command
                )
            })
        });
        if !chord {
            return;
        }
        // A clipboard that also carries text was already served by egui-winit
        // on the same press. Some sources put both — a spreadsheet cell, a
        // "copy image address", a file copied by an app that also exports its
        // path — and handling it here as well would attach the image *and*
        // paste the text. Text wins; a clipboard with nothing but image data (a
        // screenshot, "copy image") is exactly the case egui-winit dropped.
        if clipboard_has_text() {
            return;
        }
        // The chord fired, so the user meant to paste *something*: take the
        // image when there is one, and say so in the conversation when there is
        // not — the image problems belong where the conversation is.
        if let Some(source) = clipboard_image() {
            self.intake_image_source(source);
        } else {
            self.notice_error(None, "剪贴板里没有图片或图片文件");
        }
    }

    /// Files dragged onto the window. The drop arrives as paths on every
    /// native platform; anything that is not a readable image is skipped.
    fn intake_dropped_files(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        if !paths.is_empty() {
            self.intake_image_source(ClipboardImage::Paths(paths));
        }
    }

    /// Admits one batch of images into the composer, by path or by bytes.
    ///
    /// The Harness shape: check the budget, refuse with one clear message, and
    /// only then store. Nothing is half-admitted — a refusal leaves the strip
    /// exactly as it was.
    fn intake_image_source(&mut self, source: ClipboardImage) {
        // The model declares its input modalities, and a text-only one cannot
        // take a picture. Refusing here — the way the Harness refuses with
        // `does not support image input` — beats queueing an image that would
        // later fail at the provider, mid-turn.
        if !self.config.llm.supports_images() {
            self.notice_error(None, "当前模型未开启图片输入；请在「设置」里勾选「图片」");
            return;
        }
        let total: usize = self.pending_images.iter().map(|image| image.bytes).sum();
        let result = match source {
            ClipboardImage::Paths(paths) => attachments::store_from_paths(&paths, total),
            ClipboardImage::Bytes { bytes, name } => attachments::store_from_bytes(&bytes, name),
        };
        match result {
            Ok(added) => self.pending_images.extend(added),
            Err(error) => {
                tracing::warn!(%error, "failed to store an attached image");
                self.notice_error(None, error.to_string());
            }
        }
    }

    /// Asks the worker to stop one background job.
    ///
    /// Nothing is reported back: the job settles as killed once its work
    /// actually stops, and the next poll of the list shows that. A send that
    /// fails means the worker is gone, which the next run reports anyway.
    fn kill_job(&self, job_id: &str) {
        let Some(project) = self.jobs_project.clone() else {
            return;
        };
        let _ = self.cmd_tx.send(Cmd::KillJob {
            project,
            job_id: job_id.to_string(),
        });
    }
}

fn event_run_id(event: &Event) -> RunId {
    match event {
        Event::AssistantDelta { run_id, .. }
        | Event::AssistantTurnReset { run_id }
        | Event::ReasoningDelta { run_id, .. }
        | Event::AssistantDone { run_id, .. }
        | Event::Notice { run_id, .. }
        | Event::ToolStarted { run_id, .. }
        | Event::ToolFinished { run_id, .. }
        | Event::UsageSampled { run_id, .. }
        | Event::CompactionStarted { run_id, .. }
        | Event::Compacted { run_id, .. }
        | Event::RunFinished { run_id, .. }
        | Event::RunFailed { run_id, .. } => *run_id,
        // Handled before this function is reached (see `App::apply`); listed so
        // the match stays exhaustive. Jobs and sub-agent messages are not
        // scoped to a run.
        Event::Jobs { .. }
        | Event::SubagentStarted { .. }
        | Event::Subagent { .. }
        | Event::SessionsSaved { .. }
        | Event::SessionSaveFailed { .. }
        | Event::ApiKeySaved { .. }
        | Event::ApiKeySaveFailed { .. }
        | Event::ConfigSaved { .. }
        | Event::ConfigSaveFailed { .. }
        | Event::PluginsUpdated { .. }
        | Event::PluginInstalled { .. }
        | Event::PluginUiUpdated { .. }
        | Event::PluginUiClosed { .. }
        | Event::PluginUiFailed { .. }
        | Event::PluginOperationFailed { .. }
        | Event::MessageRendered { .. }
        | Event::ToolRendered { .. }
        | Event::RenderFailed { .. }
        | Event::RendererAvailability { .. } => 0,
    }
}

/// The one-line status a job row shows: a running job's elapsed time, else the
/// settled state (with the process's exit code when there is one).
fn job_status_text(job: &JobView) -> String {
    match job.state {
        JobState::Running => format!("运行中 {}", format_elapsed(job.started_ms)),
        JobState::Completed => match job.exit_code {
            Some(0) | None => JobState::Completed.label().to_string(),
            Some(code) => format!("{}（退出码 {code}）", JobState::Completed.label()),
        },
        other => other.label().to_string(),
    }
}

/// A running job's age, from its Unix-millisecond start to now, in the shortest
/// unit that fits: seconds under a minute, then minutes, then hours.
fn format_elapsed(started_ms: u128) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(started_ms);
    let secs = now_ms.saturating_sub(started_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// What a thumbnail or a chip says when hovered.
fn attachment_caption(image: &ImageRef) -> String {
    let label = image.name.clone().unwrap_or_else(|| "图片".into());
    format!(
        "{label}\n{}×{} · {} KB",
        image.width,
        image.height,
        image.bytes / 1024
    )
}

/// The last path component, for the project list.
///
/// Splits on both separators instead of using `Path::file_name`, so a session
/// written on Windows still shows a bare name when it is opened on a Unix host.
fn project_name(project: &str) -> String {
    project
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or(project)
        .to_string()
}

fn shorten(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max).collect();
    format!("{head}…")
}

/// A pasted or dropped image, before it is admitted to the store.
///
/// Two shapes because the clipboard offers two: a list of file paths (a file
/// copied in Explorer / Finder) and raw bitmap pixels (a screenshot, or "copy
/// image"). A drop is always the first shape — the OS hands over paths.
enum ClipboardImage {
    Paths(Vec<PathBuf>),
    Bytes {
        bytes: Vec<u8>,
        name: Option<String>,
    },
}

/// The clipboard as an image, if it holds one.
///
/// Two shapes, in the order the platforms offer them: copied *files* arrive as
/// a path list (Windows `CF_HDROP`, macOS `NSFilenamesPboard`, Linux
/// `text/uri-list`) and are read from disk; a screenshot or "copy image" has no
/// path at all and arrives as raw bitmap pixels, which arboard hands over as
/// straight RGBA8 and [`image_ops::encode_png`] turns into the one shape the
/// rest of the intake understands. `None` means the clipboard holds no image —
/// the caller then leaves the key to egui's own text paste.
///
/// The file list is tried before the bitmap on purpose: a copied file keeps its
/// name and is read straight from disk, while the bitmap a file also puts on
/// the clipboard would be a lossy re-encode of the same picture.
fn clipboard_image() -> Option<ClipboardImage> {
    let mut clipboard = arboard::Clipboard::new().ok()?;

    if let Ok(paths) = clipboard.get().file_list() {
        if !paths.is_empty() {
            return Some(ClipboardImage::Paths(paths));
        }
    }

    let image = clipboard.get_image().ok()?;
    let raster = image_ops::Raster {
        width: u32::try_from(image.width).ok()?,
        height: u32::try_from(image.height).ok()?,
        rgba: image.bytes.into_owned(),
    };
    let bytes = image_ops::encode_png(&raster).ok()?;
    Some(ClipboardImage::Bytes { bytes, name: None })
}

/// Whether the clipboard holds plain text.
///
/// egui-winit reads the text format for every Ctrl+V, so text is the one case
/// it already serves. Distinguishing it here is what keeps a file copy from
/// being handled twice — see [`App::intake_pasted_images`].
fn clipboard_has_text() -> bool {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.get_text())
        .map(|text| !text.is_empty())
        .unwrap_or(false)
}

/// What a queued image is called in the composer strip.
///
/// A pasted file keeps its name; a pasted bitmap has none, so it is described
/// by the only things known about it.
fn chip_label(image: &ImageRef) -> String {
    match &image.name {
        Some(name) => name.clone(),
        None if image.width > 0 && image.height > 0 => format!("{}×{}", image.width, image.height),
        None => format!("{} KB", image.bytes / 1024),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests below need these: the production code in this module no
    // longer touches egui's layout or the theme's palette.
    use egui::Align;
    use serde_json::Value;

    use crate::ipc::AuditOutcome;
    use crate::renderer::protocol::{ColorRole, Node, RenderKey, RenderKind, Run};
    use crate::theme::{self, ThemeChoice};

    /// The bubble must hug the *message column's* right edge, not the panel's.
    ///
    /// The transcript and the composer share one column, so the gap between a
    /// right-hugging bubble and the replies it answers is exactly
    /// `BUBBLE_EDGE_GAP`, and the column itself starts where the composer does.
    /// Anchor the bubble to the panel instead and that gap becomes
    /// `BUBBLE_EDGE_GAP` plus the column's own margin — a sixth of the window
    /// of empty space, which is what the bubble looked like it was floating in.
    #[test]
    fn the_user_bubble_hugs_the_message_column() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        let mut session = Session::new("project");
        session.steps.push(Step::User {
            text: "继续".into(),
            images: Vec::new(),
        });
        session.steps.push(Step::Notice {
            text: "https://tierflow.cn/v1/chat/completions returned 429 Too Many Requests".into(),
        });
        session.steps.push(Step::Assistant {
            text: "好的，我继续。".into(),
        });
        let session_id = session.id;
        app.sessions.push(session);
        app.selected = Some(session_id);

        let ctx = egui::Context::default();
        let p = theme::palette(ThemeChoice::Dark);
        let panel = 1000.0;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(panel, 800.0),
            )),
            ..Default::default()
        };
        let mut resources = GuiResources::default();
        let mut output = ctx.run_ui(input, |ui| app.draw_transcript(ui, &p, &mut resources));

        // The bubble is the rect painted in the user's fill; the reply is
        // plain text, so the column's left edge is where its text starts.
        let bubble = output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Rect(rect) if rect.fill == p.bubble_user => Some(rect.rect),
                _ => None,
            })
            .expect("the user's bubble should have been painted");
        let text_left = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some(text.pos.x),
                _ => None,
            })
            .fold(f32::INFINITY, f32::min);

        // The column the composer sits in, to the pixel.
        let column = (panel - 2.0 * ui::CHAT_MARGIN_X).min(ui::COMPOSER_MAX_WIDTH);
        let column_left = (panel - column) / 2.0;

        assert!(
            (text_left - column_left).abs() < 1.0,
            "the transcript's column should start where the composer's does, \
             left = {text_left} (wanted {column_left})"
        );
        assert!(
            (bubble.right() - (column_left + column - ui::BUBBLE_EDGE_GAP)).abs() < 1.0,
            "the bubble should hug the column's right edge minus the gap, right = {}",
            bubble.right()
        );
        assert!(
            bubble.width() < column / 4.0,
            "a two-word bubble should still shrink to its text, drew {} px",
            bubble.width()
        );

        output.textures_delta.clear();
    }

    /// A short user bubble must shrink to its text and hug the band's right
    /// edge minus the edge gap; a long one must still cap at the band.
    ///
    /// egui's own layouts cannot do the first part (a frame on a right-to-left
    /// row is handed the whole row), which is why `draw_bubble` measures first.
    #[test]
    fn user_bubble_shrinks_to_content_and_hugs_the_right() {
        let ctx = egui::Context::default();
        let p = theme::palette(ThemeChoice::Dark);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1000.0, 800.0),
            )),
            ..Default::default()
        };
        let band = 576.0;
        let usable = band - ui::BUBBLE_EDGE_GAP;

        let mut output = ctx.run_ui(input, |ui| {
            ui.set_width(1000.0);

            let short = ui::draw_bubble(
                ui,
                &p,
                ui::Message {
                    text: "继续",
                    nodes: None,
                    salt: (Uuid::nil(), 0),
                },
                p.bubble_user,
                Align::Max,
                band,
            );
            assert!(
                short.width() < usable / 4.0,
                "a two-word bubble should shrink to its text, drew {} px",
                short.width()
            );
            assert!(
                (short.right() - usable).abs() < 1.0,
                "the bubble should hug the band's right edge, right = {}",
                short.right()
            );

            let long = "a rather long message that certainly exceeds the band width \
                        so it has to wrap somewhere along the way, and keeps going \
                        to make that certain";
            let wrapped = ui::draw_bubble(
                ui,
                &p,
                ui::Message {
                    text: long,
                    nodes: None,
                    salt: (Uuid::nil(), 1),
                },
                p.bubble_user,
                Align::Max,
                band,
            );
            assert!(
                (wrapped.width() - usable).abs() <= 1.0,
                "a long bubble should cap at the band, drew {} px",
                wrapped.width()
            );
        });
        output.textures_delta.clear();
    }

    /// A home directory no test should ever read from.
    fn test_home() -> PathBuf {
        PathBuf::from("/no-such-home-in-a-test")
    }

    /// Paths pointing nowhere real, so a test never touches the user's state.
    fn test_paths() -> Paths {
        Paths { config_path: None }
    }

    /// A catalogue with nothing in it.
    ///
    /// Discovery is pointed at a home that does not exist, so it finds nothing
    /// and reports nothing: these tests are about the window, not the plugins.
    fn no_plugins() -> Arc<PluginCatalogue> {
        Arc::new(plugins::discover(
            &test_home(),
            &[],
            &plugins::PluginSettings::default(),
        ))
    }

    /// A failed render is remembered, so the item falls back to the native
    /// parser instead of asking the renderer for the same input again.
    #[test]
    fn a_failed_render_is_remembered_and_never_served_from_the_cache() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        let key = RenderKey::Message {
            session: Uuid::new_v4(),
            step: 0,
        };
        let fingerprint = render_cache::fingerprint("hello");
        app.render_cache.begin(&key, 1, fingerprint);
        app.apply(Event::RenderFailed {
            key: key.clone(),
            revision: 1,
            kind: RenderKind::Message,
            message: "boom".into(),
        });

        assert_eq!(
            app.render_cache.get(&key).expect("entry").status,
            render_cache::RenderStatus::Failed,
        );
        assert!(
            app.render_cache.rendered(&key, fingerprint).is_none(),
            "a failed render must not be served from the cache",
        );
    }

    /// A render result lands in the cache under its key; the draw loop picks it
    /// up by key rather than through the run that produced the message.
    #[test]
    fn a_rendered_message_response_is_cached_by_key() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        let key = RenderKey::Message {
            session: Uuid::new_v4(),
            step: 0,
        };
        let fingerprint = render_cache::fingerprint("hi");
        let nodes = Arc::new(vec![Node::Text {
            runs: vec![Run::text("hi", 14.0, ColorRole::Text)],
            wrap: true,
            selectable: true,
        }]);
        app.render_cache.begin(&key, 1, fingerprint);
        app.apply(Event::MessageRendered {
            key: key.clone(),
            revision: 1,
            nodes,
        });

        let cached = app
            .render_cache
            .rendered(&key, fingerprint)
            .expect("the response should be cached");
        assert_eq!(cached.len(), 1);
    }

    /// Availability is tracked both ways, and coming back clears the cache so a
    /// failed render is retried instead of staying on the plain-text fallback.
    #[test]
    fn renderer_availability_is_remembered_and_recovery_clears_the_cache() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        let key = RenderKey::Message {
            session: Uuid::new_v4(),
            step: 0,
        };
        let fingerprint = render_cache::fingerprint("hi");
        app.render_cache.begin(&key, 1, fingerprint);
        app.render_cache.fail(&key, 1);

        assert!(app.renderer_available, "the renderer starts assumed loaded");
        app.apply(Event::RendererAvailability { available: false });
        assert!(!app.renderer_available);
        assert!(
            app.render_cache.get(&key).is_some(),
            "going unavailable keeps what was already cached",
        );

        app.apply(Event::RendererAvailability { available: true });
        assert!(app.renderer_available);
        assert!(
            app.render_cache.get(&key).is_none(),
            "coming back drops the failed entries so they are requested again",
        );
    }

    #[test]
    fn token_counts_format_as_k_and_m() {
        // Under a thousand stays exact; the shorthand is for the big figures
        // the gauge actually shows.
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(12_345), "12.3k");
        assert_eq!(format_tokens(131_072), "131.1k");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(1_234_567), "1.2M");
    }

    #[test]
    fn token_counts_parse_from_the_shorthands_the_field_promises() {
        // Plain integers stay tokens; k and M are the 1024-based powers a
        // context window is conventionally quoted in, and the KiB spellings
        // read as the same numbers.
        assert_eq!(parse_token_count("8192"), Some(8_192));
        assert_eq!(parse_token_count("1M"), Some(1_048_576));
        assert_eq!(parse_token_count("8k"), Some(8_192));
        assert_eq!(parse_token_count("128K"), Some(131_072));
        assert_eq!(parse_token_count("8KiB"), Some(8_192));
        assert_eq!(parse_token_count("2MiB"), Some(2_097_152));
        assert_eq!(parse_token_count(" 1m "), Some(1_048_576));
    }

    #[test]
    fn a_non_number_leaves_the_token_count_unparsed() {
        // A unit the field does not promise is a typo, not a zero — leaving the
        // value untouched is the honest reading of input the parser cannot own.
        assert_eq!(parse_token_count(""), None);
        assert_eq!(parse_token_count("abc"), None);
        assert_eq!(parse_token_count("8GB"), None);
        assert_eq!(parse_token_count("1.5M"), None);
    }

    #[test]
    fn shorten_flattens_and_caps() {
        assert_eq!(shorten("a\nb", 10), "a b");
        assert_eq!(shorten("abcdef", 3), "abc…");
    }

    #[test]
    fn shorten_counts_characters_not_bytes() {
        // Three Chinese characters are nine bytes; a byte-based cap would cut
        // this mid-character.
        assert_eq!(shorten("中文字", 2), "中文…");
    }

    #[test]
    fn a_project_name_is_its_last_component() {
        assert_eq!(project_name("/home/me/code/thing"), "thing");
        assert_eq!(project_name(r"C:\Users\me\thing"), "thing");
    }

    #[test]
    fn a_project_with_no_file_name_shows_the_whole_path() {
        assert_eq!(project_name("/"), "/");
    }

    #[test]
    fn every_event_reports_the_run_it_belongs_to() {
        let events = [
            Event::AssistantDelta {
                run_id: 7,
                text: String::new(),
            },
            Event::AssistantTurnReset { run_id: 7 },
            Event::ReasoningDelta {
                run_id: 7,
                text: String::new(),
            },
            Event::AssistantDone {
                run_id: 7,
                content: String::new(),
            },
            Event::ToolStarted {
                run_id: 7,
                call_id: String::new(),
                name: String::new(),
                arguments: Value::Null,
                raw_arguments: String::new(),
            },
            Event::ToolFinished {
                run_id: 7,
                call_id: String::new(),
                outcome: AuditOutcome::Executed,
                output: String::new(),
                images: Vec::new(),
                hunks: Vec::new(),
                duration_ms: 0,
            },
            Event::UsageSampled {
                run_id: 7,
                measurement: None,
                usage: None,
            },
            Event::RunFinished {
                run_id: 7,
                usage: None,
                measurement: None,
            },
            Event::RunFailed {
                run_id: 7,
                message: String::new(),
            },
        ];
        for event in &events {
            assert_eq!(event_run_id(event), 7);
        }
    }

    #[test]
    fn a_stream_retry_removes_partial_output_without_finishing_the_run() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        app.prompt = "new request".into();
        app.start_run();
        let session_id = app.selected.expect("the run opened a session");
        let run_id = app.run_for(session_id).expect("the run is active");

        app.apply(Event::ReasoningDelta {
            run_id,
            text: "partial thought".into(),
        });
        app.apply(Event::AssistantDelta {
            run_id,
            text: "partial answer".into(),
        });
        app.apply(Event::AssistantTurnReset { run_id });

        let session = app.session(session_id).expect("the session remains");
        assert_eq!(session.steps.len(), 1, "only the user's prompt remains");
        assert!(matches!(
            &session.steps[0],
            Step::User { text, .. } if text == "new request"
        ));
        assert!(
            app.run_for(session_id).is_some(),
            "a retry reset is not a terminal event"
        );
    }

    #[test]
    fn saving_settings_sends_the_retry_count_to_the_worker() {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        config.llm.retry_count = 6;
        config.llm.retry_forever = true;
        let mut app = App::new(
            cmd_tx,
            event_rx,
            config,
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        app.save_settings();

        let key_request = match cmd_rx.try_recv() {
            Ok(Cmd::SaveApiKey {
                request_id,
                api_key,
            }) => {
                assert_eq!(
                    api_key.expose(),
                    "key",
                    "the credential is sent to the worker"
                );
                request_id
            }
            other => panic!("expected an API key save, got {other:?}"),
        };
        app.apply(Event::ApiKeySaved {
            request_id: key_request,
        });
        assert!(
            matches!(cmd_rx.try_recv(), Ok(Cmd::SetToolSettings(_))),
            "tool settings are sent after the credential is stored"
        );
        match cmd_rx.try_recv() {
            Ok(Cmd::SetLlmSettings(settings)) => {
                assert_eq!(
                    settings.retry_count, 6,
                    "the worker receives the chosen value"
                );
                assert!(
                    settings.retry_forever,
                    "the worker receives the unlimited retry mode"
                );
            }
            other => panic!("expected model settings, got {other:?}"),
        }
        let (request_id, saved) = match cmd_rx.try_recv() {
            Ok(Cmd::SaveConfig { request_id, config }) => (request_id, config),
            other => panic!("expected a config save, got {other:?}"),
        };
        assert_eq!(saved.llm.retry_count, 6, "the chosen value is persisted");
        assert!(saved.llm.retry_forever, "the retry mode is persisted");

        app.apply(Event::ConfigSaved { request_id });
        assert!(
            app.settings_error.is_none(),
            "the settings surface clears after the worker confirms the save"
        );
    }

    #[test]
    fn a_mid_run_usage_sample_updates_the_gauge_immediately() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        app.prompt = "第一条".into();
        app.start_run();
        let session_id = app.selected.expect("the run opened a session");
        let run_id = app.run_for(session_id).expect("the run is registered");

        // The sample lands mid-run; the gauge reads the session, so it must
        // move the moment the event is folded in — without waiting for a
        // terminal event.
        event_tx
            .send(Event::UsageSampled {
                run_id,
                measurement: Some((131_072, 9)),
                usage: None,
            })
            .expect("the app is still listening");
        app.poll();

        assert_eq!(
            app.session(session_id)
                .expect("the session is still there")
                .context_measurement,
            Some((131_072, 9)),
            "the gauge's source must carry the sample right away"
        );
        assert!(
            app.run_for(session_id).is_some(),
            "a sample is not a terminal event; the run stays in flight"
        );

        // The reset after a compaction clears it again.
        event_tx
            .send(Event::UsageSampled {
                run_id,
                measurement: None,
                usage: None,
            })
            .expect("the app is still listening");
        app.poll();
        assert_eq!(
            app.session(session_id)
                .expect("the session is still there")
                .context_measurement,
            None,
            "a compaction's reset must clear the stale figure"
        );
    }

    #[test]
    fn a_usage_sample_folds_cache_counters_into_the_session() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        app.prompt = "第一条".into();
        app.start_run();
        let session_id = app.selected.expect("the run opened a session");
        let run_id = app.run_for(session_id).expect("the run is registered");

        event_tx
            .send(Event::UsageSampled {
                run_id,
                measurement: Some((200, 4)),
                usage: Some(crate::llm::Usage {
                    prompt_tokens: Some(200),
                    prompt_cache_hit_tokens: Some(150),
                    ..Default::default()
                }),
            })
            .expect("the app is still listening");
        app.poll();

        let session = app.session(session_id).expect("the session is still there");
        assert_eq!(
            session.cache_hit_rate(),
            Some(0.75),
            "the sample's cache figures must fold into the session totals"
        );
        assert_eq!(
            session
                .usage
                .as_ref()
                .and_then(|usage| usage.cache_hit_rate()),
            Some(0.75),
            "the most recent request's usage is kept for the last-request rate"
        );
    }

    #[test]
    fn a_jobs_reply_fills_the_list_only_for_the_project_it_names() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );
        let project = PathBuf::from("/work");
        app.jobs_project = Some(project.clone());

        event_tx
            .send(Event::Jobs {
                project: project.clone(),
                jobs: vec![job_view("bash-1", "bash", JobState::Running)],
            })
            .expect("the app is still listening");
        app.poll();
        assert_eq!(app.jobs.len(), 1, "the reply is folded in");
        assert_eq!(app.jobs[0].id, "bash-1");

        // A reply for another project must be dropped, so a late answer that
        // arrives after the user switched conversations cannot repaint the
        // wrong project's list.
        event_tx
            .send(Event::Jobs {
                project: PathBuf::from("/elsewhere"),
                jobs: vec![job_view("bash-2", "bash", JobState::Running)],
            })
            .expect("the app is still listening");
        app.poll();
        assert_eq!(app.jobs.len(), 1);
        assert_eq!(
            app.jobs[0].id, "bash-1",
            "another project's reply is ignored"
        );
    }

    #[test]
    fn a_subagents_events_fill_only_its_own_transcript() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        let from = |job_id: &str, event: Event| Event::Subagent {
            job_id: job_id.to_string(),
            event: Box::new(event),
        };

        event_tx
            .send(Event::SubagentStarted {
                job_id: "subagent-1".into(),
                agent: "figma-implementation-agent".into(),
                prompt: "do the thing".into(),
            })
            .expect("the app is still listening");
        event_tx
            .send(from(
                "subagent-1",
                Event::AssistantDelta {
                    run_id: 0,
                    text: "working".into(),
                },
            ))
            .expect("the app is still listening");
        event_tx
            .send(from(
                "subagent-1",
                Event::AssistantDone {
                    run_id: 0,
                    content: "working".into(),
                },
            ))
            .expect("the app is still listening");
        // A second sub-agent's event must not land in the first's transcript.
        event_tx
            .send(from(
                "subagent-2",
                Event::AssistantDelta {
                    run_id: 0,
                    text: "elsewhere".into(),
                },
            ))
            .expect("the app is still listening");
        app.poll();

        assert_eq!(app.subagent_runs.len(), 2, "one transcript per job");

        let first = app
            .subagent_runs
            .iter()
            .find(|run| run.job_id == "subagent-1")
            .expect("the first sub-agent's transcript");
        assert_eq!(first.agent, "figma-implementation-agent");
        assert!(
            matches!(&first.steps[0], Step::User { text, .. } if text == "do the thing"),
            "the brief opens the transcript: {:?}",
            first.steps
        );
        assert_eq!(
            first.steps.len(),
            2,
            "the streamed delta and the completed turn are one bubble: {:?}",
            first.steps
        );
        assert!(
            matches!(&first.steps[1], Step::Assistant { text } if text == "working"),
            "{:?}",
            first.steps
        );

        let second = app
            .subagent_runs
            .iter()
            .find(|run| run.job_id == "subagent-2")
            .expect("the second sub-agent's transcript");
        assert_eq!(second.steps.len(), 1, "it saw only its own event");

        assert!(
            app.sessions.is_empty(),
            "a sub-agent's events must never reach a session"
        );
    }

    #[test]
    fn past_the_cap_the_oldest_transcript_is_dropped() {
        // A sub-agent is transcribed whether or not its window is open, so the
        // list has to be bounded: a window left up for days would otherwise
        // hold one transcript per delegation ever made.
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        for n in 0..=(MAX_SUBAGENT_RUNS + 1) {
            event_tx
                .send(Event::SubagentStarted {
                    job_id: format!("subagent-{n}"),
                    agent: "a".into(),
                    prompt: "p".into(),
                })
                .expect("the app is still listening");
        }
        app.poll();

        assert_eq!(app.subagent_runs.len(), MAX_SUBAGENT_RUNS);
        assert!(
            !app.subagent_runs
                .iter()
                .any(|run| run.job_id == "subagent-0"),
            "the oldest went first"
        );
        assert!(
            app.subagent_runs
                .iter()
                .any(|run| run.job_id == format!("subagent-{}", MAX_SUBAGENT_RUNS + 1)),
            "the newest is always kept"
        );
    }

    #[test]
    fn the_open_transcript_is_never_the_one_dropped() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        event_tx
            .send(Event::SubagentStarted {
                job_id: "subagent-0".into(),
                agent: "a".into(),
                prompt: "p".into(),
            })
            .expect("the app is still listening");
        app.poll();
        // The user is reading the oldest one when the list fills up.
        app.open_subagent = Some("subagent-0".into());

        for n in 1..=(MAX_SUBAGENT_RUNS + 1) {
            event_tx
                .send(Event::SubagentStarted {
                    job_id: format!("subagent-{n}"),
                    agent: "a".into(),
                    prompt: "p".into(),
                })
                .expect("the app is still listening");
        }
        app.poll();

        assert!(
            app.subagent_runs
                .iter()
                .any(|run| run.job_id == "subagent-0"),
            "a window on screen must not lose its transcript"
        );
    }

    #[test]
    fn sending_without_an_api_key_reports_the_error_in_the_conversation() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            String::new(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        let session = Session::new("project");
        let session_id = session.id;
        app.sessions.push(session);
        app.selected = Some(session_id);

        app.prompt = "你好".into();
        app.start_run();

        assert!(
            app.show_settings,
            "the settings window opens for a missing key"
        );
        let steps = &app
            .session(session_id)
            .expect("the session is still there")
            .steps;
        assert!(
            matches!(steps.last(), Some(Step::Notice { text }) if text.contains("API Key")),
            "the send-time error must land in the transcript, got: {steps:?}"
        );
    }

    #[test]
    fn a_dead_worker_reports_the_error_in_the_conversation() {
        // The command channel is closed, so the send cannot reach the worker.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(cmd_rx);
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        app.prompt = "你好".into();
        app.start_run();

        let session_id = app.selected.expect("the run opened a session");
        let steps = &app
            .session(session_id)
            .expect("the session is still there")
            .steps;
        assert!(
            matches!(steps.last(), Some(Step::Notice { text }) if text.contains("agent 线程")),
            "the send-time error must land in the transcript, got: {steps:?}"
        );
        assert!(
            app.run_for(session_id).is_none(),
            "a run that never reached the worker is retired"
        );
    }

    /// A `JobView` for the list tests, with the fields a row reads.
    fn job_view(id: &str, kind: &str, state: JobState) -> JobView {
        JobView {
            id: id.to_string(),
            kind: kind.to_string(),
            label: format!("{id} label"),
            state,
            detail: None,
            exit_code: None,
            started_ms: 0,
        }
    }

    #[test]
    fn a_follow_up_prompt_is_appended_to_the_session_it_continues() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        let mut session = Session::new("project");
        session.state = RunState::Finished;
        let session_id = session.id;
        app.sessions.push(session);
        app.selected = Some(session_id);

        app.prompt = "第二条消息".into();
        app.start_run();

        let steps = &app
            .session(session_id)
            .expect("the session is still there")
            .steps;
        assert!(
            matches!(steps.last(), Some(Step::User { text, .. }) if text == "第二条消息"),
            "the follow-up prompt must be recorded as a step, got: {steps:?}"
        );
    }

    /// Two conversations at once: opening a second chat while the first is
    /// still working must not be refused, and each run's events must land in
    /// the session that run writes into rather than in whichever one is open.
    #[test]
    fn a_second_session_runs_beside_the_first() {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        app.prompt = "第一个".into();
        app.start_run();
        let first_session = app.selected.expect("the first run opened a session");

        // The point of the whole change: a new chat is not refused because
        // something else is running.
        app.new_session();
        app.prompt = "第二个".into();
        app.start_run();
        let second_session = app.selected.expect("the second run opened a session");
        assert_ne!(first_session, second_session);
        assert_eq!(app.active.len(), 2, "both runs are in flight");

        let (first_run, second_run) = match (cmd_rx.try_recv(), cmd_rx.try_recv()) {
            (Ok(Cmd::Run { run_id: first, .. }), Ok(Cmd::Run { run_id: second, .. })) => {
                (first, second)
            }
            other => panic!("expected two runs, got {other:?}"),
        };
        assert_eq!(app.run_for(first_session), Some(first_run));
        assert_eq!(app.run_for(second_session), Some(second_run));

        // An event belongs to the run that emitted it, not to the open session.
        event_tx
            .send(Event::AssistantDelta {
                run_id: second_run,
                text: "第二条回答".into(),
            })
            .expect("the app is still listening");
        app.poll();

        let second_steps = &app.session(second_session).expect("still there").steps;
        assert!(
            matches!(second_steps.last(), Some(Step::Assistant { text }) if text == "第二条回答"),
            "the delta must land in the session its run writes into"
        );
        let first_steps = &app.session(first_session).expect("still there").steps;
        assert!(
            !matches!(first_steps.last(), Some(Step::Assistant { .. })),
            "the other session must not receive it"
        );

        // Stop stops the run of the session the composer is showing, not
        // whichever run happens to be newest.
        app.selected = Some(first_session);
        app.cancel_run();
        let mut stopped_first = false;
        while let Ok(command) = cmd_rx.try_recv() {
            if matches!(command, Cmd::Cancel { run_id } if run_id == first_run) {
                stopped_first = true;
                break;
            }
        }
        assert!(stopped_first, "Stop must name the open session's run");
    }

    #[test]
    fn a_finished_run_leaves_the_other_one_going() {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            cmd_tx,
            event_rx,
            crate::config::Config::default(),
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        app.prompt = "第一个".into();
        app.start_run();
        let first_session = app.selected.expect("the first run opened a session");
        app.new_session();
        app.prompt = "第二个".into();
        app.start_run();
        let second_session = app.selected.expect("the second run opened a session");

        let first_run = match cmd_rx.try_recv() {
            Ok(Cmd::Run { run_id, .. }) => run_id,
            other => panic!("expected a run, got {other:?}"),
        };

        event_tx
            .send(Event::RunFinished {
                run_id: first_run,
                usage: None,
                measurement: None,
            })
            .expect("the app is still listening");
        app.poll();

        // Only the run that finished is retired, and only its session is marked.
        assert_eq!(app.run_for(first_session), None);
        assert!(app.run_for(second_session).is_some());
        assert_eq!(app.active.len(), 1);
        assert_eq!(
            app.session(first_session).expect("still there").state,
            RunState::Finished
        );
        assert_eq!(
            app.session(second_session).expect("still there").state,
            RunState::Running
        );
    }

    #[test]
    fn opening_plugins_requests_a_worker_refresh_once() {
        let (mut app, mut cmd_rx) = app_at("/p", no_plugins());

        app.apply_intents(vec![UiIntent::OpenPlugins]);

        assert!(
            app.show_plugins,
            "opening the rail entry shows the plugin page"
        );
        match cmd_rx.try_recv() {
            Ok(Cmd::RefreshPlugins {
                projects, settings, ..
            }) => {
                assert_eq!(projects, vec![PathBuf::from("/p")]);
                assert!(
                    settings.plugins.is_empty(),
                    "the current trust settings cross IPC"
                );
            }
            other => panic!("expected a worker plugin refresh, got {other:?}"),
        }

        app.apply_intents(vec![UiIntent::RefreshPlugins]);
        assert!(
            cmd_rx.try_recv().is_err(),
            "a second refresh is ignored while the first discovery is pending"
        );
    }

    /// An app rooted at `project`, holding `catalogue`.
    fn app_at(
        project: &str,
        catalogue: Arc<PluginCatalogue>,
    ) -> (App, mpsc::UnboundedReceiver<Cmd>) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config {
            projects: vec![project.to_string()],
            ..Default::default()
        };
        let mut app = App::new(
            cmd_tx,
            event_rx,
            config,
            "key".into(),
            Vec::new(),
            catalogue,
            test_paths(),
        );
        app.active_project = Some(project.to_string());
        (app, cmd_rx)
    }

    #[test]
    fn an_old_save_ack_does_not_clear_a_newer_session_change() {
        let (mut app, mut cmd_rx) = app_at("/p", no_plugins());
        app.sessions.push(Session::new("/p"));
        app.mark_dirty();
        app.flush();

        let first_revision = match cmd_rx.try_recv().expect("a snapshot was queued") {
            Cmd::SaveSessions { revision, .. } => revision,
            other => panic!("expected a session snapshot, got {other:?}"),
        };

        app.mark_dirty();
        app.apply(Event::SessionsSaved {
            revision: first_revision,
        });

        assert!(app.dirty, "the newer change still needs persistence");
        app.flush();
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(Cmd::SaveSessions { revision, .. }) if revision > first_revision
        ));
    }

    #[test]
    fn shutdown_queues_the_latest_snapshot_before_stopping_the_worker() {
        let (mut app, mut cmd_rx) = app_at("/p", no_plugins());
        app.sessions.push(Session::new("/p"));
        app.mark_dirty();

        app.shutdown();

        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(Cmd::SaveSessions { revision: 1, .. })
        ));
        assert!(matches!(cmd_rx.try_recv(), Ok(Cmd::Shutdown)));
    }

    /// A home holding one global plugin, `thing@test`, plus a config that
    /// enables it.
    ///
    /// The marketplace's source is a *local* directory, so the plugin resolves
    /// to a working copy — the case an uninstall must not delete. Tests that
    /// want the deletable case put a copy in the cache instead.
    fn plugin_fixture() -> (tempfile::TempDir, crate::config::Config) {
        let home = tempfile::tempdir().unwrap();
        let marketplace = home.path().join(crate::plugins::HOME_DIR).join("plugins");
        std::fs::create_dir_all(&marketplace).unwrap();
        std::fs::write(
            marketplace.join("marketplace.json"),
            r#"{"name":"test","plugins":[{"name":"thing",
               "source":{"source":"local","path":"./plugins/thing"}}]}"#,
        )
        .unwrap();

        let plugin = home.path().join("plugins").join("thing");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.json"),
            r#"{"name":"thing","version":"1.0.0","description":"The thing.",
                "runtime":{"module":"plugin.wasm",
                "apiVersion":"deluxe.harness/plugin@0.1"}}"#,
        )
        .unwrap();

        let mut config = crate::config::Config {
            projects: vec!["/p".to_string()],
            ..Default::default()
        };
        config.plugins.set_enabled("thing@test", true);
        (home, config)
    }

    /// An app that reads plugins from `home` and saves to `config_path`.
    ///
    /// The receiver is returned rather than dropped: closing the channel would
    /// make the next `Cmd` send fail, which the plugin paths read as "the agent
    /// thread has exited".
    fn plugin_app(
        home: &Path,
        config: crate::config::Config,
    ) -> (App, mpsc::UnboundedReceiver<Cmd>) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let catalogue = Arc::new(plugins::discover(home, &[], &config.plugins));
        let app = App::new(
            cmd_tx,
            event_rx,
            config,
            "key".into(),
            Vec::new(),
            catalogue,
            test_paths(),
        );
        (app, cmd_rx)
    }

    #[test]
    fn disabling_a_plugin_queues_the_config_and_refreshes_from_worker_results() {
        let (home, config) = plugin_fixture();
        let (mut app, mut cmd_rx) = plugin_app(home.path(), config);
        assert_eq!(app.catalogue.global().len(), 1, "it starts enabled");

        app.set_plugin_enabled("thing@test", &plugins::Scope::Global, false);

        let (request_id, saved) = match cmd_rx.try_recv() {
            Ok(Cmd::ReloadPlugins { request_id, config }) => (request_id, config),
            other => panic!("expected a plugin refresh, got {other:?}"),
        };
        assert!(
            !saved.plugins.plugins["thing@test"].enabled,
            "the worker receives the disabled trust setting"
        );
        let projects: Vec<PathBuf> = saved.projects.iter().map(PathBuf::from).collect();
        let catalogue = Arc::new(plugins::discover(home.path(), &projects, &saved.plugins));
        app.apply(Event::PluginsUpdated {
            request_id,
            catalogue,
        });
        assert!(app.catalogue.global().is_empty(), "off means off");
        assert_eq!(app.catalogue.disabled()[0].id, "thing@test");
    }

    #[test]
    fn enabling_a_plugin_turns_it_back_on() {
        let (home, mut config) = plugin_fixture();
        config.plugins.set_enabled("thing@test", false);
        let (mut app, mut cmd_rx) = plugin_app(home.path(), config);
        assert!(app.catalogue.global().is_empty(), "it starts disabled");
        assert_eq!(app.catalogue.disabled().len(), 1);

        app.set_plugin_enabled("thing@test", &plugins::Scope::Global, true);

        let (request_id, saved) = match cmd_rx.try_recv() {
            Ok(Cmd::ReloadPlugins { request_id, config }) => (request_id, config),
            other => panic!("expected a plugin refresh, got {other:?}"),
        };
        assert!(saved.plugins.plugins["thing@test"].enabled);
        let projects: Vec<PathBuf> = saved.projects.iter().map(PathBuf::from).collect();
        let catalogue = Arc::new(plugins::discover(home.path(), &projects, &saved.plugins));
        app.apply(Event::PluginsUpdated {
            request_id,
            catalogue,
        });
        assert_eq!(app.catalogue.global().len(), 1);
        assert!(app.catalogue.disabled().is_empty());
    }

    #[test]
    fn uninstalling_a_cached_plugin_deletes_its_files_and_switches_it_off() {
        // The cache is the one copy this agent may remove: an install populated it.
        let home = tempfile::tempdir().unwrap();
        let cached = crate::plugins::plugin_cache_root(home.path()).join("test/thing/1.0.0");
        std::fs::create_dir_all(&cached).unwrap();
        std::fs::write(
            cached.join("plugin.json"),
            r#"{"name":"thing","version":"1.0.0","runtime":{
                "module":"plugin.wasm",
                "apiVersion":"deluxe.harness/plugin@0.1"}}"#,
        )
        .unwrap();

        let mut config = crate::config::Config::default();
        config.plugins.set_enabled("thing@test", true);
        let (mut app, mut cmd_rx) = plugin_app(home.path(), config);
        assert_eq!(app.catalogue.global().len(), 1);

        app.uninstall_plugin("thing@test", &plugins::Scope::Global);

        match cmd_rx.try_recv() {
            Ok(Cmd::UninstallPlugin { id, config, .. }) => {
                assert_eq!(id, "thing@test");
                assert!(!config.plugins.plugins["thing@test"].enabled);
            }
            other => panic!("expected an uninstall request, got {other:?}"),
        }
        assert!(
            cached.is_dir(),
            "the GUI leaves filesystem mutation to the worker adapter"
        );
    }

    #[test]
    fn uninstalling_a_local_working_copy_keeps_its_files() {
        // A marketplace's local source is somebody's working tree — the copy a
        // developer is editing. It is switched off, never deleted.
        let (home, config) = plugin_fixture();
        let (mut app, mut cmd_rx) = plugin_app(home.path(), config);
        let working_copy = home.path().join("plugins").join("thing");
        assert!(working_copy.is_dir());

        app.uninstall_plugin("thing@test", &plugins::Scope::Global);

        assert!(working_copy.is_dir(), "a working copy must not be deleted");
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(Cmd::UninstallPlugin { id, .. }) if id == "thing@test"
        ));
    }

    #[test]
    fn a_plugin_operation_failure_keeps_the_current_catalogue() {
        let (home, config) = plugin_fixture();
        let (mut app, mut cmd_rx) = plugin_app(home.path(), config);
        let original = app.catalogue.clone();

        app.set_plugin_enabled("thing@test", &plugins::Scope::Global, false);
        let request_id = match cmd_rx.try_recv() {
            Ok(Cmd::ReloadPlugins { request_id, .. }) => request_id,
            other => panic!("expected a plugin refresh, got {other:?}"),
        };
        app.apply(Event::PluginOperationFailed {
            request_id,
            message: "保存配置失败".into(),
        });

        assert_eq!(app.catalogue.global().len(), 1, "the old catalogue remains");
        assert!(Arc::ptr_eq(&app.catalogue, &original));
        assert!(
            app.plugins_error
                .as_deref()
                .is_some_and(|error| error.contains("保存配置失败")),
            "the failure is reported in the plugins window, got: {:?}",
            app.plugins_error
        );
    }

    #[test]
    fn a_new_chat_is_rooted_at_the_active_project() {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config {
            projects: vec!["/a".into(), "/b".into()],
            ..Default::default()
        };
        let mut app = App::new(
            cmd_tx,
            event_rx,
            config,
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );
        app.active_project = Some("/b".into());

        app.prompt = "你好".into();
        app.start_run();

        // The session the run created is rooted at the active project...
        let session = app.sessions.last().expect("a session was created");
        assert_eq!(session.project, "/b");
        // ...and that is the directory the worker is told to run in.
        match cmd_rx.try_recv().expect("a run was sent") {
            Cmd::Run { project, .. } => assert_eq!(project, PathBuf::from("/b")),
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn a_follow_up_runs_in_the_projects_own_directory() {
        // Pointing the sidebar at another project must not move an open
        // conversation: the session's own project is what the run is rooted at.
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = crate::config::Config {
            projects: vec!["/a".into(), "/b".into()],
            ..Default::default()
        };
        let mut app = App::new(
            cmd_tx,
            event_rx,
            config,
            "key".into(),
            Vec::new(),
            no_plugins(),
            test_paths(),
        );

        let mut session = Session::new("/a");
        session.state = RunState::Finished;
        let session_id = session.id;
        app.sessions.push(session);
        app.selected = Some(session_id);
        app.active_project = Some("/b".into());

        app.prompt = "继续".into();
        app.start_run();

        match cmd_rx.try_recv().expect("a run was sent") {
            Cmd::Run { project, .. } => assert_eq!(project, PathBuf::from("/a")),
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn a_reasoning_block_keeps_its_id_while_it_streams() {
        // The fold's open/closed state is keyed on this id. It used to be
        // derived from the block's text, so it changed with every streamed
        // fragment — and an open chain of thought snapped shut on the next
        // frame, long before the model stopped thinking.
        let mut session = Session::new("project");
        session.push_reasoning("先看看");
        let id = match session.steps.last() {
            Some(Step::Reasoning { id, .. }) => *id,
            other => panic!("expected a reasoning step, got {other:?}"),
        };

        session.push_reasoning("目录结构。");
        session.push_reasoning("有一个 src 目录。");
        match session.steps.last() {
            Some(Step::Reasoning { id: still, .. }) => assert_eq!(*still, id),
            other => panic!("expected the same reasoning step, got {other:?}"),
        }
    }

    #[test]
    fn two_reasoning_blocks_never_share_a_fold_state() {
        let mut session = Session::new("project");
        session.push_reasoning("第一个想法。");
        let first = match session.steps.last() {
            Some(Step::Reasoning { id, .. }) => *id,
            other => panic!("expected a reasoning step, got {other:?}"),
        };

        // Anything but reasoning closes the block, so the next one gets a fresh
        // id — distinct from the first, even when the text is identical.
        session.steps.push(Step::Assistant {
            text: "回答".into(),
        });
        session.push_reasoning("第一个想法。");
        match session.steps.last() {
            Some(Step::Reasoning { id: second, .. }) => assert_ne!(*second, first),
            other => panic!("expected a second reasoning step, got {other:?}"),
        }
    }
}
