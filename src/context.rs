//! Context-window management.
//!
//! The model is configured with a *limit* and a *trigger share* in the
//! settings panel. Once the measured prompt reaches `limit × share`, the whole
//! conversation is handed to the model in a follow-up request and replaced by
//! the continuation brief it writes back. Nothing is kept verbatim — the brief
//! is what the run carries on from.
//!
//! What drives the check is the usage figure each assistant turn carries — the
//! provider's own token count for the prompt it received — never a local
//! estimate, which would disagree with the provider by a wide margin on
//! anything but plain English. The summary costs one extra request; the
//! settings make th    at trade-off the user's call.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::harness::LlmProvider;
use crate::llm::Message;
use crate::runtime_context;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSettings {
    /// The model's context window, in tokens. Zero disables compaction.
    pub context_limit: u64,
    /// The share of the window at which compaction engages, in percent: once
    /// the measured prompt reaches this much of the limit, the oldest turns
    /// are summarised.
    ///
    /// Zero disables compaction as well — every prompt meets a threshold of
    /// zero, so it would otherwise summarise on every single turn. The field
    /// was once named `keepPercent`, meaning the share to keep *after*
    /// compaction; the alias lets a config written back then still load.
    #[serde(alias = "keepPercent")]
    pub threshold_percent: u32,
    /// How many of the most recent user turns compaction keeps verbatim.
    ///
    /// Only the older turns are folded into the brief, so the model keeps the
    /// recent exchange word-for-word instead of reading a paraphrase of it.
    /// Zero folds everything, the behaviour compaction had before this field
    /// existed; a value larger than the conversation simply leaves it alone.
    ///
    /// `serde(default)` so a config written before the field existed loads as
    /// the default rather than as zero, which would silently drop the tail.
    #[serde(default = "default_keep_recent_turns")]
    pub keep_recent_turns: u32,
}

/// The default tail compaction keeps: the last two user turns.
fn default_keep_recent_turns() -> u32 {
    2
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            context_limit: 0,
            threshold_percent: 60,
            keep_recent_turns: default_keep_recent_turns(),
        }
    }
}

impl ContextSettings {
    /// Whether compaction is configured at all. A zero limit or a zero
    /// threshold turns the whole mechanism off rather than compacting always.
    fn is_active(&self) -> bool {
        self.context_limit > 0 && self.threshold_percent > 0
    }

    /// The prompt size, in tokens, at which compaction engages.
    fn threshold(&self) -> u64 {
        (self.context_limit as f32 * self.threshold_ratio()) as u64
    }

    /// The trigger as a share of the window.
    ///
    /// The composer's context gauge colours its ring by the same line the
    /// compaction check fires on, so the indicator and the mechanism cannot
    /// drift apart the way a second, hand-copied constant would. A hand-edited
    /// config could still name more than 100%, so the share is bounded at 1.0.
    pub fn threshold_ratio(&self) -> f32 {
        self.threshold_percent.min(100) as f32 / 100.0
    }
}

/// The compaction state of one conversation.
///
/// Exactly one measurement is kept: the provider-reported prompt tokens for
/// the most recent request, together with how many messages it carried. The
/// next request is priced from it by *linear extrapolation in message count*:
/// the counts almost never match — every turn appends at least an assistant
/// message — so a check that demanded an exact shape would simply never fire.
///
/// The measurement travels with the conversation, not with the process: a
/// follow-up run seeds a fresh window with it, so the very first request of a
/// long session is already guarded.
#[derive(Debug, Default)]
pub struct ContextWindow {
    settings: ContextSettings,
    latest: Option<(u64, usize)>,
}

impl ContextWindow {
    /// A window with no measurement yet; the first request of a run seeds it.
    pub fn new(settings: ContextSettings) -> Self {
        Self {
            settings,
            latest: None,
        }
    }

    /// The settings this window was built with.
    pub fn settings(&self) -> ContextSettings {
        self.settings
    }

    /// Restores a measurement carried over from an earlier run of the same
    /// conversation. `None` leaves the window blank, which is the state after
    /// a restart or a provider that never reports usage.
    pub fn restore(&mut self, latest: Option<(u64, usize)>) {
        self.latest = latest;
    }

    /// The measurement to carry to the next run of this conversation.
    pub fn measurement(&self) -> Option<(u64, usize)> {
        self.latest
    }

    /// Records what the provider said the last request cost.
    pub fn record_usage(&mut self, prompt_tokens: u64, message_count: usize) {
        if self.settings.is_active() && prompt_tokens > 0 {
            self.latest = Some((prompt_tokens, message_count));
        }
    }

    /// The provider-reported prompt tokens for the last request, if any.
    pub fn latest_usage(&self) -> Option<u64> {
        self.latest.map(|(tokens, _)| tokens)
    }

    /// The measured prompt size, projected onto a request of `message_count`
    /// messages.
    ///
    /// Rounded up: an overestimate by at most one message's worth errs toward
    /// compacting early, which is the safe side.
    fn projected(&self, message_count: usize) -> Option<u64> {
        let (tokens, measured) = self.latest?;
        if measured == 0 {
            return None;
        }
        let scaled = tokens.saturating_mul(message_count.max(1) as u64);
        Some(scaled.div_ceil(measured as u64))
    }

    /// Whether a request carrying `messages` would cross the compaction
    /// threshold.
    pub fn should_compact(&self, messages: &[Message]) -> bool {
        if !self.settings.is_active() {
            return false;
        }
        self.projected(messages.len())
            .map(|tokens| tokens >= self.settings.threshold())
            .unwrap_or(false)
    }
}

/// The instruction appended to the conversation when it is summarised.
///
/// It rides at the end of the real conversation rather than in a prompt of its
/// own, so the request's prefix is byte-identical to the turn that triggered
/// compaction and the provider serves the whole history from its prompt cache.
const SUMMARIZE_INSTRUCTION: &str = "\
The conversation above is getting too long to carry in full. Do not continue it \
and do not call any tools. Reply with only a continuation brief: the user's \
goals, the decisions taken, the files touched, and anything still unfinished. \
Two hundred words at most. Answer in the language the conversation used.";

/// Summarises the whole conversation into one continuation brief.
///
/// Returns `None` when there is nothing worth sending: a conversation with no
/// text but its system prompt, or a brief that came back empty.
///
/// The request reuses the conversation's own messages — the same system prompt,
/// the same tool schema, the same prefix — and appends [`SUMMARIZE_INSTRUCTION`],
/// so the provider's prompt cache covers the history instead of re-reading a
/// re-rendered transcript at full price. The summary comes back as plain text
/// rather than a ready-made message: the caller decides how it re-enters the
/// wire — see [`summary_turn`].
pub async fn summarize(
    history: &[Message],
    tools: &Value,
    llm: &dyn LlmProvider,
    cancel: &CancellationToken,
) -> crate::error::Result<Option<String>> {
    // The system prompt is not conversation; a history of nothing but it has
    // nothing to summarise.
    let has_text = history
        .iter()
        .any(|message| message.role != "system" && !message.text().trim().is_empty());
    if !has_text {
        return Ok(None);
    }

    let mut messages = history.to_vec();
    messages.push(Message::user(SUMMARIZE_INSTRUCTION));

    let turn = llm.complete_turn(&messages, tools, cancel).await?;

    let summary = turn.content.trim().to_string();
    if summary.is_empty() {
        return Ok(None);
    }
    Ok(Some(summary))
}

/// The turn a compaction leaves behind.
///
/// Built here rather than at either call site because the two must produce
/// byte-identical text: the run that compacts, and a later run replaying the
/// same session from its stored summary.
///
/// It is a `user` turn rather than a note folded into the system prompt, for
/// two reasons. A request of nothing but a system message has no turn for the
/// model to answer, and several providers reject that shape outright. And
/// [`render`] skips system messages, so a brief parked there would be
/// invisible to the compaction after this one — it would be re-derived from
/// nothing every time.
pub fn summary_turn(summary: &str) -> Message {
    Message::user(format!(
        "Here is a summary of our conversation so far:\n\n{summary}\n\n\
         Continue from where it left off."
    ))
}

/// Where a compaction should cut `messages`: the index of the first message
/// that survives verbatim, or `None` when there is nothing worth folding.
///
/// The cut lands on a `user` turn, so a tool call is never separated from its
/// result — a `tool` message answering nothing is rejected by the provider.
/// Runtime-context blocks are skipped when counting turns: they are
/// environment notes the host appended, not something the user asked, so they
/// must not push a real turn out of the kept tail.
///
/// `keep_recent_turns` counts *user* messages, the current prompt included, so
/// the default of two keeps the previous exchange plus the prompt being sent.
pub fn compaction_split(messages: &[Message], keep_recent_turns: u32) -> Option<usize> {
    let keep = keep_recent_turns as usize;
    if keep == 0 {
        // No tail: fold everything after the system prompt, as compaction did
        // before the tail existed.
        return (messages.len() > 1).then_some(messages.len());
    }

    let users: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == "user" && !runtime_context::is_context_message(&message.text())
        })
        .map(|(index, _)| index)
        .collect();

    if users.len() <= keep {
        return None;
    }
    let split = users[users.len() - keep];
    // A cut at or before the first real turn leaves only the system prompt to
    // summarise, which is not worth a request.
    (split > 1).then_some(split)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    fn assistant(text: &str) -> Message {
        Message::assistant(text.to_string(), Vec::new())
    }

    fn tool() -> Message {
        Message::tool("call_1", "the file says 42")
    }

    fn window(limit: u64, threshold_percent: u32) -> ContextWindow {
        ContextWindow::new(ContextSettings {
            context_limit: limit,
            threshold_percent,
            ..ContextSettings::default()
        })
    }

    #[test]
    fn a_zero_limit_disables_everything() {
        let mut window = window(0, 60);
        window.record_usage(999_999, 10);
        assert!(!window.should_compact(&[user("a"), assistant("b")]));
        assert_eq!(window.latest_usage(), None);
    }

    #[test]
    fn a_zero_threshold_disables_everything() {
        // Every prompt meets a threshold of zero, so it would compact on every
        // single turn; zero is read as "off" instead.
        let mut window = window(100, 0);
        window.record_usage(999_999, 10);
        assert!(!window.should_compact(&[user("a"), assistant("b")]));
        assert_eq!(window.latest_usage(), None);
    }

    #[test]
    fn the_trigger_is_the_configured_share_of_the_limit() {
        // 60% of 100 is 60 tokens: a 59-token prompt is left alone, a 60-token
        // one is not.
        let mut at_sixty = window(100, 60);
        at_sixty.record_usage(59, 1);
        assert!(!at_sixty.should_compact(&[user("a")]));
        at_sixty.record_usage(60, 1);
        assert!(at_sixty.should_compact(&[user("a")]));

        // Raising the share raises the bar with it.
        let mut at_ninety = window(100, 90);
        at_ninety.record_usage(80, 1);
        assert!(!at_ninety.should_compact(&[user("a")]));
    }

    #[test]
    fn a_small_history_is_left_alone() {
        // 8 of 100 measured against a threshold of 60: plenty of room.
        let mut window = window(100, 60);
        window.record_usage(8, 2);
        assert!(!window.should_compact(&[user("a"), assistant("b")]));
    }

    #[test]
    fn the_projection_extrapolates_across_shapes() {
        // The same n-message measurement, projected onto 2n messages: double.
        let mut window = window(100, 60);
        window.record_usage(50, 5);
        let messages = vec![user("a"); 10];
        assert!(window.should_compact(&messages), "100 >= 60");
        // …and onto n/2: half.
        let short = vec![user("a"); 2];
        assert!(!window.should_compact(&short), "20 < 60");
    }

    #[test]
    fn a_zero_measurement_is_ignored() {
        let mut window = window(100, 60);
        window.record_usage(0, 4);
        assert!(!window.should_compact(&[user("a"), assistant("b")]));
    }

    #[test]
    fn a_measurement_survives_a_round_trip() {
        // The window is rebuilt for every run; the measurement travels with
        // the conversation instead.
        let mut first = window(100, 60);
        first.record_usage(90, 9);
        let carried = first.measurement();

        let mut next = window(100, 60);
        assert!(!next.should_compact(&[user("a"), assistant("b")]));
        next.restore(carried);
        let messages = vec![user("a"); 9];
        assert!(next.should_compact(&messages));
    }

    #[test]
    fn a_settings_round_trip_preserves_the_fields() {
        let settings = ContextSettings {
            context_limit: 131_072,
            threshold_percent: 45,
            ..ContextSettings::default()
        };
        let text = toml::to_string(&settings).unwrap();
        let parsed: ContextSettings = toml::from_str(&text).unwrap();
        assert_eq!(parsed, settings);
    }

    #[test]
    fn a_config_naming_the_old_field_still_loads_it() {
        // `keepPercent` used to mean the share kept *after* compaction. The
        // number is read as the trigger share now, but an old file must not
        // silently fall back to the default.
        let parsed: ContextSettings =
            toml::from_str("contextLimit = 1000\nkeepPercent = 45\n").unwrap();
        assert_eq!(parsed.threshold_percent, 45);
    }

    #[test]
    fn the_displayed_share_is_the_one_compaction_uses() {
        // The gauge reads this rather than a private constant; if the share
        // ever moves, the ring must move with it.
        let settings = ContextSettings {
            context_limit: 100,
            threshold_percent: 45,
            ..ContextSettings::default()
        };
        assert_eq!(settings.threshold_ratio(), 0.45);
        assert_eq!(settings.threshold(), 45);
    }

    #[test]
    fn the_split_keeps_the_configured_number_of_user_turns() {
        let messages = vec![
            Message::system("sys"),
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2"),
            user("u3"),
            assistant("a3"),
            user("u4"),
        ];
        // Users sit at 1, 3, 5 and 7. Keeping two cuts at 5, so u3, a3 and the
        // current prompt survive.
        assert_eq!(compaction_split(&messages, 2), Some(5));
        // Keeping one leaves only the prompt being sent.
        assert_eq!(compaction_split(&messages, 1), Some(7));
        // Keeping four would cut at u1, leaving only the system prompt.
        assert_eq!(compaction_split(&messages, 4), None);
        assert_eq!(compaction_split(&messages, 9), None);
    }

    #[test]
    fn a_zero_tail_folds_everything_after_the_system_prompt() {
        let messages = vec![Message::system("sys"), user("u1"), assistant("a1")];
        assert_eq!(compaction_split(&messages, 0), Some(3));
        // Nothing but the system prompt is nothing to fold.
        assert_eq!(compaction_split(&[Message::system("sys")], 0), None);
    }

    #[test]
    fn runtime_context_blocks_do_not_count_as_user_turns() {
        let context = crate::runtime_context::render(std::path::Path::new("/tmp/proj"));
        let messages = vec![
            Message::system("sys"),
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2"),
            Message::user(context),
            user("u3"),
        ];
        // The real users sit at 1, 3 and 6; the block at 5 is skipped, so
        // keeping two still cuts at the second real turn.
        assert_eq!(compaction_split(&messages, 2), Some(3));
    }

    /// An `LlmProvider` that records what the summariser sent and returns a
    /// fixed brief.
    struct RecordingLlm {
        seen: std::sync::Mutex<Vec<Message>>,
        tools_seen: std::sync::Mutex<Option<Value>>,
        reply: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for RecordingLlm {
        async fn stream_turn(
            &self,
            _messages: &[Message],
            _tools: &Value,
            _thinking: Option<crate::llm::ThinkingLevel>,
            _cancel: &CancellationToken,
            _sink: &mut dyn crate::harness::LlmStreamSink,
        ) -> crate::error::Result<crate::llm::AssistantTurn> {
            unreachable!("the summariser never streams a turn")
        }

        async fn complete_turn(
            &self,
            messages: &[Message],
            tools: &Value,
            _cancel: &CancellationToken,
        ) -> crate::error::Result<crate::llm::AssistantTurn> {
            *self.seen.lock().unwrap() = messages.to_vec();
            *self.tools_seen.lock().unwrap() = Some(tools.clone());
            Ok(crate::llm::AssistantTurn {
                content: self.reply.clone(),
                tool_calls: Vec::new(),
                usage: None,
                finish_reason: None,
            })
        }
    }

    fn recording_llm(reply: &str) -> RecordingLlm {
        RecordingLlm {
            seen: std::sync::Mutex::new(Vec::new()),
            tools_seen: std::sync::Mutex::new(None),
            reply: reply.into(),
        }
    }

    #[tokio::test]
    async fn the_summary_request_reuses_the_conversation_and_appends_the_instruction() {
        // The point of the whole exercise: the request carries the conversation
        // verbatim — system prompt included — so the provider serves it from
        // the prefix cache, with the instruction as the only new turn.
        let llm = recording_llm("brief");
        let tools = json!([{ "type": "function" }]);
        let history = vec![
            Message::system("you are a coding agent"),
            user("list the files"),
            assistant("looking…"),
            tool(),
        ];

        let summary = summarize(&history, &tools, &llm, &CancellationToken::new())
            .await
            .expect("the summary request succeeds")
            .expect("a brief comes back");
        assert_eq!(summary, "brief");

        let sent = llm.seen.lock().unwrap();
        // Compare the serialised bytes, which is what the cache keys on.
        assert_eq!(
            serde_json::to_string(&sent[..history.len()]).unwrap(),
            serde_json::to_string(&history).unwrap(),
            "the conversation prefix must be byte-identical"
        );
        assert_eq!(sent.len(), history.len() + 1);
        assert_eq!(
            sent.last().unwrap().text(),
            SUMMARIZE_INSTRUCTION,
            "the instruction is the one new turn"
        );
        assert_eq!(
            llm.tools_seen.lock().unwrap().as_ref(),
            Some(&tools),
            "the tool schema travels with the request to keep the cached prefix"
        );
    }

    #[tokio::test]
    async fn a_history_of_nothing_but_the_system_prompt_is_not_summarised() {
        let llm = recording_llm("brief");
        let history = vec![Message::system("you are a coding agent")];

        let summary = summarize(&history, &json!([]), &llm, &CancellationToken::new())
            .await
            .expect("an empty conversation is not an error");

        assert_eq!(summary, None);
        assert!(
            llm.seen.lock().unwrap().is_empty(),
            "no request is made when there is nothing to summarise"
        );
    }
}
