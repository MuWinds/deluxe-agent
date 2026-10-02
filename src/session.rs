//! The session model, and its on-disk store.
//!
//! A session is one prompt and everything the agent did about it. It is the unit
//! the sidebar lists, the unit the transcript renders, and the unit that is
//! persisted — so this module is the single definition of that shape.
//!
//! Two decisions worth knowing before editing:
//!
//! * **The project is stored as a lossy `String`, not a `PathBuf`.** serde
//!   serialises a `PathBuf` through `Path::to_str()`, which fails on a path that
//!   is not valid UTF-8. On Windows a folder picked through the native dialog
//!   can contain an unpaired surrogate, and one such path would fail the whole
//!   array — losing every session, not just that one.
//! * **The store is JSON, not TOML.** `Step::Tool.arguments` is an arbitrary
//!   `serde_json::Value` and can contain `null`; TOML has no null, so
//!   `toml::to_string` errors out on perfectly ordinary tool arguments. TOML
//!   stays the right choice for the config file, which a human edits by hand.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::attachments::ImageRef;
use crate::context::summary_turn;
use crate::error::{AgentError, Result};
use crate::harness::SessionStore;
use crate::harness::{AuditOutcome, HunkLines, RunState};
use crate::llm::{FunctionCall, Message, ThinkingLevel, ToolCall, Usage, UserTurn};

/// Bumped when the shape changes in a way worth mentioning in a log line. A
/// mismatch is tolerated rather than fatal — see [`JsonSessionStore::load`].
const VERSION: u32 = 1;

/// How many sessions are kept. Older ones are dropped from the front.
const MAX_SESSIONS: usize = 200;

/// Byte budgets applied before measuring the store's size.
const MAX_STORED_OUTPUT_BYTES: usize = 8_000;
const MAX_STORED_TEXT_BYTES: usize = 16_000;

/// The chain of thought is kept for the transcript but capped harder than the
/// answer: it is the one field that routinely runs to thousands of tokens.
const MAX_STORED_REASONING_BYTES: usize = 6_000;

/// If the store still exceeds this, whole sessions are dropped.
const MAX_STORE_BYTES: usize = 5 * 1024 * 1024;

/// One prompt and the work it produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Stable across restarts, and deliberately not the wire-level `RunId`.
    ///
    /// `RunId` is a per-process correlation token that starts at 1 every launch;
    /// persisting it would collide with the ids of the sessions just loaded from
    /// disk, and a new run's events would be folded into an old session.
    pub id: Uuid,
    /// The working directory this session ran in. See the module docs for why
    /// this is a `String`.
    pub project: String,
    /// Unix seconds. Stored as a number rather than a formatted date so that
    /// ordering needs no calendar arithmetic and no date dependency.
    pub created_at: u64,
    pub state: RunState,
    /// The most recent request's provider usage, when it reported one.
    ///
    /// Never cleared by a later request that reported nothing: the last figure
    /// the provider gave is the honest "most recent" reading, and the cache
    /// tooltip divides it to show the last request's hit rate.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// The context measurement the last run reported: provider prompt tokens
    /// and the message count they describe.
    ///
    /// Carried into the next run so a long conversation's first request is
    /// already guarded, rather than waiting a round-trip to discover it is too
    /// big. `serde(default)` so a session written before the field existed
    /// still loads.
    #[serde(default)]
    pub context_measurement: Option<(u64, usize)>,
    /// Cumulative cache counters for this conversation, summed over every
    /// request whose provider reported a cache figure.
    ///
    /// Stored as raw totals rather than a rate so the weighting survives: the
    /// rate is `cache_hit_tokens / cache_measured_tokens`, token-weighted, and
    /// recomputing it from totals never compounds a rounding error. `serde(default)`
    /// so a session written before the fields existed loads as "nothing measured".
    #[serde(default)]
    pub cache_hit_tokens: u64,
    #[serde(default)]
    pub cache_measured_tokens: u64,
    /// The reasoning effort this conversation asks for, or `None` for "send no
    /// parameter". A property of the conversation, chosen in the composer like
    /// pi's shift+tab indicator, so it is stored beside the transcript rather
    /// than in the global model settings.
    ///
    /// `serde(default)` so a session written before the field existed loads as
    /// "no parameter", the only safe reading of "the user never chose one".
    #[serde(default)]
    pub thinking: Option<ThinkingLevel>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// One line of the transcript.
///
/// Struct variants rather than newtypes: serde's internally-tagged representation
/// cannot serialise a newtype variant that holds a bare `String`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Step {
    User {
        text: String,
        /// Images attached to this prompt, as durable references. Empty for
        /// every prompt the user typed without one.
        ///
        /// `serde(default)` is what keeps a session written before the field
        /// existed loading: an old transcript simply has no attachments.
        #[serde(default)]
        images: Vec<ImageRef>,
    },
    Assistant {
        text: String,
    },
    /// The model's chain of thought for the turn that follows it.
    ///
    /// Rendered collapsed, like a tool card. Separate from `Assistant` so the
    /// answer stays clean and the reasoning can be folded away as a whole.
    Reasoning {
        /// Assigned once, when the step is created — never re-derived.
        ///
        /// The block's identity has to be stable while it streams, because the
        /// transcript view keys the fold's open/closed state on it. Deriving the
        /// key from the text (as an earlier version did) meant the key changed
        /// with every streamed fragment, and the UI could not hold a block open
        /// until the model stopped thinking.
        id: Uuid,
        text: String,
    },
    Tool {
        call_id: String,
        name: String,
        arguments: Value,
        /// The raw JSON text the model emitted for the call, kept verbatim.
        ///
        /// `arguments` is the parsed form the transcript renders; replaying
        /// *that* would re-serialise it — reordering keys and dropping
        /// whitespace — and the prompt prefix would diverge from the model's
        /// own turn onward, invalidating the provider's cache. `serde(default)`
        /// so a session written before the field existed still loads; its
        /// replay falls back to re-serialising the parsed form.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_arguments: Option<String>,
        #[serde(default)]
        result: Option<ToolResult>,
    },
    Notice {
        text: String,
    },
    /// A host-authored message delivered to the model as a `user` turn.
    ///
    /// Distinct from [`Step::Notice`]: a notice is transcript-only, but this
    /// one was on the wire when the run sent it — the agent loop's inbox for a
    /// background job that settled mid-run — so the replay must reproduce it,
    /// or every later turn's prefix shifts.
    HostMessage {
        text: String,
    },
    /// A context compaction: the older turns were folded into one summary,
    /// while the turns after this marker were kept verbatim.
    ///
    /// Recorded so the transcript shows why the older turns vanished. It is
    /// never replayed to the model, which receives the summary itself as the
    /// turn it continues from; everything *after* the marker still replays.
    Compaction {
        summary: String,
    },
}

/// How a tool call ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub outcome: AuditOutcome,
    pub output: String,
    /// Images the call produced, as durable references. Empty for every tool
    /// but `read_image`.
    ///
    /// `serde(default)` is not cosmetic: a session written before this field
    /// existed must still load, and the store deliberately tolerates a shape it
    /// does not recognise rather than discarding every session.
    #[serde(default)]
    pub images: Vec<ImageRef>,
    pub duration_ms: u64,
    /// Real file line numbers an `apply_patch` call resolved its hunks to,
    /// captured when the tool read the target. Empty for every other tool.
    ///
    /// `serde(default)` keeps a session written before this field existed
    /// loading; the store's tolerance for unknown shapes already covers the rest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<HunkLines>,
}

/// Appends to a trailing reasoning step, or starts one.
///
/// A turn's chain of thought arrives as many fragments, exactly like the answer
/// does, and must land in one foldable block rather than one per fragment.
/// Starting an assistant answer or a tool call below it closes the reasoning off
/// for good, because neither ever appends to it.
///
/// A free function over a bare `Vec<Step>` rather than a method, because two
/// transcripts fold the same events: a session's own, and a delegated
/// sub-agent's, which the window keeps as a plain step list — its events never
/// reach a session.
pub fn push_reasoning(steps: &mut Vec<Step>, text: &str) {
    match steps.last_mut() {
        Some(Step::Reasoning { text: existing, .. }) => existing.push_str(text),
        _ => steps.push(Step::Reasoning {
            // The id is minted by the step, not by its contents: a key that
            // changes as the text grows cannot hold a fold open.
            id: Uuid::new_v4(),
            text: text.to_string(),
        }),
    }
}

/// Appends to the trailing assistant step, or starts one.
///
/// A turn arrives as many `AssistantDelta` events, and they must land in one
/// bubble rather than one per fragment.
pub fn push_assistant(steps: &mut Vec<Step>, text: &str) {
    match steps.last_mut() {
        Some(Step::Assistant { text: existing }) => existing.push_str(text),
        _ => steps.push(Step::Assistant {
            text: text.to_string(),
        }),
    }
}

/// Removes the trailing reasoning and answer fragments of an incomplete turn.
///
/// A failed streamed request can already have emitted live deltas; its retry
/// must start with a clean transcript while leaving earlier turns and tool
/// results untouched.
pub fn reset_turn(steps: &mut Vec<Step>) {
    while matches!(
        steps.last(),
        Some(Step::Assistant { .. } | Step::Reasoning { .. })
    ) {
        steps.pop();
    }
}

/// Adds a completed turn's text, unless the streamed deltas already built it.
///
/// A provider that answers in one piece, with no streaming chunks, emits no
/// `AssistantDelta` — without this the turn would never appear at all. One that
/// streams has already pushed the same text through [`push_assistant`], so
/// pushing again would show the answer twice.
pub fn push_answer(steps: &mut Vec<Step>, content: &str) {
    if content.is_empty() {
        return;
    }
    let already_present = matches!(steps.last(), Some(Step::Assistant { text }) if text == content);
    if !already_present {
        steps.push(Step::Assistant {
            text: content.to_string(),
        });
    }
}

/// Records a tool call, with its result still to come.
///
/// `raw_arguments` is the model's own JSON text, kept so the replay can send
/// the same bytes the live run did; `None` only for a caller that has no raw
/// form (a legacy transcript), which replays by re-serialising `arguments`.
pub fn push_tool(
    steps: &mut Vec<Step>,
    call_id: String,
    name: String,
    arguments: Value,
    raw_arguments: Option<String>,
) {
    steps.push(Step::Tool {
        call_id,
        name,
        arguments,
        raw_arguments,
        result: None,
    });
}

/// Attaches a result to the call it belongs to.
///
/// Searched from the back because the matching call is almost always the one
/// just added. Takes a slice, not a `Vec`: it only ever rewrites a step that is
/// already there.
pub fn finish_tool(steps: &mut [Step], call_id: &str, result: ToolResult) {
    for step in steps.iter_mut().rev() {
        if let Step::Tool {
            call_id: id,
            result: slot,
            ..
        } = step
        {
            if id == call_id {
                *slot = Some(result);
                return;
            }
        }
    }
    tracing::warn!(call_id, "a tool result arrived for an unknown call");
}

impl Session {
    /// A fresh session rooted at `project`.
    ///
    /// Starts in [`RunState::Running`], because a session only exists once its
    /// first prompt is in flight — `App::start_run` creates it as it dispatches
    /// the turn, so there is no window in which it is idle-but-saved.
    pub fn new(project: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            project: project.into(),
            created_at: now_unix(),
            state: RunState::Running,
            usage: None,
            context_measurement: None,
            cache_hit_tokens: 0,
            cache_measured_tokens: 0,
            thinking: None,
            steps: Vec::new(),
        }
    }

    /// Folds one request's usage into the conversation's cache counters.
    ///
    /// Only requests that reported *both* a prompt size and a cache figure are
    /// counted. A provider that never mentions caching would otherwise inflate
    /// the denominator with its whole prompt and report a hit rate of 0% —
    /// which reads as a broken cache rather than an unreported one. A reported
    /// zero is counted, because a cold first request genuinely misses.
    pub fn record_cache_usage(&mut self, usage: Option<&Usage>) {
        let Some(usage) = usage else {
            return;
        };
        let (Some(prompt), Some(cached)) = (usage.prompt_tokens(), usage.cached_prompt_tokens())
        else {
            return;
        };
        if prompt == 0 {
            return;
        }
        self.cache_hit_tokens = self.cache_hit_tokens.saturating_add(cached.min(prompt));
        self.cache_measured_tokens = self.cache_measured_tokens.saturating_add(prompt);
    }

    /// The conversation's token-weighted cache hit rate, in `0.0..=1.0`.
    ///
    /// `None` until at least one request reported a cache figure.
    pub fn cache_hit_rate(&self) -> Option<f32> {
        if self.cache_measured_tokens == 0 {
            return None;
        }
        Some((self.cache_hit_tokens as f32 / self.cache_measured_tokens as f32).clamp(0.0, 1.0))
    }

    /// Appends to the trailing reasoning step, or starts one.
    ///
    /// See [`push_reasoning`], which holds the logic and serves the sub-agent
    /// transcripts too.
    pub fn push_reasoning(&mut self, text: &str) {
        push_reasoning(&mut self.steps, text);
    }

    /// What the sidebar shows for this session.
    ///
    /// Derived from the first user step rather than stored separately: a second
    /// copy of the same text is a second thing that can go stale.
    pub fn title(&self) -> String {
        for step in &self.steps {
            if let Step::User { text, images } = step {
                // A prompt can be an image with no words, so an empty text is
                // not "no title": fall back to how many pictures it carried
                // rather than showing the empty-session placeholder.
                if !text.trim().is_empty() {
                    return text.clone();
                }
                if !images.is_empty() {
                    return format!("[{} 张图片]", images.len());
                }
            }
        }
        "（空会话）".to_string()
    }

    /// The last path component, which is what the project list shows.
    ///
    /// Splits on both separators instead of using `Path::file_name`, so a
    /// session written on Windows still shows a bare name when it is opened on
    /// a Unix host.
    pub fn project_name(&self) -> String {
        self.project
            .rsplit(['/', '\\'])
            .find(|component| !component.is_empty())
            .unwrap_or(self.project.as_str())
            .to_string()
    }

    /// Appends to the trailing assistant step, or starts one.
    ///
    /// See [`push_assistant`], which holds the logic.
    pub fn push_assistant(&mut self, text: &str) {
        push_assistant(&mut self.steps, text);
    }

    /// See [`push_tool`], which holds the logic.
    pub fn push_tool(
        &mut self,
        call_id: String,
        name: String,
        arguments: Value,
        raw_arguments: Option<String>,
    ) {
        push_tool(&mut self.steps, call_id, name, arguments, raw_arguments);
    }

    /// See [`finish_tool`], which holds the logic.
    pub fn finish_tool(&mut self, call_id: &str, result: ToolResult) {
        finish_tool(&mut self.steps, call_id, result);
    }

    /// The full transcript as plain text, for the clipboard.
    pub fn as_text(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            match step {
                Step::User { text, .. } => {
                    out.push_str("## 你\n\n");
                    out.push_str(text);
                }
                Step::Assistant { text } => {
                    out.push_str("## Agent\n\n");
                    out.push_str(text);
                }
                Step::Reasoning { text, .. } => {
                    out.push_str("## Agent（思考）\n\n");
                    out.push_str(text);
                }
                Step::Tool {
                    name,
                    arguments,
                    result,
                    ..
                } => {
                    out.push_str(&format!("### 工具 {name}\n\n"));
                    out.push_str(&format!(
                        "参数：{}\n\n",
                        serde_json::to_string_pretty(arguments)
                            .unwrap_or_else(|_| arguments.to_string())
                    ));
                    if let Some(result) = result {
                        out.push_str(&format!("结论：{}\n\n", result.outcome.label()));
                        out.push_str(&result.output);
                    }
                }
                Step::Notice { text } => {
                    out.push_str("## 失败\n\n");
                    out.push_str(text);
                }
                Step::HostMessage { text } => {
                    out.push_str("## 系统通知\n\n");
                    out.push_str(text);
                }
                Step::Compaction { summary } => {
                    out.push_str("## 上下文压缩\n\n");
                    out.push_str(summary);
                }
            }
            out.push_str("\n\n");
        }
        out
    }

    /// The conversation so far, in the shape the model API expects.
    ///
    /// This is what makes a follow-up prompt continue the session rather than
    /// start over: the GUI derives it from the very transcript it displays and
    /// hands it to the worker with the next `Cmd::Run`.
    ///
    /// Reasoning is deliberately skipped — the chain of thought is shown to the
    /// user and never replayed (the models that emit it expect their last turn
    /// to carry the answer only, and it is not part of the visible transcript).
    /// A notice is a message to the user, not something to be reasoned over, so
    /// a failed run contributes nothing to the replay either. What survives is
    /// exactly the user / assistant / tool sequence the agent loop would have
    /// built had the whole session happened inside one run.
    ///
    /// A compaction cuts the replay short. Everything ahead of the last marker
    /// was folded into that marker's brief, so replaying those turns would both
    /// contradict the brief and undo the compaction — the very next prompt
    /// would cross the threshold again. The turns *after* the marker are the
    /// tail the compacting run kept verbatim, and they replay in full. The
    /// marker itself replays as [`summary_turn`], the same message the
    /// compacting run carried on from, so a follow-up's history is the prefix
    /// that run's own messages had. This is what makes a compaction outlive the
    /// run that produced it.
    pub fn to_messages(&self) -> Vec<Message> {
        let Some(index) = self.last_compaction() else {
            return replay(&self.steps);
        };
        let Step::Compaction { summary } = &self.steps[index] else {
            unreachable!("last_compaction only ever names a compaction step");
        };

        let mut messages = vec![summary_turn(summary)];
        messages.extend(replay(&self.steps[index + 1..]));
        messages
    }

    /// Records a compaction whose brief replaces everything before the last
    /// `keep` replayed messages, inserting the marker at that boundary.
    ///
    /// The marker must land *before* the kept tail, not at the end: the replay
    /// cuts at the last non-empty marker, so a marker appended after the tail
    /// would drop the very turns the compacting run kept. A marker with no
    /// summary — a failed compaction — is appended instead, because nothing
    /// was actually cut.
    pub fn record_compaction(&mut self, summary: String, keep: usize) {
        if summary.trim().is_empty() || keep == 0 {
            self.steps.push(Step::Compaction { summary });
            return;
        }

        // The current surface begins after the last brief that actually
        // produced one; an empty marker does not cut anything.
        let start = self.last_compaction().map_or(0, |index| index + 1);
        let plan = replay_plan(&self.steps[start..]);

        if keep >= plan.len() {
            // The whole surface is the kept tail: the run folded only the
            // previous brief. The new marker goes right after that brief, so
            // it is the last marker and the whole surface still replays after
            // it. With no previous marker this cannot happen — a kept tail is
            // always shorter than the surface.
            if start == 0 {
                self.steps.push(Step::Compaction { summary });
                return;
            }
            self.steps.insert(start, Step::Compaction { summary });
            return;
        }

        let boundary = start + plan[plan.len() - keep].1;
        // The agent cuts on a user turn, so this holds for any compaction it
        // announced; a mismatch means the boundary is unusable, and folding
        // nothing is safer than orphaning a tool result.
        if !matches!(
            self.steps[boundary],
            Step::User { .. } | Step::HostMessage { .. }
        ) {
            self.steps.push(Step::Compaction { summary });
            return;
        }
        self.steps.insert(boundary, Step::Compaction { summary });
    }

    /// Where the last compaction that actually produced a brief sits.
    ///
    /// A marker carrying no summary is a compaction that failed: that run went
    /// out with its history intact, so the replay must keep it too.
    fn last_compaction(&self) -> Option<usize> {
        self.steps.iter().rposition(
            |step| matches!(step, Step::Compaction { summary } if !summary.trim().is_empty()),
        )
    }
}

/// One replayed message and the index of the step it came from.
///
/// The index is what [`Session::record_compaction`] uses to find where a kept
/// tail begins: the compaction marker is inserted before the step that
/// produced the first kept message.
type Replayed = (Message, usize);

/// The user / assistant / tool / host-message turns among `steps`, in order,
/// each paired with the step it came from.
///
/// Everything else — reasoning, notices, compaction markers — is transcript
/// only and has no place on the wire.
///
/// A turn's prose and the calls it asked for live in separate steps here: the
/// answer is one [`Step::Assistant`], each call that followed it a
/// [`Step::Tool`]. The wire wants them joined — an assistant message carries
/// `tool_calls`, and each `tool` message answers one of them by id — so the
/// pairing is rebuilt. Emitting the results as bare `tool` messages instead
/// leaves every one of them answering nothing, which the provider rejects
/// outright: *Messages with role 'tool' must be a response to a preceding
/// message with 'tool_calls'*.
///
/// A call the run never answered — the process died mid-tool — is still given a
/// result, because a `tool_calls` entry with no reply is the same rejection
/// from the other side.
fn replay_plan(steps: &[Step]) -> Vec<Replayed> {
    let mut plan = Vec::new();
    let mut index = 0;

    while index < steps.len() {
        match &steps[index] {
            Step::User { text, images } => {
                plan.push((
                    Message::user_turn(UserTurn {
                        text: text.clone(),
                        images: images.clone(),
                    }),
                    index,
                ));
                index += 1;
            }
            // The prose of a turn. Any tool steps immediately after it are the
            // calls it made, so the two go out as one assistant message plus
            // its results.
            Step::Assistant { text } => {
                let tools = tool_run(steps, index + 1);
                if tools.is_empty() {
                    if !text.is_empty() {
                        plan.push((Message::assistant(text.clone(), Vec::new()), index));
                    }
                    index += 1;
                } else {
                    push_tool_turn(&mut plan, text, tools, index + 1, index);
                    index += 1 + tools.len();
                }
            }
            // A call whose turn wrote no prose: there is no assistant step to
            // hang it on, so one is synthesised with an empty body — the shape
            // a tool-only turn has on the wire.
            Step::Tool { .. } => {
                let tools = tool_run(steps, index);
                push_tool_turn(&mut plan, "", tools, index, index);
                index += tools.len();
            }
            // A host message was a `user` turn on the wire when the run sent
            // it, so the replay reproduces it — unlike a transcript-only
            // notice, which the run never sent to the model.
            Step::HostMessage { text } => {
                plan.push((Message::user(text.clone()), index));
                index += 1;
            }
            _ => index += 1,
        }
    }

    plan
}

/// The replayed messages alone, in order.
fn replay(steps: &[Step]) -> Vec<Message> {
    replay_plan(steps)
        .into_iter()
        .map(|(message, _)| message)
        .collect()
}

/// The consecutive [`Step::Tool`]s starting at `start`.
fn tool_run(steps: &[Step], start: usize) -> &[Step] {
    let mut end = start;
    while end < steps.len() && matches!(steps[end], Step::Tool { .. }) {
        end += 1;
    }
    &steps[start..end]
}

/// Emits one assistant message carrying `tools`' calls, then a `tool` message
/// per call.
///
/// `text` is the turn's prose, or empty for a turn that only called tools.
/// `base` is the position of the first call within the replayed slice, used to
/// mint a stable id for a call the transcript never recorded one for and to
/// record each call's originating step. `assistant_step` is the step the
/// assistant message belongs to: the prose step when there is one, else the
/// first call's step.
fn push_tool_turn(
    plan: &mut Vec<Replayed>,
    text: &str,
    tools: &[Step],
    base: usize,
    assistant_step: usize,
) {
    // Pair every call with the id its result must name. The id is minted only
    // when the transcript has none — a session written before it was kept — and
    // it is derived from the call's position rather than randomised: a fresh
    // UUID every replay would change the message bytes on every run and throw
    // away the provider's prefix cache from this turn onward.
    let calls: Vec<(String, &Step)> = tools
        .iter()
        .enumerate()
        .filter_map(|(offset, tool)| match tool {
            Step::Tool { call_id, .. } => Some((
                if call_id.is_empty() {
                    format!("call_replay_{}", base + offset)
                } else {
                    call_id.clone()
                },
                tool,
            )),
            _ => None,
        })
        .collect();

    let tool_calls = calls
        .iter()
        .filter_map(|(id, tool)| match tool {
            Step::Tool {
                name,
                arguments,
                raw_arguments,
                ..
            } => Some(ToolCall {
                id: id.clone(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: name.clone(),
                    // Prefer the model's own bytes; only a legacy transcript
                    // without them falls back to re-serialising the parsed
                    // form, which reorders keys and would shift the prefix.
                    arguments: raw_arguments.clone().unwrap_or_else(|| {
                        serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into())
                    }),
                },
            }),
            _ => None,
        })
        .collect();

    plan.push((
        Message::assistant(text.to_string(), tool_calls),
        assistant_step,
    ));

    for (offset, (id, tool)) in calls.into_iter().enumerate() {
        let Step::Tool { result, .. } = tool else {
            continue;
        };
        let message = match result {
            // Replayed images are what make a follow-up question about one
            // still work: the reference is resolved back into the same picture
            // on every later turn.
            Some(result) => Message::tool_with_images(id, result.output.clone(), &result.images),
            None => Message::tool(id, "The tool call did not complete before the run ended."),
        };
        plan.push((message, base + offset));
    }
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// A coarse "how long ago", which is what a chat list shows and what needs no
/// calendar arithmetic.
pub fn age_label(created_at: u64, now: u64) -> String {
    match now.saturating_sub(created_at) {
        seconds if seconds < 60 => "刚刚".into(),
        seconds if seconds < 3_600 => format!("{} 分钟前", seconds / 60),
        seconds if seconds < 86_400 => format!("{} 小时前", seconds / 3_600),
        seconds if seconds < 2_592_000 => format!("{} 天前", seconds / 86_400),
        seconds => format!("{} 个月前", seconds / 2_592_000),
    }
}

/// Where the store lives, next to `config.toml`.
pub fn store_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join("sessions.json"))
}

pub struct JsonSessionStore {
    path: Option<PathBuf>,
}

impl JsonSessionStore {
    /// Creates a JSON store targeting `path`, or an unavailable store when it is `None`.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }
}

#[async_trait::async_trait]
impl SessionStore for JsonSessionStore {
    async fn load(&self) -> Result<Vec<Session>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || load_at(path.as_deref()))
            .await
            .map_err(|error| AgentError::internal(format!("Session store worker failed: {error}")))
    }

    async fn save(&self, sessions: &[Session]) -> Result<()> {
        let path = self.path.clone().ok_or_else(|| {
            AgentError::internal("No config directory is available on this system")
        })?;
        let sessions = sessions.to_vec();
        tokio::task::spawn_blocking(move || save_at(&path, &sessions))
            .await
            .map_err(|error| {
                AgentError::internal(format!("Session store worker failed: {error}"))
            })?
    }
}

/// Reads the store, falling back to an empty list.
///
/// A malformed or missing file is a warning, never a startup failure: refusing
/// to open the window because the history is unreadable would be a trap, and the
/// history is the part of this app that is safe to lose.
///
/// The sessions come back oldest-first, which is the order the store writes.
fn load_at(path: Option<&Path>) -> Vec<Session> {
    let Some(path) = path else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    match serde_json::from_str::<Store>(&text) {
        Ok(store) => {
            // A mismatch is logged but not fatal. Every field that might be
            // missing carries `#[serde(default)]`, so an older file still loads,
            // and dropping the user's history over a version number would be a
            // far worse outcome than reading it slightly wrong.
            if store.version != VERSION {
                tracing::warn!(
                    found = store.version,
                    expected = VERSION,
                    "session store was written by another version; reading it anyway"
                );
            }
            settle_interrupted(store.sessions)
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring unreadable session store");
            Vec::new()
        }
    }
}

/// Marks any session still claiming to be running as interrupted.
///
/// No run outlives the process — `RunId` restarts at 1 on every launch and the
/// app begins with no active run — so a `Running` state read from disk is
/// always the residue of a crash or of the window closing mid-run. Left alone it
/// would pulse a running dot in the sidebar forever, next to a transcript that
/// stops mid-sentence with no explanation.
fn settle_interrupted(mut sessions: Vec<Session>) -> Vec<Session> {
    for session in &mut sessions {
        if session.state == RunState::Running {
            session.state = RunState::Failed;
            session.steps.push(Step::Notice {
                text: "上次运行被中断（应用已关闭）".into(),
            });
        }
    }
    sessions
}

/// Writes the store, dropping the oldest sessions until it fits the budget.
fn save_at(path: &Path, sessions: &[Session]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AgentError::from_io("Failed to create the config directory", error))?;
    }

    let text = encode(sessions)?;

    // The temp file goes in the same directory as the target: `rename` is only
    // atomic within a filesystem, and across one it simply fails.
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text)
        .map_err(|error| AgentError::from_io("Failed to write the session store", error))?;

    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        AgentError::from_io("Failed to replace the session store", error)
    })
}

/// Renders the store, trimming payloads and then whole sessions until it fits.
fn encode(sessions: &[Session]) -> Result<String> {
    let start = sessions.len().saturating_sub(MAX_SESSIONS);
    let mut kept: Vec<Session> = sessions[start..].to_vec();
    for session in &mut kept {
        trim_payloads(session);
    }

    loop {
        let text = serde_json::to_string(&StoreRef {
            version: VERSION,
            sessions: &kept,
        })
        .map_err(|error| {
            AgentError::internal(format!("Failed to serialise the session store: {error}"))
        })?;

        // Halving rather than dropping one at a time: re-measuring the whole
        // array per dropped session is quadratic, and this runs on the GUI
        // thread where a stall is visible.
        if text.len() <= MAX_STORE_BYTES || kept.len() <= 1 {
            return Ok(text);
        }

        let drop_count = (kept.len() / 2).max(1);
        tracing::warn!(
            dropping = drop_count,
            bytes = text.len(),
            "session store is over budget; dropping the oldest sessions"
        );
        kept.drain(..drop_count);
    }
}

/// Caps the fields that can grow without bound, in place.
fn trim_payloads(session: &mut Session) {
    for step in &mut session.steps {
        match step {
            Step::User { text, .. }
            | Step::Assistant { text }
            | Step::Notice { text }
            | Step::HostMessage { text } => {
                cap_bytes(text, MAX_STORED_TEXT_BYTES);
            }
            Step::Reasoning { text, .. } => {
                cap_bytes(text, MAX_STORED_REASONING_BYTES);
            }
            Step::Tool { result, .. } => {
                if let Some(result) = result {
                    cap_bytes(&mut result.output, MAX_STORED_OUTPUT_BYTES);
                }
            }
            Step::Compaction { summary } => {
                cap_bytes(summary, MAX_STORED_TEXT_BYTES);
            }
        }
    }
}

/// Truncates `text` in place to at most `max` bytes, never splitting a
/// character.
fn cap_bytes(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push('…');
}

/// The shape read from disk.
#[derive(Deserialize)]
struct Store {
    version: u32,
    #[serde(default)]
    sessions: Vec<Session>,
}

/// The shape written to disk. Borrows so that re-measuring the shrinking list
/// does not clone it each time round.
#[derive(Serialize)]
struct StoreRef<'a> {
    version: u32,
    sessions: &'a [Session],
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session_with(steps: Vec<Step>) -> Session {
        Session {
            id: Uuid::new_v4(),
            project: "/w".into(),
            created_at: 0,
            state: RunState::Finished,
            usage: None,
            context_measurement: None,
            cache_hit_tokens: 0,
            cache_measured_tokens: 0,
            thinking: None,
            steps,
        }
    }

    #[tokio::test]
    async fn json_session_store_round_trips_in_a_temporary_directory() {
        let dir = tempfile::tempdir().expect("a temp directory is available");
        let path = dir.path().join("sessions.json");
        let store = JsonSessionStore::new(Some(path.clone()));
        let mut session = session_with(vec![Step::User {
            text: "persist this".into(),
            images: Vec::new(),
        }]);
        session.state = RunState::Running;

        store
            .save(&[session.clone()])
            .await
            .expect("the snapshot is written");
        let loaded = store.load().await.expect("the snapshot is read");

        assert_eq!(loaded.len(), 1, "one saved session is restored");
        assert_eq!(loaded[0].id, session.id, "the durable id is preserved");
        assert_eq!(loaded[0].title(), "persist this");
        assert_eq!(
            loaded[0].state,
            RunState::Failed,
            "a run cannot still be active after a restart"
        );
        assert!(
            matches!(loaded[0].steps.last(), Some(Step::Notice { text }) if text.contains("中断")),
            "the interrupted run is explained in the transcript"
        );
        assert!(path.is_file(), "the adapter writes to its injected path");
    }

    #[tokio::test]
    async fn json_session_store_without_a_path_refuses_saves() {
        let store = JsonSessionStore::new(None);

        assert!(
            store.save(&[]).await.is_err(),
            "an unavailable store cannot claim that a snapshot was saved"
        );
        assert!(
            store
                .load()
                .await
                .expect("an unavailable store reads empty")
                .is_empty(),
            "loading without a configured path remains non-fatal"
        );
    }

    #[test]
    fn the_step_helpers_fold_a_bare_transcript_the_way_a_session_does() {
        // The same folding serves a sub-agent's transcript, which is a plain
        // step list: fragments of one turn must land in one step, not one each.
        let mut steps = Vec::new();

        push_reasoning(&mut steps, "weighing ");
        push_reasoning(&mut steps, "options");
        push_assistant(&mut steps, "the ");
        push_assistant(&mut steps, "answer");
        push_tool(
            &mut steps,
            "call-1".into(),
            "read_file".into(),
            json!({}),
            None,
        );
        finish_tool(
            &mut steps,
            "call-1",
            ToolResult {
                outcome: AuditOutcome::Executed,
                output: "contents".into(),
                images: Vec::new(),
                duration_ms: 3,
                hunks: Vec::new(),
            },
        );

        assert_eq!(steps.len(), 3, "one step per turn, not per fragment");
        assert!(matches!(&steps[0], Step::Reasoning { text, .. } if text == "weighing options"));
        assert!(matches!(&steps[1], Step::Assistant { text } if text == "the answer"));
        assert!(matches!(
            &steps[2],
            Step::Tool { result: Some(result), .. } if result.output == "contents"
        ));
    }

    #[test]
    fn a_completed_turn_is_not_added_twice() {
        // A streaming provider already pushed the text through `push_assistant`;
        // the `AssistantDone` that follows carries the same text and must not
        // add a second bubble.
        let mut steps = Vec::new();
        push_assistant(&mut steps, "the answer");
        push_answer(&mut steps, "the answer");
        assert_eq!(steps.len(), 1, "the streamed turn was already there");

        // A provider that answers in one piece emits no delta at all, so this is
        // the only thing that puts its turn on screen.
        push_answer(&mut steps, "a second turn");
        assert_eq!(steps.len(), 2);
        assert!(matches!(&steps[1], Step::Assistant { text } if text == "a second turn"));
    }

    #[test]
    fn resetting_a_turn_removes_only_its_trailing_streamed_output() {
        let mut steps = vec![
            Step::Assistant {
                text: "earlier answer".into(),
            },
            Step::User {
                text: "new request".into(),
                images: Vec::new(),
            },
            Step::Reasoning {
                id: Uuid::new_v4(),
                text: "partial thought".into(),
            },
            Step::Assistant {
                text: "partial answer".into(),
            },
        ];

        reset_turn(&mut steps);

        assert_eq!(
            steps.len(),
            2,
            "earlier turns and the current prompt remain"
        );
        assert!(matches!(&steps[0], Step::Assistant { text } if text == "earlier answer"));
        assert!(matches!(&steps[1], Step::User { text, .. } if text == "new request"));
    }

    #[test]
    fn the_title_comes_from_the_first_user_step() {
        let session = session_with(vec![
            Step::Assistant {
                text: "hello".into(),
            },
            Step::User {
                text: "fix the bug".into(),
                images: Vec::new(),
            },
        ]);
        assert_eq!(session.title(), "fix the bug");
    }

    #[test]
    fn to_messages_replays_the_conversation_in_order() {
        let session = session_with(vec![
            Step::User {
                text: "list the directory".into(),
                images: Vec::new(),
            },
            Step::Reasoning {
                id: Uuid::new_v4(),
                text: "thinking out loud".into(),
            },
            Step::Assistant {
                text: "I will look.".into(),
            },
            Step::Tool {
                call_id: "call_1".into(),
                name: "list_dir".into(),
                arguments: serde_json::json!({}),
                raw_arguments: None,
                result: Some(ToolResult {
                    outcome: AuditOutcome::Executed,
                    output: "empty".into(),
                    images: Vec::new(),
                    hunks: Vec::new(),
                    duration_ms: 3,
                }),
            },
            Step::Assistant {
                text: "it is empty".into(),
            },
        ]);

        let messages = session.to_messages();
        let rendered: Vec<(String, String)> = messages
            .iter()
            .map(|message| (message.role.clone(), message.text()))
            .collect();

        assert_eq!(
            rendered,
            vec![
                ("user".into(), "list the directory".into()),
                ("assistant".into(), "I will look.".into()),
                ("tool".into(), "empty".into()),
                ("assistant".into(), "it is empty".into()),
            ]
        );

        // The prose and its call are one assistant turn on the wire: the
        // result answers a call that turn actually carries, or the provider
        // rejects the whole request.
        let calls = messages[1]
            .tool_calls
            .as_ref()
            .expect("the turn carries its call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "list_dir");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn to_messages_skips_reasoning_and_answers_an_interrupted_call() {
        let session = session_with(vec![
            Step::Reasoning {
                id: Uuid::new_v4(),
                text: "private".into(),
            },
            Step::Assistant {
                text: "calling a tool".into(),
            },
            Step::Tool {
                call_id: "call_1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({}),
                raw_arguments: None,
                result: None,
            },
            Step::Notice {
                text: "上次运行被中断".into(),
            },
        ]);

        let messages = session.to_messages();
        // Reasoning is never sent back and a notice is for the user, so neither
        // replays. The call that never finished still needs a reply — an
        // assistant `tool_calls` entry with no `tool` message is rejected just
        // as an orphan `tool` message is — so its result is synthesised.
        let roles: Vec<&str> = messages
            .iter()
            .map(|message| message.role.as_str())
            .collect();
        assert_eq!(roles, vec!["assistant", "tool"]);
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
        assert!(
            messages[1].text().contains("did not complete"),
            "the interrupted call gets an honest reply: {}",
            messages[1].text()
        );
    }

    #[test]
    fn to_messages_hangs_a_tool_only_turn_on_a_synthesised_assistant() {
        // A turn that calls a tool without any prose leaves no `Step::Assistant`
        // behind, so the call would otherwise be an orphan with nothing to
        // answer.
        let session = session_with(vec![
            Step::User {
                text: "read it".into(),
                images: Vec::new(),
            },
            Step::Tool {
                call_id: "call_7".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({ "path": "a.txt" }),
                raw_arguments: None,
                result: Some(ToolResult {
                    outcome: AuditOutcome::Executed,
                    output: "42".into(),
                    images: Vec::new(),
                    hunks: Vec::new(),
                    duration_ms: 1,
                }),
            },
        ]);

        let messages = session.to_messages();
        let roles: Vec<&str> = messages
            .iter()
            .map(|message| message.role.as_str())
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "tool"]);

        let calls = messages[1]
            .tool_calls
            .as_ref()
            .expect("the turn carries its call");
        assert_eq!(calls[0].id, "call_7");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_7"));
    }

    #[test]
    fn replay_sends_the_models_own_tool_argument_bytes() {
        // A model emits keys in schema order and with its own spacing; the
        // transcript also keeps the parsed form. Replaying the parsed form
        // would re-serialise to alphabetical keys and shift the prompt prefix
        // from this call onward, so the raw text must win.
        let raw = r#"{"startLine":1,"path":"a.txt"}"#;
        let session = session_with(vec![
            Step::User {
                text: "read it".into(),
                images: Vec::new(),
            },
            Step::Assistant {
                text: "on it".into(),
            },
            Step::Tool {
                call_id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({ "startLine": 1, "path": "a.txt" }),
                raw_arguments: Some(raw.into()),
                result: None,
            },
        ]);

        let messages = session.to_messages();
        let call = &messages[1].tool_calls.as_ref().expect("the call replays")[0];
        assert_eq!(call.function.arguments, raw);
        // The canonical form would have sorted the keys; prove we did not.
        assert_eq!(
            serde_json::to_string(&json!({ "startLine": 1, "path": "a.txt" })).unwrap(),
            r#"{"path":"a.txt","startLine":1}"#
        );
        assert_ne!(call.function.arguments, r#"{"path":"a.txt","startLine":1}"#);
    }

    #[test]
    fn a_legacy_call_without_raw_bytes_falls_back_to_reserialising() {
        let session = session_with(vec![Step::Tool {
            call_id: "call_1".into(),
            name: "read_file".into(),
            arguments: json!({ "path": "a.txt" }),
            raw_arguments: None,
            result: None,
        }]);

        let messages = session.to_messages();
        let call = &messages[0].tool_calls.as_ref().expect("the call replays")[0];
        assert_eq!(call.function.arguments, r#"{"path":"a.txt"}"#);
    }

    #[test]
    fn a_call_with_no_recorded_id_replays_a_stable_id() {
        // Legacy sessions kept no id; minting a fresh UUID per replay would
        // change the message bytes on every run. Deriving it from the call's
        // position makes two replays agree.
        let session = session_with(vec![Step::Tool {
            call_id: String::new(),
            name: "read_file".into(),
            arguments: json!({}),
            raw_arguments: None,
            result: None,
        }]);

        let id = |session: &Session| {
            session.to_messages()[0]
                .tool_calls
                .as_ref()
                .expect("the call replays")[0]
                .id
                .clone()
        };
        assert_eq!(id(&session), id(&session));
        assert_eq!(id(&session), "call_replay_0");
    }

    #[test]
    fn a_host_message_replays_as_a_user_turn() {
        // The agent loop injects a background-job notice into the request as a
        // user turn; a plain notice is transcript-only. The replay must
        // reproduce the former or every later turn's prefix shifts.
        let session = session_with(vec![
            Step::User {
                text: "run it".into(),
                images: Vec::new(),
            },
            Step::HostMessage {
                text: "job 3 finished".into(),
            },
            Step::Assistant {
                text: "done".into(),
            },
        ]);

        let messages = session.to_messages();
        let roles: Vec<&str> = messages
            .iter()
            .map(|message| message.role.as_str())
            .collect();
        assert_eq!(roles, vec!["user", "user", "assistant"]);
        assert_eq!(messages[1].text(), "job 3 finished");
    }

    #[test]
    fn a_session_with_no_prompt_still_has_a_title() {
        assert_eq!(session_with(Vec::new()).title(), "（空会话）");
    }

    #[test]
    fn the_project_name_is_the_last_path_component() {
        let mut session = session_with(Vec::new());
        session.project = r"C:\Users\me\code\thing".into();
        assert_eq!(session.project_name(), "thing");
    }

    #[test]
    fn a_project_with_no_file_name_falls_back_to_the_whole_path() {
        let mut session = session_with(Vec::new());
        session.project = "/".into();
        assert_eq!(session.project_name(), "/");
    }

    #[test]
    fn consecutive_assistant_text_lands_in_one_bubble() {
        let mut session = session_with(Vec::new());
        session.push_assistant("the file ");
        session.push_assistant("says 42");
        assert_eq!(session.steps.len(), 1);
        match &session.steps[0] {
            Step::Assistant { text } => assert_eq!(text, "the file says 42"),
            other => panic!("expected an assistant step, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_result_attaches_to_its_call() {
        let mut session = session_with(Vec::new());
        session.push_tool(
            "call_1".into(),
            "read_file".into(),
            json!({ "path": "a" }),
            None,
        );
        session.finish_tool(
            "call_1",
            ToolResult {
                outcome: AuditOutcome::Executed,
                output: "42".into(),
                images: Vec::new(),
                hunks: Vec::new(),
                duration_ms: 7,
            },
        );

        match &session.steps[0] {
            Step::Tool { result, .. } => {
                let result = result.as_ref().expect("the result must be attached");
                assert_eq!(result.output, "42");
                assert_eq!(result.outcome, AuditOutcome::Executed);
            }
            other => panic!("expected a tool step, got {other:?}"),
        }
    }

    #[test]
    fn a_result_for_an_unknown_call_is_ignored_rather_than_panicking() {
        let mut session = session_with(Vec::new());
        session.finish_tool(
            "nope",
            ToolResult {
                outcome: AuditOutcome::Failed,
                output: String::new(),
                images: Vec::new(),
                hunks: Vec::new(),
                duration_ms: 0,
            },
        );
        assert!(session.steps.is_empty());
    }

    #[test]
    fn a_session_round_trips_through_json() {
        let mut session = session_with(Vec::new());
        session.push_assistant("hi");
        session.push_tool(
            "c".into(),
            "exec".into(),
            json!({ "lineNumbers": null }),
            None,
        );
        session.finish_tool(
            "c",
            ToolResult {
                outcome: AuditOutcome::Denied,
                output: "Refused: nope".into(),
                images: Vec::new(),
                hunks: Vec::new(),
                duration_ms: 3,
            },
        );

        let text = serde_json::to_string(&session).unwrap();
        let parsed: Session = serde_json::from_str(&text).unwrap();

        assert_eq!(parsed.id, session.id);
        assert_eq!(parsed.steps.len(), 2);
        // The `null` in the arguments is exactly what TOML could not have held.
        match &parsed.steps[1] {
            Step::Tool { arguments, .. } => assert!(arguments["lineNumbers"].is_null()),
            other => panic!("expected a tool step, got {other:?}"),
        }
    }

    #[test]
    fn a_non_utf8_project_does_not_fail_the_save() {
        // `PathBuf` would have failed here, taking every other session with it.
        let mut session = session_with(Vec::new());
        session.project = String::from_utf8_lossy(b"/tmp/\xFF\xFEbad").into_owned();

        let text = encode(&[session]).expect("a lossy path must still serialise");
        let store: Store = serde_json::from_str(&text).expect("it must parse back");
        assert_eq!(store.sessions.len(), 1);
        assert!(store.sessions[0].project.contains('\u{FFFD}'));
    }

    #[test]
    fn the_store_keeps_only_the_newest_sessions() {
        let sessions: Vec<Session> = (0..MAX_SESSIONS + 25)
            .map(|index| {
                let mut session = session_with(Vec::new());
                session.created_at = index as u64;
                session
            })
            .collect();

        let text = encode(&sessions).expect("encoding must succeed");
        let store: Store = serde_json::from_str(&text).expect("it must parse back");

        assert_eq!(store.sessions.len(), MAX_SESSIONS);
        // Oldest-first order is preserved, and the oldest entries are the ones
        // that went.
        assert_eq!(store.sessions.first().unwrap().created_at, 25);
        assert_eq!(
            store.sessions.last().unwrap().created_at,
            MAX_SESSIONS as u64 + 24
        );
    }

    #[test]
    fn trimming_caps_a_long_payload_on_a_character_boundary() {
        let mut session = session_with(vec![Step::Assistant {
            text: "中".repeat(MAX_STORED_TEXT_BYTES),
        }]);
        trim_payloads(&mut session);

        match &session.steps[0] {
            Step::Assistant { text } => {
                assert!(text.len() <= MAX_STORED_TEXT_BYTES + '…'.len_utf8());
                assert!(text.ends_with('…'));
                // The cap must not have split a three-byte character.
                assert!(text.is_char_boundary(text.len() - '…'.len_utf8()));
            }
            other => panic!("expected an assistant step, got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_store_drops_whole_sessions_rather_than_failing() {
        // Each session is padded past the per-payload cap, so the only way to
        // get under the budget is to drop some.
        let big = "x".repeat(MAX_STORED_TEXT_BYTES * 4);
        let sessions: Vec<Session> = (0..40)
            .map(|_| session_with(vec![Step::Assistant { text: big.clone() }]))
            .collect();

        let text = encode(&sessions).expect("encoding must succeed");
        assert!(text.len() <= MAX_STORE_BYTES);
        assert!(text.contains("version"));
    }

    #[test]
    fn age_labels_read_naturally() {
        assert_eq!(age_label(1_000, 1_000), "刚刚");
        assert_eq!(age_label(1_000, 1_059), "刚刚");
        assert_eq!(age_label(1_000, 1_060), "1 分钟前");
        assert_eq!(age_label(0, 7_200), "2 小时前");
        assert_eq!(age_label(0, 172_800), "2 天前");
        assert_eq!(age_label(0, 5_184_000), "2 个月前");
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_underflow() {
        assert_eq!(age_label(2_000, 1_000), "刚刚");
    }

    #[test]
    fn a_run_that_was_still_going_is_settled_on_load() {
        // Otherwise the sidebar pulses a running dot for a run that died with
        // the previous process, and the transcript stops with no explanation.
        let mut session = session_with(vec![Step::User {
            text: "跑一下测试".into(),
            images: Vec::new(),
        }]);
        session.state = RunState::Running;
        assert_eq!(session.state, RunState::Running);

        let settled = settle_interrupted(vec![session]);
        assert_eq!(settled[0].state, RunState::Failed);
        assert!(matches!(
            settled[0].steps.last(),
            Some(Step::Notice { text }) if text.contains("中断")
        ));
    }

    #[test]
    fn a_finished_session_is_left_alone() {
        let mut session = session_with(vec![Step::User {
            text: "你好".into(),
            images: Vec::new(),
        }]);
        session.state = RunState::Finished;
        let before = session.steps.len();

        let settled = settle_interrupted(vec![session]);
        assert_eq!(settled[0].state, RunState::Finished);
        assert_eq!(settled[0].steps.len(), before);
    }

    #[test]
    fn a_compaction_truncates_the_replay_to_its_brief() {
        let mut session = session_with(vec![
            Step::User {
                text: "fix the build".into(),
                images: Vec::new(),
            },
            Step::Compaction {
                summary: "Earlier: the user wants the build fixed.".into(),
            },
            Step::Assistant {
                text: "on it".into(),
            },
        ]);

        // The transcript keeps everything: the marker is what explains where
        // the older turns went.
        let text = session.as_text();
        assert!(
            text.contains("上下文压缩"),
            "the marker must render: {text}"
        );
        assert!(
            text.contains("fix the build"),
            "the transcript is not truncated: {text}"
        );

        // The wire does not. The brief replaces what it summarised, so a
        // follow-up starts from it instead of crossing the threshold again.
        let rendered: Vec<String> = session
            .to_messages()
            .iter()
            .map(|message| message.text())
            .collect();
        assert_eq!(
            rendered,
            vec![
                summary_turn("Earlier: the user wants the build fixed.").text(),
                "on it".to_string(),
            ],
            "the replay must open with the brief and skip what it replaced"
        );

        // The stored marker is capped like any other text field.
        session.steps[1] = Step::Compaction {
            summary: "长".repeat(MAX_STORED_TEXT_BYTES * 2),
        };
        trim_payloads(&mut session);
        match &session.steps[1] {
            Step::Compaction { summary } => {
                assert!(summary.len() <= MAX_STORED_TEXT_BYTES + '…'.len_utf8());
            }
            other => panic!("expected a compaction step, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_compaction_leaves_the_replay_alone() {
        // An empty summary means that run went out with its history intact, so
        // the follow-up has to keep it too — otherwise a failed compaction
        // would silently swallow the conversation.
        let session = session_with(vec![
            Step::User {
                text: "fix the build".into(),
                images: Vec::new(),
            },
            Step::Compaction {
                summary: String::new(),
            },
            Step::Assistant {
                text: "on it".into(),
            },
        ]);

        let rendered: Vec<String> = session
            .to_messages()
            .iter()
            .map(|message| message.text())
            .collect();
        assert_eq!(
            rendered,
            vec!["fix the build".to_string(), "on it".to_string()]
        );
    }

    fn step_user(text: &str) -> Step {
        Step::User {
            text: text.into(),
            images: Vec::new(),
        }
    }

    fn step_assistant(text: &str) -> Step {
        Step::Assistant { text: text.into() }
    }

    /// The replayed wire text, which is what the model actually receives.
    fn rendered(session: &Session) -> Vec<String> {
        session
            .to_messages()
            .iter()
            .map(|message| message.text())
            .collect()
    }

    #[test]
    fn a_range_compaction_keeps_the_tail() {
        let mut session = session_with(vec![
            step_user("u1"),
            step_assistant("a1"),
            step_user("u2"),
            step_assistant("a2"),
            step_user("u3"),
            step_assistant("a3"),
            step_user("prompt"),
        ]);
        session.record_compaction("brief".into(), 3);

        assert_eq!(
            rendered(&session),
            vec![
                summary_turn("brief").text(),
                "u3".to_string(),
                "a3".to_string(),
                "prompt".to_string(),
            ],
            "the brief must replace the folded prefix and keep the tail"
        );
        // The marker sits at the boundary, not at the end.
        assert!(matches!(session.steps[4], Step::Compaction { .. }));
    }

    #[test]
    fn a_second_compaction_cuts_inside_the_current_surface() {
        let mut session = session_with(vec![
            step_user("u1"),
            step_assistant("a1"),
            Step::Compaction {
                summary: "old".into(),
            },
            step_user("u2"),
            step_assistant("a2"),
            step_user("u3"),
            step_assistant("a3"),
            step_user("prompt"),
        ]);
        session.record_compaction("new".into(), 3);

        assert_eq!(
            rendered(&session),
            vec![
                summary_turn("new").text(),
                "u3".to_string(),
                "a3".to_string(),
                "prompt".to_string(),
            ],
            "the newest brief wins and the old one is folded away"
        );
        assert_eq!(
            session
                .steps
                .iter()
                .filter(|step| matches!(step, Step::Compaction { .. }))
                .count(),
            2,
            "the old marker stays in the transcript"
        );
    }

    #[test]
    fn a_second_compaction_over_the_whole_surface_replaces_the_old_brief() {
        // Keeping everything after the old marker means the run folded only
        // that brief; the new marker follows it and becomes the one the replay
        // cuts at, rather than cutting the surface it was meant to keep.
        let mut session = session_with(vec![
            step_user("u1"),
            step_assistant("a1"),
            Step::Compaction {
                summary: "old".into(),
            },
            step_user("u2"),
            step_assistant("a2"),
            step_user("u3"),
            step_assistant("a3"),
            step_user("prompt"),
        ]);
        session.record_compaction("new".into(), 5);

        assert_eq!(
            rendered(&session),
            vec![
                summary_turn("new").text(),
                "u2".to_string(),
                "a2".to_string(),
                "u3".to_string(),
                "a3".to_string(),
                "prompt".to_string(),
            ]
        );
    }

    #[test]
    fn an_empty_summary_is_appended_rather_than_cutting_the_tail() {
        let mut session = session_with(vec![step_user("u1"), step_assistant("a1")]);
        session.record_compaction(String::new(), 1);

        assert_eq!(
            rendered(&session),
            vec!["u1".to_string(), "a1".to_string()],
            "a failed compaction must not drop anything"
        );
        assert!(matches!(
            session.steps.last(),
            Some(Step::Compaction { .. })
        ));
    }

    #[test]
    fn a_zero_tail_folds_the_whole_conversation() {
        let mut session = session_with(vec![step_user("u1"), step_assistant("a1")]);
        session.record_compaction("brief".into(), 0);

        assert_eq!(rendered(&session), vec![summary_turn("brief").text()]);
    }

    #[test]
    fn cache_counters_are_token_weighted_across_requests() {
        let mut session = session_with(Vec::new());

        // Two requests of different sizes: weighting by prompt tokens is what
        // keeps a tiny cold request from dragging the rate of a large warm one.
        session.record_cache_usage(Some(&Usage {
            prompt_tokens: Some(100),
            prompt_cache_hit_tokens: Some(0),
            ..Default::default()
        }));
        session.record_cache_usage(Some(&Usage {
            prompt_tokens: Some(900),
            prompt_cache_hit_tokens: Some(810),
            ..Default::default()
        }));

        assert_eq!(session.cache_hit_tokens, 810);
        assert_eq!(session.cache_measured_tokens, 1_000);
        assert_eq!(session.cache_hit_rate(), Some(0.81));
    }

    #[test]
    fn a_provider_that_never_reports_caching_leaves_the_rate_unset() {
        let mut session = session_with(Vec::new());
        session.record_cache_usage(Some(&Usage {
            prompt_tokens: Some(500),
            completion_tokens: Some(5),
            ..Default::default()
        }));

        assert_eq!(session.cache_measured_tokens, 0);
        assert_eq!(
            session.cache_hit_rate(),
            None,
            "an unreported cache must not read as 0%"
        );
    }

    #[test]
    fn the_openai_cache_shape_is_counted_too() {
        let mut session = session_with(Vec::new());
        session.record_cache_usage(Some(&Usage {
            prompt_tokens: Some(200),
            prompt_tokens_details: Some(crate::llm::PromptTokensDetails {
                cached_tokens: Some(50),
            }),
            ..Default::default()
        }));

        assert_eq!(session.cache_hit_tokens, 50);
        assert_eq!(session.cache_hit_rate(), Some(0.25));
    }
}
