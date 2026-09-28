//! The optional `---` block at the top of a plugin's Markdown files.
//!
//! Two kinds of file in the format open with one: a skill's `SKILL.md`, whose
//! block carries the `name` and `description` the prompt lists, and a command's
//! `commands/*.md`, whose block may carry `name`, `description` and
//! `argument-hint`. Both need the same narrow slice of it, and neither needs a
//! YAML parser to get it, so the split lives here rather than twice over.
//!
//! # What is *not* hand-waved
//!
//! The block is small but the edges are not, because plugins are written by
//! other people on other machines: a file authored on Windows arrives with CRLF
//! endings, a description routinely contains a colon, values are quoted about
//! half the time, and a block whose closing `---` is missing has to be told
//! apart from one that is simply absent. Each of those has a test.

use std::collections::BTreeMap;

/// A leading `---` block and the body that followed it.
pub struct Frontmatter<'a> {
    fields: BTreeMap<String, String>,
    body: &'a str,
}

impl<'a> Frontmatter<'a> {
    /// One field's value, with a layer of quotes removed.
    ///
    /// `None` when the block is absent or the field is not in it. Keys this
    /// agent does not read are kept rather than filtered, so a caller that
    /// wants one later does not have to come back and change the parser.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    /// Everything after the closing delimiter.
    ///
    /// The whole text when there was no usable block: without a closing `---`
    /// there is no way to tell the fields from the instructions, and guessing
    /// would silently swallow the body — which for a command is the entire
    /// prompt.
    pub fn body(&self) -> &'a str {
        self.body
    }
}

/// Splits `text` into its leading `---` block and the body after it.
///
/// Never fails. A file with no block, or with an opening `---` that is never
/// closed, yields no fields and the whole text as its body.
pub fn split(text: &str) -> Frontmatter<'_> {
    match block(text) {
        Some((fields, body)) => Frontmatter { fields, body },
        None => Frontmatter {
            fields: BTreeMap::new(),
            body: text,
        },
    }
}

/// The parsed block and the body, or `None` when there is no usable block.
fn block(text: &str) -> Option<(BTreeMap<String, String>, &str)> {
    let mut rest = text;
    // The byte offset of `rest` within `text`, which is what lets the body come
    // back as a slice of the original rather than a copy.
    let mut offset = 0;

    // Leading blank lines are tolerated; anything else before the first `---`
    // means there is no frontmatter.
    loop {
        let (line, after) = next_line(rest)?;
        if line.trim().is_empty() {
            offset += rest.len() - after.len();
            rest = after;
            continue;
        }
        if line.trim() != "---" {
            return None;
        }
        offset += rest.len() - after.len();
        rest = after;
        break;
    }

    let mut fields = BTreeMap::new();
    loop {
        let (line, after) = next_line(rest)?;
        // Everything up to and including this line's newline. Adding it to the
        // offset lands exactly on the first byte of the body.
        let consumed = rest.len() - after.len();

        if line.trim() == "---" {
            return Some((fields, &text[offset + consumed..]));
        }

        // Split on the *first* colon: a description routinely contains one
        // ("Control Windows apps: from Codex"), a key never does.
        if let Some((key, value)) = line.split_once(':') {
            let value = unquote(value.trim());
            if !value.is_empty() {
                // A key repeated in one block keeps its last value, which is
                // what a YAML parser would do with a duplicate key.
                fields.insert(key.trim().to_string(), value);
            }
        }

        offset += consumed;
        rest = after;
    }
}

/// The next line and everything after it, or `None` at the end of the text.
///
/// Splitting on `\n` and trimming the caller's line is what makes a
/// Windows-authored file parse identically to a Unix one: the `\r` lands on the
/// end of the line and `trim` takes it off. A test covers that rather than a
/// hand-rolled strip, so a later switch that leaves `\r` in every value fails
/// loudly.
fn next_line(text: &str) -> Option<(&str, &str)> {
    if text.is_empty() {
        return None;
    }
    match text.find('\n') {
        Some(index) => Some((&text[..index], &text[index + 1..])),
        None => Some((text, "")),
    }
}

/// Removes one layer of matching quotes, if the value is quoted.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block shaped exactly like the real `computer-use` one, CRLF and all —
    /// the installs on this machine really do use Windows endings.
    const COMPUTER_USE: &str = "---\r\nname: computer-use\r\ndescription: Control Windows apps from Codex\r\n---\r\n\r\n# Computer Use\r\n\r\nUse this skill to automate the UI of Microsoft Windows apps.\r\n";

    #[test]
    fn a_real_block_parses_across_crlf() {
        let front = split(COMPUTER_USE);
        assert_eq!(front.get("name"), Some("computer-use"));
        assert_eq!(
            front.get("description"),
            Some("Control Windows apps from Codex"),
            "the trailing carriage return must not survive into the value"
        );
    }

    #[test]
    fn an_lf_file_parses_the_same_way() {
        let front = split("---\nname: a\ndescription: b\n---\nbody");
        assert_eq!(front.get("name"), Some("a"));
        assert_eq!(front.get("description"), Some("b"));
    }

    #[test]
    fn the_body_is_everything_after_the_closing_delimiter() {
        let front = split("---\nname: a\n---\n\n# Heading\n\nProse.");
        assert_eq!(front.body(), "\n# Heading\n\nProse.");
    }

    #[test]
    fn the_body_keeps_its_own_delimiters() {
        // A command body may legitimately contain a horizontal rule, so only
        // the first closing `---` ends the block.
        let front = split("---\nname: a\n---\nbefore\n---\nafter\n");
        assert_eq!(front.body(), "before\n---\nafter\n");
    }

    #[test]
    fn a_description_may_contain_a_colon() {
        let front = split("---\nname: a\ndescription: Use when: the build fails\n---\n");
        assert_eq!(front.get("description"), Some("Use when: the build fails"));
    }

    #[test]
    fn quoted_values_lose_their_quotes() {
        let front = split("---\nname: \"a\"\ndescription: 'b: c'\n---\n");
        assert_eq!(front.get("name"), Some("a"));
        assert_eq!(front.get("description"), Some("b: c"));
    }

    #[test]
    fn keys_this_agent_does_not_read_are_still_parsed() {
        // Kept rather than filtered, so a caller that wants one later does not
        // have to reopen the parser.
        let front = split("---\nname: a\nallowed-tools: [Read, Bash]\nargument-hint: [x]\n---\n");
        assert_eq!(front.get("name"), Some("a"));
        assert_eq!(front.get("allowed-tools"), Some("[Read, Bash]"));
        assert_eq!(front.get("argument-hint"), Some("[x]"));
        assert_eq!(front.get("absent"), None);
    }

    #[test]
    fn a_colon_inside_a_value_does_not_split_a_second_time() {
        // `name: boss:plan` is a real spelling in the wild; the key is `name`
        // and the value is `boss:plan`.
        let front = split("---\nname: boss:plan\n---\n");
        assert_eq!(front.get("name"), Some("boss:plan"));
    }

    #[test]
    fn no_block_at_all_leaves_the_whole_text_as_the_body() {
        let front = split("# Title\n\nInstructions.");
        assert_eq!(front.get("name"), None);
        assert_eq!(front.body(), "# Title\n\nInstructions.");
    }

    #[test]
    fn an_unterminated_block_is_treated_as_absent() {
        // Without the closing `---` there is no way to tell fields from
        // instructions; reading them anyway would swallow the whole body.
        let text = "---\nname: a\ndescription: b\n";
        let front = split(text);
        assert_eq!(front.get("name"), None);
        assert_eq!(front.body(), text, "nothing is consumed");
    }

    #[test]
    fn blank_lines_before_the_opening_delimiter_are_tolerated() {
        let front = split("\n\n---\nname: a\n---\n");
        assert_eq!(front.get("name"), Some("a"));
    }

    #[test]
    fn a_trailing_space_on_the_delimiter_still_closes_the_block() {
        let front = split("---\nname: a\n---   \nbody");
        assert_eq!(front.get("name"), Some("a"));
        assert_eq!(front.body(), "body");
    }

    #[test]
    fn an_empty_value_is_not_a_value() {
        let front = split("---\nname: a\ndescription:\n---\n");
        assert_eq!(front.get("description"), None);
    }

    #[test]
    fn a_repeated_key_keeps_the_last_value() {
        let front = split("---\nname: first\nname: second\n---\n");
        assert_eq!(front.get("name"), Some("second"));
    }

    #[test]
    fn an_empty_file_has_no_fields_and_no_body() {
        let front = split("");
        assert_eq!(front.get("name"), None);
        assert_eq!(front.body(), "");
    }
}
