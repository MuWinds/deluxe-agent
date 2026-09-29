//! The agent loop.
//!
//! One run is: send the conversation to the model, stream back an assistant
//! turn, execute whatever tools it asked for, append the results, repeat until
//! it answers without calling a tool.
//!
//! The chain of thought, when the provider streams one, is shown to the user and
//! nothing more: it is never sent back to the model, which replays its own
//! reasoning internally — models that think expect their last turn to carry
//! only the answer.
//!
//! There is no approval step and no path sandbox: the model runs with full
//! permissions. The ordering inside [`Agent::dispatch`] is still the part worth
//! preserving — validate, then execute — because splitting it across callers is
//! how one of them eventually forgets to validate.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::attachments::ImageRef;
use crate::context::{summarize, summary_turn, ContextSettings, ContextWindow};
use crate::error::{AgentError, Result};
use crate::ipc::{AuditOutcome, Event, RunId};
use crate::llm::{
    AssistantTurn, LlmClient, Message, StreamFragment, ThinkingLevel, ToolCall, UserTurn,
};
use crate::plugins::hooks::Hook;
use crate::plugins::skills::{self, Skill};
use crate::plugins::{AgentRole, LoadedPlugin};
use crate::tools::{
    to_openai_tools, validate_arguments, JobRegistry, ToolOutput, ToolRegistry, ToolSettings,
};

/// How often streamed text is flushed to the UI.
///
/// A fast model emits hundreds of fragments per second, and one repaint per
/// fragment pins a core for no visible benefit — the screen cannot show more
/// than ~60 updates a second anyway.
const FLUSH_INTERVAL: Duration = Duration::from_millis(33);

/// A turn's streamed text, batched between flushes.
///
/// Reasoning and answer never share a buffer, so a thinking model's fragments
/// cannot interleave the two in the transcript.
#[derive(Default)]
struct TurnText {
    reasoning: String,
    content: String,
}

/// Project-level instruction files, looked up in the working directory.
///
/// `README.md` last on purpose: it is documentation first and instructions
/// second, so an explicit `AGENTS.md` — or the de-facto `CLAUDE.md` — wins a
/// conflict simply by appearing earlier.
const PROJECT_CONTEXT_FILES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "README.md"];

/// A cap per context file, so a runaway README cannot eat the context window.
const MAX_PROJECT_CONTEXT_BYTES: usize = 16_000;

/// The host's own rules, appended after the tools' guidelines.
const HOST_RULES: &str = "\
- Read a file before you change it.
- Relative paths resolve against the working directory; absolute paths are used \
as given. You have full read and write access to this machine.
- Say in one short sentence what you are about to do before you call a tool, so \
the user can follow along.
- Do not repeat a failing call unchanged. If a tool returns an error, read it and \
adjust. If it says the call was refused, the host blocked it — do not try again \
and do not look for a way around it.
- When the task is done, reply with a short summary of what changed. Do not call \
a tool just to confirm.
</rules>";

/// The rules a delegated sub-agent runs under.
///
/// A role is a worker, not a peer: it cannot ask the caller a question, so the
/// one thing it must be told is to state its assumptions rather than stall. The
/// rest of the host's rules would be wrong here — a sub-agent that "replied with
/// a summary of what changed" without doing the task would be useless.
const SUB_AGENT_RULES: &str = "\
- You are a sub-agent: another agent delegated a single task to you. Complete it \
and answer with the result. You cannot ask the caller anything, so state any \
assumption you had to make rather than stopping.
- When the task is done, reply with your findings. Do not call a tool just to \
confirm.
</rules>";

/// The `<tools>` element, which every prompt carries.
fn tool_section(registry: &ToolRegistry) -> String {
    format!("<tools>\n{}\n</tools>\n", registry.tools_for_prompt())
}

/// The `<rules>` element: the tools' own guidelines, then the host's.
fn rule_section(registry: &ToolRegistry, host_rules: &str) -> String {
    let mut rules = String::from("<rules>\n");
    for rule in registry.guidelines_for_prompt() {
        rules.push_str("- ");
        rules.push_str(&rule);
        rules.push('\n');
    }
    rules.push_str(host_rules);
    rules
}

/// The immutable prefix of every request: identity, the tool catalogue, the
/// skill catalogue, and the global rules.
///
/// Assembled from the [`ToolRegistry`], so a tool's `summary` and `guidelines`
/// each have one source — the tool itself — instead of a hand-copied list that
/// drifts out of step with the schema. The full prompt is byte-stable for the
/// lifetime of the `Agent`: re-sending an identical prefix is what keeps the
/// providers' automatic prompt caches warm, which is also why the project
/// files are snapshotted at construction rather than re-read per prompt.
///
/// `skills` are the ones contributed by the plugins that apply to this agent's
/// project — see [`crate::plugins::PluginCatalogue::for_project`]. `agents` are
/// the delegation targets those plugins offer, and are empty when `task` is not
/// registered, so the prompt never points at a tool the model does not have.
fn build_system_prompt(
    registry: &ToolRegistry,
    skills: &[&Skill],
    agents: &[&AgentRole],
) -> String {
    let mut prompt =
        String::from("You are a useful coding agent running on the user's own machine.\n\n");

    prompt.push_str(&tool_section(registry));

    // The skill catalogue sits beside the tool catalogue: both answer "what can
    // this agent do", and both are read before the rules that say how to use
    // them. The agent catalogue follows for the same reason.
    if let Some(section) = skills::render(skills) {
        prompt.push('\n');
        prompt.push_str(&section);
        prompt.push('\n');
    }

    if let Some(section) = crate::plugins::agents::render(agents) {
        prompt.push('\n');
        prompt.push_str(&section);
        prompt.push('\n');
    }

    prompt.push('\n');
    prompt.push_str(&rule_section(registry, HOST_RULES));
    prompt
}

/// The prefix a delegated sub-agent runs under: the role's own instructions,
/// then the tool catalogue and the rules a worker needs.
///
/// The role file says what the sub-agent *is*, and that is the whole reason the
/// plugin shipped it, so it leads and is not wrapped in the host's identity. The
/// catalogue and rules follow, because a role that cannot see its tools cannot
/// use them — a `figma` role is told to call `get_design_context`, and the tools
/// element is where it learns that tool exists.
pub fn build_role_prompt(role: &AgentRole, registry: &ToolRegistry) -> String {
    format!(
        "{}\n\n{}{}",
        role.instructions,
        tool_section(registry),
        rule_section(registry, SUB_AGENT_RULES)
    )
}

/// Reads the project context files found in `cwd`.
///
/// Files that are missing, unreadable, or blank are simply skipped: a project
/// without a README is not an error.
fn collect_project_context(cwd: &Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for name in PROJECT_CONTEXT_FILES {
        let Ok(content) = std::fs::read_to_string(cwd.join(name)) else {
            continue;
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        found.push((name.to_string(), cap_bytes(trimmed).to_string()));
    }
    found
}

/// Caps a context file on a character boundary, so a multi-byte character is
/// never split into invalid UTF-8 mid-prompt.
fn cap_bytes(text: &str) -> &str {
    if text.len() <= MAX_PROJECT_CONTEXT_BYTES {
        return text;
    }
    let mut end = MAX_PROJECT_CONTEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Wraps each context file in a labelled element, after pi's shape.
fn render_project_context(context: &[(String, String)]) -> String {
    let mut rendered = String::from("Project-specific instructions and guidelines:\n");
    for (path, content) in context {
        rendered.push_str(&format!(
            "\n<project_instructions path=\"{path}\">\n{content}\n</project_instructions>\n"
        ));
    }
    rendered
}

/// Where the agent publishes progress.
///
/// The GUI implements this by pushing into a channel and asking egui to repaint;
/// tests implement it by collecting into a `Vec`.
pub trait EventSink: Send + Sync {
    /// Delivers one event. Called from the worker thread, so it must not block.
    fn emit(&self, event: Event);
}

pub struct Agent {
    llm: LlmClient,
    registry: Arc<ToolRegistry>,
    /// Behind a lock so the settings panel can flip the destructive-command
    /// guard and have it apply on the next call, rather than needing a restart.
    settings: Arc<RwLock<ToolSettings>>,
    /// The project this agent serves, snapshotted at construction.
    ///
    /// Injected into the tool settings at dispatch, so a relative path and
    /// `exec`'s default cwd belong to the session's project rather than to the
    /// default directory the shared [`ToolSettings`] happens to hold.
    working_directory: PathBuf,
    tools_schema: Value,
    /// Built once at construction — see [`build_system_prompt`].
    system_prompt: String,
    /// The user's context-window configuration: the model's limit and the
    /// share of it that triggers compaction. Read per run, which is when a
    /// fresh measurement window is built from it.
    context_settings: ContextSettings,
    /// The `PostToolUse` hooks that apply to this agent's project, snapshotted
    /// at construction like the skills.
    ///
    /// Empty for a sub-agent: a delegated role's tool calls do not re-fire the
    /// plugin's hooks, so delegation cannot quietly multiply a hook's work.
    hooks: Vec<Hook>,
}

/// Everything one run carries beyond the prompt itself.
///
/// A single struct rather than eight positional parameters: the two `Option`s
/// of the same shape (`thinking` and `carried`) sit apart in the list, and a
/// call site passing `None, ..., None` is exactly how one of them ends up in
/// the wrong slot.
pub struct RunRequest<'a> {
    /// The conversation this prompt continues, oldest first, in wire format.
    pub history: &'a [Message],
    /// The reasoning effort this conversation runs at, or `None` to send no
    /// `reasoning_effort` parameter and leave the model on its own default.
    pub thinking: Option<ThinkingLevel>,
    /// The usage the previous run of this conversation measured: provider
    /// prompt tokens and the message count they describe. Seeded into the
    /// fresh window so the first request of a long session is already guarded.
    pub carried: Option<(u64, usize)>,
}

/// The result of a dispatch, paired with what the audit trail should record.
///
/// The two travel together because the outcome is decided in several different
/// branches, and reconstructing it from the error afterwards would lose the
/// distinction between "refused" and "failed".
struct Dispatched {
    result: Result<ToolOutput>,
    outcome: AuditOutcome,
}

impl Dispatched {
    /// Classifies a tool error.
    ///
    /// A refusal is recorded apart from a failure because it tells the model
    /// something different: do not retry this call unchanged.
    fn from_error(error: AgentError) -> Self {
        let outcome = if error.is_denial() {
            AuditOutcome::Denied
        } else {
            AuditOutcome::Failed
        };
        Self {
            result: Err(error),
            outcome,
        }
    }
}

impl Agent {
    /// Builds an agent for one project.
    ///
    /// `working_directory` is that project's root: it seeds the project-context
    /// snapshot in the system prompt, and every tool call this agent makes is
    /// resolved against it. `settings` stays shared with the worker so a guard
    /// flip still reaches an already-cached agent.
    ///
    /// `plugins` are the ones that apply to this project, already resolved by
    /// [`crate::plugins::PluginCatalogue::for_project`]. The agent is handed the
    /// resolved list rather than the catalogue so it never has to know that
    /// plugins come in scopes at all, and borrowed rather than owned because it
    /// only reads them — a clone here would copy every manifest and skill
    /// description each time a project's agent is rebuilt.
    pub fn new(
        llm: LlmClient,
        registry: Arc<ToolRegistry>,
        settings: Arc<RwLock<ToolSettings>>,
        working_directory: PathBuf,
        context_settings: ContextSettings,
        plugins: &[&LoadedPlugin],
    ) -> Self {
        let tools_schema = to_openai_tools(&registry);
        // Snapshot, not a per-prompt read: identical bytes on every follow-up
        // request is what keeps the prefix cache warm, and re-reading the
        // project files would buy nothing but cache misses. The worker rebuilds
        // the agent when the model or the working directory changes, which is
        // the way to refresh the snapshot.
        let skills: Vec<&Skill> = plugins
            .iter()
            .flat_map(|plugin| plugin.skills.iter())
            .collect();
        // Advertised only when `task` is registered for this project, so the
        // prompt never tells the model to delegate to a tool it does not have.
        // `main` registers `task` exactly when a project has roles, so the two
        // agree — and this check is what keeps them agreeing if that changes.
        let agents: Vec<&AgentRole> = if registry.get("task").is_some() {
            plugins
                .iter()
                .flat_map(|plugin| plugin.agents.iter())
                .collect()
        } else {
            Vec::new()
        };
        let mut system_prompt = build_system_prompt(&registry, &skills, &agents);
        let context = collect_project_context(&working_directory);
        if !context.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&render_project_context(&context));
        }
        // The hooks are snapshotted for the same reason the skills are: they
        // belong to this project, and a run must not pick up another project's
        // plugin mid-flight.
        let hooks: Vec<Hook> = plugins
            .iter()
            .flat_map(|plugin| plugin.hooks.iter().cloned())
            .collect();
        Self {
            llm,
            registry,
            settings,
            working_directory,
            tools_schema,
            system_prompt,
            context_settings,
            hooks,
        }
    }

    /// Builds a sub-agent for a plugin-defined role.
    ///
    /// The difference from [`Agent::new`] is the prefix: a role file says what
    /// the sub-agent *is*, so its instructions replace the host's identity
    /// rather than sitting under it — see [`build_role_prompt`]. Everything
    /// else is shared, which is the point: a delegated role is a full agent
    /// with tools and a loop, not a prompt expansion.
    ///
    /// `hooks` are deliberately empty. A sub-agent's tool calls do not re-fire
    /// the plugin's `PostToolUse` hooks, so delegating a job cannot silently
    /// multiply a hook's work; the hook already ran for the call that delegated.
    pub fn for_role(
        llm: LlmClient,
        registry: Arc<ToolRegistry>,
        settings: Arc<RwLock<ToolSettings>>,
        working_directory: PathBuf,
        context_settings: ContextSettings,
        role: &AgentRole,
    ) -> Self {
        let tools_schema = to_openai_tools(&registry);
        let system_prompt = build_role_prompt(role, &registry);
        Self {
            llm,
            registry,
            settings,
            working_directory,
            tools_schema,
            system_prompt,
            context_settings,
            hooks: Vec::new(),
        }
    }

    /// Replaces the conversation with one summary the model writes.
    ///
    /// Nothing survives verbatim: every message goes into the transcript, and
    /// what comes back is the whole of what the run carries on from — see
    /// [`Agent::with_summary`].
    async fn compact_history(
        &self,
        messages: Vec<Message>,
        context: &mut ContextWindow,
        sink: &dyn EventSink,
        run_id: RunId,
    ) -> Result<Vec<Message>> {
        let count = messages.len();

        sink.emit(Event::CompactionStarted {
            run_id,
            dropping: count,
        });
        tracing::info!(
            run_id,
            dropping = count,
            measured_tokens = context.latest_usage().unwrap_or(0),
            "context threshold reached; compacting the history"
        );

        match summarize(&messages, &self.llm).await {
            Ok(Some(summary)) => {
                // The measurement belonged to the pre-compaction shape and no
                // longer describes what will go out next; the turn that
                // follows re-measures.
                *context = ContextWindow::new(context.settings());
                // The gauge has to forget the old figure too, or it would sit
                // there claiming a token count that describes a conversation
                // which no longer exists.
                sink.emit(Event::UsageSampled {
                    run_id,
                    measurement: None,
                });
                sink.emit(Event::Compacted {
                    run_id,
                    summary: summary.clone(),
                });
                Ok(self.with_summary(&summary))
            }
            Ok(None) => {
                // Nothing to summarise: the transcript rendered empty, or the
                // model answered with nothing. The history goes out as it
                // stands rather than being replaced by a blank brief.
                tracing::warn!(
                    run_id,
                    "compaction produced no summary; sending the full history"
                );
                Ok(messages)
            }
            Err(error) => {
                // Compaction is a mitigation, not the task. A failed summary
                // request is logged and swallowed so the original prompt still
                // goes out; the worst case is the provider refusing it, which
                // the run failure already surfaces.
                tracing::warn!(run_id, %error, "compaction failed; sending the full history");
                sink.emit(Event::Compacted {
                    run_id,
                    summary: String::new(),
                });
                Ok(messages)
            }
        }
    }

    /// Rebuilds the conversation around a compaction summary.
    ///
    /// The brief is the whole of what the run carries on from, and it is built
    /// by [`summary_turn`] so a later run replaying this session reconstructs
    /// the identical message.
    fn with_summary(&self, summary: &str) -> Vec<Message> {
        vec![
            Message::system(self.system_prompt.clone()),
            summary_turn(summary),
        ]
    }

    /// The background job runtime this agent's tools share.
    ///
    /// Exposed so the worker can hand the window a snapshot of the jobs a
    /// project's agent has started — the composer's task list reads it without
    /// the GUI ever reaching into the tool layer.
    pub fn jobs(&self) -> &Arc<JobRegistry> {
        self.registry.jobs()
    }

    /// Runs one prompt to completion.
    ///
    /// The per-conversation pieces travel in [`RunRequest`]; the updated usage
    /// measurement comes back in the result, for the next run of the same
    /// conversation to seed its own window with.
    pub async fn run(
        &self,
        run_id: RunId,
        prompt: UserTurn,
        request: RunRequest<'_>,
        cancel: CancellationToken,
        sink: &dyn EventSink,
    ) -> Result<Option<(u64, usize)>> {
        let RunRequest {
            history,
            thinking,
            carried,
        } = request;

        // The system prompt first, then whatever the session has already said,
        // then the new prompt. `history` arrives in wire format from the GUI,
        // which derived it from the session's own transcript, so a follow-up
        // really does continue that conversation instead of starting over.
        //
        // Cloned rather than borrowed: `Message` owns its text, and the prompt
        // is byte-identical across every request this agent serves — until a
        // compaction replaces the whole conversation, see
        // [`Agent::with_summary`].
        let mut messages = Vec::with_capacity(history.len() + 2);
        messages.push(Message::system(self.system_prompt.clone()));
        messages.extend_from_slice(history);
        messages.push(Message::user_turn(prompt));

        // Fresh per run, but seeded with whatever the previous run of this same
        // conversation measured: the window itself lives on the frame, so a
        // long session's very first request is already guarded instead of
        // waiting for one round-trip to discover it is too big.
        let mut context = ContextWindow::new(self.context_settings);
        context.restore(carried);

        // Deliberately uncapped. A long-horizon task can legitimately take
        // dozens or hundreds of tool calls, and the loop already ends on the
        // model's own terms: the turn that comes back without tool calls is the
        // model saying it is done. The only other exits are cancellation and a
        // transport error. Capping it here just chops off exactly the tasks the
        // agent is most useful for.
        loop {
            if cancel.is_cancelled() {
                return Err(AgentError::cancelled());
            }

            // The provider's own count for the *previous* request decides
            // whether this one would overflow — never a local estimate. The
            // measurement matches the message count it was taken on, which is
            // the count now about to go out (the user prompt is already in).
            if context.should_compact(&messages) {
                messages = self
                    .compact_history(messages, &mut context, sink, run_id)
                    .await?;
            }

            let turn = self
                .stream_turn(&messages, thinking, &cancel, sink, run_id)
                .await?;

            // The figure that decides the next iteration's shape. A turn the
            // provider gave no usage for — or one that returned mid-flight —
            // leaves the last measurement in place, which is the honest state:
            // "unknown" is not "small".
            if let Some(usage) = &turn.usage {
                context.record_usage(usage.prompt_tokens().unwrap_or(0), messages.len());
            }
            // Tool calls mean more requests are coming, so the gauge gets the
            // figure now rather than at the end — that is the difference
            // between tracking the run and summarising it. Only the sample that
            // would change the gauge is emitted; a provider that reported no
            // usage has nothing to say and stays quiet. A successful compaction
            // invalidates the figure instead — a `None` reset goes out from
            // [`Agent::compact_history`].
            if !turn.tool_calls.is_empty() {
                sink.emit(Event::UsageSampled {
                    run_id,
                    measurement: context.measurement(),
                });
            }

            // A reply cut off by the token limit is a confusing failure: the
            // model may have been mid-tool-call. Say so rather than letting it
            // look like the model simply stopped.
            if turn.finish_reason.as_deref() == Some("length") {
                tracing::warn!(run_id, "the model's reply hit the token limit");
            }

            sink.emit(Event::AssistantDone {
                run_id,
                content: turn.content.clone(),
            });

            messages.push(Message::assistant(
                turn.content.clone(),
                turn.tool_calls.clone(),
            ));

            // No tool calls means the model considers the task finished.
            if turn.tool_calls.is_empty() {
                sink.emit(Event::RunFinished {
                    run_id,
                    usage: turn.usage,
                    // Handed back so the next run of this conversation can seed
                    // its own window with the figure this one measured.
                    measurement: context.measurement(),
                });
                return Ok(context.measurement());
            }

            for call in &turn.tool_calls {
                if cancel.is_cancelled() {
                    return Err(AgentError::cancelled());
                }
                let (text, images) = self.execute(call, &cancel, sink, run_id).await;
                messages.push(Message::tool_with_images(call.id.clone(), text, &images));
            }

            // Background jobs that settled while this batch ran are announced
            // now, in the same turn, so the model learns about them rather than
            // polling. The notice is injected as a user turn — the harness's
            // inbox — and shown in the transcript as a notice.
            let notices = self.registry.jobs().drain_notifications();
            if !notices.is_empty() {
                let text = notices.join("\n");
                messages.push(Message::user(text.clone()));
                sink.emit(Event::Notice { run_id, text });
            }
        }
    }

    /// Streams one assistant turn, coalescing text before it reaches the UI.
    ///
    /// Reasoning and answer are batched separately and flushed separately, so
    /// the transcript always shows which one a fragment belonged to.
    async fn stream_turn(
        &self,
        messages: &[Message],
        thinking: Option<ThinkingLevel>,
        cancel: &CancellationToken,
        sink: &dyn EventSink,
        run_id: RunId,
    ) -> Result<AssistantTurn> {
        let mut turn = TurnText::default();
        let mut last_flush = Instant::now();

        let outcome = self
            .llm
            .stream_turn(messages, &self.tools_schema, thinking, cancel, |fragment| {
                match fragment {
                    StreamFragment::Reasoning(text) => turn.reasoning.push_str(text),
                    StreamFragment::Content(text) => turn.content.push_str(text),
                }
                if last_flush.elapsed() >= FLUSH_INTERVAL {
                    flush_turn(&mut turn, run_id, sink);
                    last_flush = Instant::now();
                }
            })
            .await;

        // Flush the tail even on failure, so the UI never shows a half-rendered
        // turn and then nothing.
        flush_turn(&mut turn, run_id, sink);

        outcome
    }

    /// Executes one tool call and renders the result for the model.
    ///
    /// Returns the text the transcript records and any images the call
    /// produced. The two travel together because the tool message that carries
    /// them is built from both: a `read_image` result is a text envelope plus
    /// the picture itself.
    ///
    /// Failures become tool output rather than a failed run: the model can read
    /// "no such file" and try something else, whereas aborting throws away the
    /// whole conversation.
    async fn execute(
        &self,
        call: &ToolCall,
        cancel: &CancellationToken,
        sink: &dyn EventSink,
        run_id: RunId,
    ) -> (String, Vec<ImageRef>) {
        let started = Instant::now();
        let parsed = serde_json::from_str::<Value>(&call.function.arguments);

        // Emitted before the arguments are parsed, so a call whose arguments are
        // not JSON still gets a card in the transcript. It carries the raw text
        // in that case: a failure the user cannot see is worse than an ugly one.
        sink.emit(Event::ToolStarted {
            run_id,
            call_id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: match &parsed {
                Ok(value) => value.clone(),
                Err(_) => Value::String(call.function.arguments.clone()),
            },
        });

        let arguments = match parsed {
            Ok(value) => value,
            Err(error) => {
                let message = format!(
                    "Could not parse the arguments for `{}` as JSON: {error}. \
                     Raw arguments were: {}",
                    call.function.name, call.function.arguments
                );
                sink.emit(Event::ToolFinished {
                    run_id,
                    call_id: call.id.clone(),
                    outcome: AuditOutcome::Failed,
                    output: message.clone(),
                    images: Vec::new(),
                    hunks: Vec::new(),
                    duration_ms: started.elapsed().as_millis() as u64,
                });
                return (message, Vec::new());
            }
        };

        let dispatched = self.dispatch(call, &arguments, cancel).await;
        let (mut text, images, hunks) = match dispatched.result {
            Ok(output) => {
                // Read the images out first: `as_text` borrows `output`, and a
                // second borrow cannot overlap it.
                let images = output.images();
                (output.as_text(), images, output.hunks)
            }
            Err(error) => {
                // A refusal reads differently to the model than a failure: it
                // should not retry the same call unchanged.
                let label = if error.is_denial() {
                    "Refused"
                } else {
                    "Error"
                };
                (format!("{label}: {error}"), Vec::new(), Vec::new())
            }
        };

        // Hooks run after the call, and their output joins the result: that is
        // what makes a `PostToolUse` hook a *nudge* rather than a side note, as
        // the model reads the very text this becomes.
        //
        // A *refused* call is the one case they are skipped. The denylist
        // stopped the tool before it did anything, so a hook that exists to
        // react to what a tool did — "you just wrote a file, re-check parity" —
        // would be describing something that never happened.
        if dispatched.outcome != AuditOutcome::Denied {
            self.run_hooks(&call.function.name, cancel, &mut text).await;
        }

        sink.emit(Event::ToolFinished {
            run_id,
            call_id: call.id.clone(),
            outcome: dispatched.outcome,
            output: text.clone(),
            images: images.clone(),
            hunks: hunks.clone(),
            duration_ms: started.elapsed().as_millis() as u64,
        });

        (text, images)
    }

    /// Runs the `PostToolUse` hooks that match `tool`, appending their output to
    /// the tool result.
    ///
    /// The output is appended rather than surfaced on its own because it is part
    /// of what the call produced: the model reads it as the tool's answer, and
    /// the transcript shows it in the same card. A hook that cannot run is
    /// reported in that same place, so a broken one is visible rather than
    /// silent.
    ///
    /// Hooks are few and sequential. They are also the one place a plugin gets
    /// to run a command the model did not ask for, so each goes through `exec`
    /// and inherits its shell backend, timeout, and destructive-command guard.
    async fn run_hooks(&self, tool: &str, cancel: &CancellationToken, text: &mut String) {
        let hooks: Vec<&Hook> = self
            .hooks
            .iter()
            .filter(|hook| hook.matches(tool))
            .collect();
        if hooks.is_empty() {
            return;
        }

        // Cloned out rather than held across the awaits, for the same reason
        // `dispatch` clones them: a slow hook must not block the settings panel.
        let settings = {
            let settings = self.settings.read().await;
            let mut settings = settings.clone();
            settings.working_directory = self.working_directory.clone();
            settings
        };

        for hook in hooks {
            if cancel.is_cancelled() {
                return;
            }

            tracing::debug!(
                tool,
                plugin = %hook.plugin,
                pattern = %hook.pattern,
                command = %hook.command,
                "running a PostToolUse hook"
            );

            // The `cwd` is the plugin root, which is what makes a relative
            // `./scripts/x.sh` resolve the way the plugin author meant.
            let arguments = json!({ "command": hook.command.as_str(), "cwd": &hook.root });
            // The registered `exec`, not a fresh one: the hook runs through the
            // same tool the model would call, so it inherits the shell backend,
            // timeout, and destructive-command guard, and its job registry.
            let result = match self.registry.get("exec") {
                Some(exec) => exec.execute(arguments, &settings).await,
                None => Err(AgentError::tool_not_found("exec")),
            };
            let (body, failed) = match result {
                Ok(output) => {
                    let failed = output.is_error;
                    (
                        output.truncate_to(settings.max_output_chars).as_text(),
                        failed,
                    )
                }
                Err(error) => (format!("could not run: {error}"), true),
            };

            text.push_str("\n\n");
            text.push_str(&format!(
                "PostToolUse hook `{}` (plugin {}){}:\n{}",
                hook.command,
                hook.plugin,
                if failed { " failed" } else { "" },
                body.trim()
            ));
        }
    }

    /// Validate → execute → truncate.
    async fn dispatch(
        &self,
        call: &ToolCall,
        arguments: &Value,
        cancel: &CancellationToken,
    ) -> Dispatched {
        let name = call.function.name.as_str();

        let tool = match self.registry.require(name) {
            Ok(tool) => tool,
            Err(error) => return Dispatched::from_error(error),
        };
        let descriptor = tool.descriptor();

        // Schema validation runs before anything else, so a malformed call never
        // reaches a handler that might act on the parts it could parse.
        //
        // Only where the schema is the host's to enforce. An MCP server's is
        // not: it may use keywords this narrow `ObjectSchema` cannot express,
        // and the server validates the call itself — so refusing one here would
        // mean inventing a rejection the real validator never made.
        if descriptor.host_validates_arguments {
            if let Err(error) = validate_arguments(&descriptor.input_schema, arguments) {
                return Dispatched::from_error(error);
            }
        }

        // The settings are cloned out rather than held across the await, so a
        // long-running command cannot block the settings panel from applying.
        // The clone also carries this run's project: the shared settings hold
        // the default directory, and a tool must resolve against the session's.
        //
        // The timeout is the host's net. A tool that bounds itself — `exec`,
        // whose foreground wait ends by promoting the command to a background
        // job — gets a generous ceiling instead, so the shorter default cannot
        // cut it off before its own budget applies.
        const HOST_TIMEOUT_CEILING_MS: u64 = 605_000;
        let (settings, timeout) = {
            let settings = self.settings.read().await;
            let mut settings = settings.clone();
            settings.working_directory = self.working_directory.clone();
            let timeout = if tool.bounds_own_timeout() {
                Duration::from_millis(settings.default_timeout_ms.max(HOST_TIMEOUT_CEILING_MS))
            } else {
                Duration::from_millis(settings.default_timeout_ms)
            };
            (settings, timeout)
        };

        let output = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Dispatched::from_error(AgentError::cancelled()),
            result = tokio::time::timeout(timeout, tool.execute(arguments.clone(), &settings)) => {
                match result {
                    Ok(Ok(output)) => output,
                    Ok(Err(error)) => return Dispatched::from_error(error),
                    Err(_) => {
                        return Dispatched::from_error(AgentError::timeout(format!(
                            "`{name}` exceeded the {timeout:?} host limit"
                        )));
                    }
                }
            }
        };

        // A tool that reports an error in its output still counts as a failure,
        // even though the call itself ran.
        let outcome = if output.is_error {
            AuditOutcome::Failed
        } else {
            AuditOutcome::Executed
        };

        Dispatched {
            result: Ok(output.truncate_to(settings.max_output_chars)),
            outcome,
        }
    }
}

/// Hands the batched fragments of the current turn to the sink, taking them so
/// the periodic flushes never emit the same text twice.
fn flush_turn(turn: &mut TurnText, run_id: RunId, sink: &dyn EventSink) {
    let reasoning = std::mem::take(&mut turn.reasoning);
    if !reasoning.is_empty() {
        sink.emit(Event::ReasoningDelta {
            run_id,
            text: reasoning,
        });
    }
    let content = std::mem::take(&mut turn.content);
    if !content.is_empty() {
        sink.emit(Event::AssistantDelta {
            run_id,
            text: content,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_prompt_is_assembled_from_the_tool_catalogue() {
        let prompt = build_system_prompt(&ToolRegistry::with_builtins(), &[], &[]);

        for name in ["apply_patch", "exec", "list_dir", "read_file"] {
            assert!(
                prompt.contains(&format!("- `{name}`:")),
                "the catalogue is missing `{name}`: {prompt}"
            );
        }
        // A guideline lives with its tool and reaches the rules from there —
        // nothing here may hand-copy it.
        assert!(prompt.contains("one patch rather than two"));
        assert!(prompt.contains("Prefer `read_file` over `exec`"));
    }

    #[test]
    fn plugin_skills_reach_the_system_prompt_ahead_of_the_rules() {
        let skill = Skill {
            name: "computer-use".into(),
            description: Some("Control Windows apps".into()),
            path: PathBuf::from("/plugins/computer-use/skills/computer-use/SKILL.md"),
            plugin: "computer-use@openai-bundled".into(),
        };

        let prompt = build_system_prompt(&ToolRegistry::with_builtins(), &[&skill], &[]);

        assert!(prompt.contains("<skills>"), "{prompt}");
        assert!(prompt.contains("`computer-use`"), "{prompt}");
        assert!(
            prompt.contains("/plugins/computer-use/skills/computer-use/SKILL.md"),
            "the model is told where to read the body: {prompt}"
        );
        assert!(
            prompt.find("<skills>").unwrap() < prompt.find("<rules>").unwrap(),
            "the catalogue precedes the rules it is used under: {prompt}"
        );
    }

    #[test]
    fn a_plugin_free_install_gets_no_skills_section() {
        // An empty `<skills></skills>` would be noise in every request of an
        // install that has no plugins.
        let prompt = build_system_prompt(&ToolRegistry::with_builtins(), &[], &[]);
        assert!(!prompt.contains("<skills>"), "{prompt}");
        assert!(!prompt.contains("<agents>"), "{prompt}");
    }

    #[test]
    fn the_agent_catalogue_reaches_the_prompt_ahead_of_the_rules() {
        let role = AgentRole {
            name: "figma-implementation-agent".into(),
            description: Some("Write the code".into()),
            instructions: "You are the Figma Implementation Agent.".into(),
            plugin: "figma@openai-curated".into(),
            path: PathBuf::from("/plugins/figma/agents/figma-implementation-agent.md"),
        };

        let prompt = build_system_prompt(&ToolRegistry::with_builtins(), &[], &[&role]);

        assert!(prompt.contains("<agents>"), "{prompt}");
        assert!(prompt.contains("`figma-implementation-agent`"), "{prompt}");
        assert!(
            prompt.find("<agents>").unwrap() < prompt.find("<rules>").unwrap(),
            "the catalogue precedes the rules it is used under: {prompt}"
        );
        // The role's own instructions belong to the sub-agent, not here: this
        // prompt only offers the delegation target.
        assert!(
            !prompt.contains("You are the Figma Implementation Agent."),
            "{prompt}"
        );
    }

    #[test]
    fn the_tool_schema_is_shaped_for_openai() {
        let registry = ToolRegistry::with_builtins();
        let schema = to_openai_tools(&registry);
        let entries = schema.as_array().expect("an array of tools");
        assert_eq!(entries.len(), 7);

        for entry in entries {
            assert_eq!(entry["type"], "function");
            let function = &entry["function"];
            assert!(function["name"].is_string());
            assert!(function["description"].is_string());
            assert_eq!(function["parameters"]["type"], "object");
        }
    }

    #[test]
    fn a_refusal_is_recorded_apart_from_a_failure() {
        let refused = Dispatched::from_error(AgentError::denied("no"));
        assert_eq!(refused.outcome, AuditOutcome::Denied);

        let failed = Dispatched::from_error(AgentError::internal("boom"));
        assert_eq!(failed.outcome, AuditOutcome::Failed);
    }
}
