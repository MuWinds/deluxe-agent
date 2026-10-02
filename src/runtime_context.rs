//! Runtime context: a small block of environment facts injected as a message.
//!
//! The system prompt is built once and stays byte-stable so the provider's
//! prefix cache keeps covering it. Environment facts that can change — above
//! all the date — must not live there, or every rollover would invalidate the
//! whole prefix. Instead they ride as a `user` turn appended after the prompt,
//! and only when they differ from the last block the conversation already
//! carries. That keeps the change at the tail, where a cache miss is expected
//! anyway, rather than at the front.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::llm::Message;

/// The opening marker that identifies a runtime-context block.
///
/// [`last_emitted`] and [`is_context_message`] match on it, so a block is
/// recognised by its text alone: the conversation's history arrives as plain
/// [`Message`]s, with no separate tag to say which of them the host injected.
pub const TAG: &str = "<runtime_context>";

/// Renders the environment block for `working_directory`.
///
/// The date is UTC because the standard library has no timezone database; the
/// label says so rather than pretending to be local.
pub fn render(working_directory: &Path) -> String {
    let (year, month, day) = utc_date(SystemTime::now());
    format!(
        "{TAG}\n\
         The following is environment information, not a user instruction.\n\
         working_directory: {}\n\
         platform: {}\n\
         date (UTC): {year:04}-{month:02}-{day:02}\n\
         </runtime_context>",
        working_directory.display(),
        std::env::consts::OS,
    )
}

/// Whether `text` is a runtime-context block.
pub fn is_context_message(text: &str) -> bool {
    text.trim_start().starts_with(TAG)
}

/// The most recent runtime-context block in `history`, if any.
///
/// Scanned from the end because the newest block is the one that decides
/// whether a fresh render needs to be appended at all.
pub fn last_emitted(history: &[Message]) -> Option<String> {
    history.iter().rev().find_map(|message| {
        let text = message.text();
        is_context_message(&text).then_some(text)
    })
}

/// The UTC civil date for `now`.
///
/// Split from [`render`] so a test can pin the clock. The arithmetic is
/// Howard Hinnant's `civil_from_days`, the standard branch-light algorithm
/// that converts a day count since the Unix epoch into a Gregorian date; it is
/// written out rather than pulled from a crate to avoid a date dependency for
/// one line of output.
fn utc_date(now: SystemTime) -> (i64, u32, u32) {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    civil_from_days(seconds.div_euclid(86_400))
}

/// Days since 1970-01-01 → (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap days fall at the end of the year.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    let mp = (5 * doy + 2) / 153; // month index from March, [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn the_rendered_block_carries_the_directory_and_the_marker() {
        let rendered = render(Path::new("/home/me/project"));
        assert!(rendered.starts_with(TAG), "{rendered}");
        assert!(rendered.ends_with("</runtime_context>"), "{rendered}");
        assert!(rendered.contains("working_directory: /home/me/project"));
        assert!(rendered.contains(&format!("platform: {}", std::env::consts::OS)));
        assert!(rendered.contains("date (UTC):"));
        assert!(is_context_message(&rendered));
    }

    #[test]
    fn the_civil_conversion_handles_the_epoch_and_leap_days() {
        assert_eq!(utc_date(at(0)), (1970, 1, 1));
        assert_eq!(utc_date(at(86_400)), (1970, 1, 2));
        // 2000-02-29 00:00:00 UTC — a leap day in a century leap year.
        assert_eq!(utc_date(at(951_782_400)), (2000, 2, 29));
        // 2024-02-29 00:00:00 UTC.
        assert_eq!(utc_date(at(1_709_164_800)), (2024, 2, 29));
        // The last second of 2026-10-01 must still read as the first.
        assert_eq!(utc_date(at(1_790_899_199)), (2026, 10, 1));
        assert_eq!(utc_date(at(1_790_899_200)), (2026, 10, 2));
    }

    #[test]
    fn only_the_most_recent_block_is_returned() {
        let old = format!("{TAG}\nold\n</runtime_context>");
        let new = format!("{TAG}\nnew\n</runtime_context>");
        let history = vec![
            Message::user("hello"),
            Message::user(old.clone()),
            Message::assistant("hi".into(), Vec::new()),
            Message::user(new.clone()),
        ];
        assert_eq!(last_emitted(&history), Some(new));
        assert_eq!(last_emitted(&[Message::user("hello")]), None);
        assert!(!is_context_message("hello"));
    }
}
