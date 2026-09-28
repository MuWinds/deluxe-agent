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
//! settings make that trade-off the user's call.

use serde::{Deserialize, Serialize};

use crate::llm::Message;

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
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            context_limit: 0,
            threshold_percent: 60,
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

/// Summarises the whole conversation into one continuation brief.
///
/// Returns `None` when there is nothing worth sending: a transcript with no
/// text in it at all, or a brief that came back empty.
///
/// The summary comes back as plain text rather than as a ready-made message:
/// the caller decides how it re-enters the wire — see [`summary_turn`].
pub async fn summarize(
    history: &[Message],
    llm: &crate::llm::LlmClient,
) -> crate::error::Result<Option<String>> {
    let transcript = render(history);
    if transcript.is_empty() {
        return Ok(None);
    }

    let turn = llm
        .complete_turn(&[
            Message::system(
                "You compress a conversation between a user and a coding agent into a short \
                 continuation brief. Keep the user's goals, the decisions taken, the files \
                 touched, and anything still unfinished. Two hundred words at most. Answer in \
                 the language the conversation used.",
            ),
            Message::user(transcript),
        ])
        .await?;

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

/// Renders a conversation as a readable transcript for the summariser.
///
/// Only text is rendered. A message that carries an image contributes its
/// envelope and nothing else — inlining the picture's base64 into a prompt
/// meant to be a short brief would be megabytes of noise. The system prompt is
/// skipped too: it is the agent's standing instructions, not conversation, and
/// re-summarising it every time would only crowd out what matters.
fn render(messages: &[Message]) -> String {
    let mut text = String::new();
    for message in messages {
        let label = match message.role.as_str() {
            "user" => "User",
            "assistant" => "Agent",
            "tool" => "Tool result",
            _ => continue,
        };
        let content = message.text();
        if content.trim().is_empty() {
            continue;
        }
        text.push_str(&format!("{label}: {content}\n\n"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        assert_eq!(settings.threshold_ratio(), 0.45);
        assert_eq!(settings.threshold(), 45);
    }

    #[test]
    fn the_transcript_labels_every_turn_and_skips_the_system_prompt() {
        // The summariser sees the conversation, never the agent's standing
        // instructions: re-summarising those every time would crowd out what
        // the brief is actually for.
        let messages = vec![
            Message::system("you are a coding agent"),
            user("list the files"),
            assistant("looking…"),
            tool(),
        ];

        let transcript = render(&messages);

        assert!(transcript.contains("User: list the files"), "{transcript}");
        assert!(transcript.contains("Agent: looking…"), "{transcript}");
        assert!(
            transcript.contains("Tool result: the file says 42"),
            "{transcript}"
        );
        assert!(
            !transcript.contains("you are a coding agent"),
            "the system prompt is not conversation: {transcript}"
        );
    }
}
