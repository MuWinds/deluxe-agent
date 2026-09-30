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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::attachments::ImageRef;
use crate::context::{summary_turn, ContextSettings, ContextWindow};
use crate::error::{AgentError, Result};
use crate::harness::{
    AgentEvent, AgentEventSink, AgentServices, AuditOutcome, HookContext, LlmStreamEvent,
    LlmStreamSink, RunId,
};
use crate::llm::{AssistantTurn, Message, ThinkingLevel, ToolCall, UserTurn};
use crate::tools::ToolOutput;

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

struct TurnStreamSink<'a> {
    turn: &'a mut TurnText,
    last_flush: &'a mut Instant,
    run_id: RunId,
    sink: &'a dyn AgentEventSink,
}

impl LlmStreamSink for TurnStreamSink<'_> {
    fn push(&mut self, event: LlmStreamEvent) {
        match event {
            LlmStreamEvent::Reset => {
                *self.turn = TurnText::default();
                self.sink.emit(AgentEvent::AssistantTurnReset {
                    run_id: self.run_id,
                });
                *self.last_flush = Instant::now();
            }
            LlmStreamEvent::Reasoning(text) => self.turn.reasoning.push_str(&text),
            LlmStreamEvent::Content(text) => self.turn.content.push_str(&text),
        }

        if self.last_flush.elapsed() >= FLUSH_INTERVAL {
            flush_turn(self.turn, self.run_id, self.sink);
            *self.last_flush = Instant::now();
        }
    }
}

pub struct Agent {
    services: Arc<AgentServices>,
    /// The project this agent serves, snapshotted at construction.
    ///
    /// Injected into the tool settings at dispatch, so a relative path and
    /// `exec`'s default cwd belong to the session's project rather than to the
    /// default directory the shared [`ToolSettings`] happens to hold.
    working_directory: PathBuf,
    tools_schema: Value,
    /// Built once at construction by the injected prompt provider.
    system_prompt: String,
    /// The user's context-window configuration: the model's limit and the
    /// share of it that triggers compaction. Read per run, which is when a
    /// fresh measurement window is built from it.
    context_settings: ContextSettings,
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
    /// Creates an agent from runtime ports and an already-rendered prompt.
    ///
    /// Composition code owns prompt snapshots and concrete adapters. The loop
    /// only receives those services and keeps no knowledge of plugins,
    /// registries, HTTP clients, or hook definitions.
    pub fn from_services(
        services: Arc<AgentServices>,
        working_directory: PathBuf,
        context_settings: ContextSettings,
        system_prompt: String,
    ) -> Self {
        let tools = services.tools.descriptors();
        let tools_schema = services.prompts.tool_schema(&tools);
        Self {
            services,
            working_directory,
            tools_schema,
            system_prompt,
            context_settings,
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
        sink: &dyn AgentEventSink,
        run_id: RunId,
        cancel: &CancellationToken,
    ) -> Result<Vec<Message>> {
        let count = messages.len();

        sink.emit(AgentEvent::CompactionStarted {
            run_id,
            dropping: count,
        });
        tracing::info!(
            run_id,
            dropping = count,
            measured_tokens = context.latest_usage().unwrap_or(0),
            "context threshold reached; compacting the history"
        );

        match self.services.context.summarize(&messages, cancel).await {
            Ok(Some(summary)) => {
                // The measurement belonged to the pre-compaction shape and no
                // longer describes what will go out next; the turn that
                // follows re-measures.
                *context = ContextWindow::new(context.settings());
                // The gauge has to forget the old figure too, or it would sit
                // there claiming a token count that describes a conversation
                // which no longer exists.
                sink.emit(AgentEvent::UsageSampled {
                    run_id,
                    measurement: None,
                });
                sink.emit(AgentEvent::Compacted {
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
                if cancel.is_cancelled() {
                    return Err(AgentError::cancelled());
                }
                // Compaction is a mitigation, not the task. A failed summary
                // request is logged and swallowed so the original prompt still
                // goes out; the worst case is the provider refusing it, which
                // the run failure already surfaces.
                tracing::warn!(run_id, %error, "compaction failed; sending the full history");
                sink.emit(AgentEvent::Compacted {
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
        sink: &dyn AgentEventSink,
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
                    .compact_history(messages, &mut context, sink, run_id, &cancel)
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
                sink.emit(AgentEvent::UsageSampled {
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

            sink.emit(AgentEvent::AssistantDone {
                run_id,
                content: turn.content.clone(),
            });

            messages.push(Message::assistant(
                turn.content.clone(),
                turn.tool_calls.clone(),
            ));

            // No tool calls means the model considers the task finished.
            if turn.tool_calls.is_empty() {
                sink.emit(AgentEvent::RunFinished {
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
            let notices = self.services.jobs.drain_notifications();
            if !notices.is_empty() {
                let text = notices.join("\n");
                messages.push(Message::user(text.clone()));
                sink.emit(AgentEvent::Notice { run_id, text });
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
        sink: &dyn AgentEventSink,
        run_id: RunId,
    ) -> Result<AssistantTurn> {
        let mut turn = TurnText::default();
        let mut last_flush = Instant::now();

        let mut stream_sink = TurnStreamSink {
            turn: &mut turn,
            last_flush: &mut last_flush,
            run_id,
            sink,
        };
        let outcome = self
            .services
            .llm
            .stream_turn(
                messages,
                &self.tools_schema,
                thinking,
                cancel,
                &mut stream_sink,
            )
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
        sink: &dyn AgentEventSink,
        run_id: RunId,
    ) -> (String, Vec<ImageRef>) {
        let started = Instant::now();
        let parsed = serde_json::from_str::<Value>(&call.function.arguments);

        // Emitted before the arguments are parsed, so a call whose arguments are
        // not JSON still gets a card in the transcript. It carries the raw text
        // in that case: a failure the user cannot see is worse than an ugly one.
        sink.emit(AgentEvent::ToolStarted {
            run_id,
            call_id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: match &parsed {
                Ok(value) => value.clone(),
                Err(_) => Value::String(call.function.arguments.clone()),
            },
        });

        match parsed {
            Ok(_) => {}
            Err(error) => {
                let message = format!(
                    "Could not parse the arguments for `{}` as JSON: {error}. \
                     Raw arguments were: {}",
                    call.function.name, call.function.arguments
                );
                sink.emit(AgentEvent::ToolFinished {
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
        }

        let dispatched = self.dispatch(call, cancel).await;
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
            let tool_context = self.services.tools.context(&self.working_directory).await;
            let context = HookContext {
                project: self.working_directory.clone(),
                max_output_chars: tool_context.max_output_chars,
            };
            if let Err(error) = self
                .services
                .hooks
                .after_tool(&call.function.name, &mut text, &context, cancel)
                .await
            {
                text.push_str(&format!("\n\nPostToolUse hook failed: {error}"));
            }
        }

        sink.emit(AgentEvent::ToolFinished {
            run_id,
            call_id: call.id.clone(),
            outcome: dispatched.outcome,
            output: text.clone(),
            images: images.clone(),
            hunks,
            duration_ms: started.elapsed().as_millis() as u64,
        });

        (text, images)
    }

    /// Validate → execute → truncate.
    async fn dispatch(&self, call: &ToolCall, cancel: &CancellationToken) -> Dispatched {
        let context = self.services.tools.context(&self.working_directory).await;
        match self.services.tools.execute(call, &context, cancel).await {
            Ok(execution) => Dispatched {
                result: Ok(execution.output),
                outcome: execution.outcome,
            },
            Err(error) => Dispatched::from_error(error),
        }
    }
}

/// Hands the batched fragments of the current turn to the sink, taking them so
/// the periodic flushes never emit the same text twice.
fn flush_turn(turn: &mut TurnText, run_id: RunId, sink: &dyn AgentEventSink) {
    let reasoning = std::mem::take(&mut turn.reasoning);
    if !reasoning.is_empty() {
        sink.emit(AgentEvent::ReasoningDelta {
            run_id,
            text: reasoning,
        });
    }
    let content = std::mem::take(&mut turn.content);
    if !content.is_empty() {
        sink.emit(AgentEvent::AssistantDelta {
            run_id,
            text: content,
        });
    }
}
