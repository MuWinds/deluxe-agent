//! How a message body is drawn.
//!
//! The model answers in Markdown, and this used to hand the raw string to one
//! `egui::Label` — so `**核心逻辑**` reached the screen as asterisks. This module
//! is the parse-and-draw pair that fixes that.
//!
//! Hand-rolled rather than a crate, because the subset a chat transcript
//! actually uses is small, and because the alternative is a CommonMark parser
//! plus an intermediate tree this codebase would immediately throw away. The
//! two halves are split the way `code_view` splits `patch_lines` from
//! `body_job`: [`parse`] is pure — no `Ui`, no galleys — so it is unit-tested
//! directly, and [`draw`] only turns the result into widgets.
//!
//! Deliberate divergences from CommonMark, so nobody has to guess whether a
//! difference is a bug:
//!
//! * no setext headings, indented code blocks, HTML, or images;
//! * no reference links, and no autolinking of bare URLs — only `[text](url)`
//!   and `<url>` are links;
//! * a hard break is a trailing backslash only. Two trailing spaces are
//!   invisible, and a model leaks them constantly;
//! * list depth comes from a fixed two-space indent rather than the parent
//!   item's content indent, which is what a model actually emits;
//! * an emphasis run is matched against the next run of the same delimiter
//!   *at least* as long, not by CommonMark's delimiter stack. `**a**b*` and
//!   `a***b***` therefore differ from the reference;
//! * a code span keeps its interior spaces (` ` a ` ` renders as ` a `);
//! * a table is recognised only when the delimiter row is the very next line.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use eframe::egui;
use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, FontId, Frame, Layout, Margin, RichText, Stroke, Vec2};

use crate::code_view;
use crate::theme::{self, Palette};

/// The size a bubble's body text is drawn at, which is what `draw_bubble` used
/// before this module existed.
const BODY_SIZE: f32 = theme::font(14.0);
/// A table cell, one step down from the body so a table reads as an inset.
const TABLE_SIZE: f32 = theme::font(13.0);
/// Between two table columns, and between the cells of a row.
const TABLE_SPACING: f32 = 12.0;
/// The vertical padding a grid rule is inset by.
const TABLE_V_PAD: f32 = 4.0;
/// The widest a table column is measured at, so a very long cell does not
/// starve the proportional shrink of budget to give the others.
const TABLE_CELL_CAP: f32 = 320.0;
/// The narrowest a column may be before the table stops being a grid.
const TABLE_MIN_COLUMN: f32 = 24.0;
/// A token at least this long marks the cell holding it as path-shaped.
const TABLE_LONG_TOKEN: usize = 20;
/// Average words per body cell at which a column reads as prose.
const TABLE_PROSE_WORDS: f32 = 4.0;
/// The most times `fit_widths` takes a step, as a bound on an impossible fit.
const TABLE_SHRINK_STEPS: usize = 512;
/// How far a nested list item is pushed right of its parent.
const LIST_INDENT: f32 = 16.0;
/// The width of a list item's marker column. Fixed, so that `9.` and `10.`
/// share a right edge and a wrapped item's second line starts under its text.
const LIST_MARKER: f32 = 22.0;
/// The gutter a quote's bar sits in.
const QUOTE_INDENT: i8 = 12;

/// How one run of text is decorated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
}

/// One inline run: its text, how it is decorated, and where it links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
    /// `Some(url)` on a link's label. It lives here rather than in a
    /// `TextFormat` because `TextFormat` has no URL field — which is why a
    /// paragraph containing a link cannot be drawn as one galley. See
    /// `draw_spans`.
    pub link: Option<String>,
}

impl Span {
    /// A span with default styling and no link.
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: SpanStyle::default(),
            link: None,
        }
    }
}

/// A list item's leading token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Bullet,
    /// Carries the number the source wrote, so `3.` does not silently become
    /// `1.` — a model sometimes resumes a list on purpose.
    Ordered(u64),
}

/// A table column's alignment, from its delimiter row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// One block of a message body.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph {
        spans: Vec<Span>,
    },
    /// Flat, with a depth, rather than a nested `List`: the parser is
    /// line-based, and a hanging indent is a draw-time concern rather than a
    /// tree-shape one.
    Item {
        depth: usize,
        marker: Marker,
        spans: Vec<Span>,
    },
    /// A fence that was never closed keeps everything to the end of the text.
    /// Mid-stream that is the normal state, not an error.
    Code {
        lang: String,
        text: String,
    },
    Quote {
        blocks: Vec<Block>,
    },
    Rule,
    Table {
        align: Vec<ColumnAlign>,
        header: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
        /// The table as it was written, kept so that a bubble too narrow for a
        /// grid can redraw the source instead of breaking it.
        text: String,
    },
}

/// Parses a message body into blocks.
///
/// Pure: no `egui`, no `Ui`, no galleys. This is the half that is tested.
pub fn parse(text: &str) -> Vec<Block> {
    parse_blocks(&text.lines().collect::<Vec<_>>())
}

fn parse_blocks(lines: &[&str]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        // The order is load-bearing. A fence and a rule both start with a
        // character a list item could start with, and the paragraph arm has to
        // come last so that it cannot swallow a table's header row.
        if let Some((lang, text, next)) = fence(lines, i) {
            blocks.push(Block::Code { lang, text });
            i = next;
            continue;
        }

        if is_rule(line) {
            blocks.push(Block::Rule);
            i += 1;
            continue;
        }

        if let Some((level, text)) = heading(line) {
            blocks.push(Block::Heading {
                level,
                spans: parse_inline(text),
            });
            i += 1;
            continue;
        }

        if quote_line(line).is_some() {
            let (quoted, next) = gather_quote(lines, i);
            let quoted: Vec<&str> = quoted.iter().map(String::as_str).collect();
            blocks.push(Block::Quote {
                blocks: parse_blocks(&quoted),
            });
            i = next;
            continue;
        }

        if let Some(align) = table_delimiter(lines, i) {
            let (table, next) = read_table(lines, i, align);
            blocks.push(table);
            i = next;
            continue;
        }

        if let Some((depth, marker, rest)) = list_item(line) {
            let (spans, next) = item_spans(lines, i, rest);
            blocks.push(Block::Item {
                depth,
                marker,
                spans,
            });
            i = next;
            continue;
        }

        let (spans, next) = paragraph(lines, i);
        blocks.push(Block::Paragraph { spans });
        i = next;
    }

    blocks
}

/// Whether this line opens a block, and so ends a paragraph or a list item.
fn starts_block(lines: &[&str], i: usize) -> bool {
    let line = lines[i];
    fence(lines, i).is_some()
        || is_rule(line)
        || heading(line).is_some()
        || quote_line(line).is_some()
        || list_item(line).is_some()
        || table_delimiter(lines, i).is_some()
}

/// A fence opening at `start`, as `(info string, body, index after the close)`.
///
/// An unterminated fence returns everything that is left as the body: while a
/// message is streaming, that is the only possible reading, and it keeps a
/// half-arrived ```` ``` ```` from flashing as a paragraph of backticks.
fn fence(lines: &[&str], start: usize) -> Option<(String, String, usize)> {
    let trimmed = lines[start].trim_start();
    let (ch, run) = fence_marker(trimmed)?;
    let lang = trimmed[run..].trim().to_string();

    let mut body = String::new();
    let mut i = start + 1;
    while i < lines.len() {
        if closes_fence(lines[i], ch, run) {
            i += 1;
            break;
        }
        body.push_str(lines[i]);
        body.push('\n');
        i += 1;
    }

    Some((lang, body, i))
}

/// The fence a line opens, as `(character, run length)`.
fn fence_marker(trimmed: &str) -> Option<(u8, usize)> {
    let first = *trimmed.as_bytes().first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let run = trimmed.bytes().take_while(|byte| *byte == first).count();
    (run >= 3).then_some((first, run))
}

/// Whether a line is nothing but a long enough run of the fence's own
/// character. A shorter run inside the body is content, not a close.
fn closes_fence(line: &str, ch: u8, run: usize) -> bool {
    let trimmed = line.trim();
    let closing = trimmed.bytes().take_while(|byte| *byte == ch).count();
    closing >= run && closing == trimmed.len()
}

/// `---`, `***` and `___` are rules; `- item` is a bullet.
fn is_rule(line: &str) -> bool {
    let mut marks = 0;
    for ch in line.trim().chars() {
        match ch {
            '-' | '*' | '_' => marks += 1,
            ' ' | '\t' => {}
            _ => return false,
        }
    }
    marks >= 3
}

/// An ATX heading's `(level, text)`.
///
/// A bare `#` with nothing after it is not a heading: it is far more likely to
/// be a fragment of a message that is still arriving, and an empty heading
/// draws as nothing at all.
fn heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if rest.is_empty() {
        return None;
    }
    let text = rest.strip_prefix(' ')?;
    Some((hashes as u8, text.trim_end()))
}

/// A `>` line's text, with the marker and one following space removed.
fn quote_line(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// The run of `>` lines starting at `start`, each stripped of one marker.
///
/// A blank line ends the quote even if another `>` line follows: two quotes
/// separated by a blank line are two quotes.
fn gather_quote(lines: &[&str], start: usize) -> (Vec<String>, usize) {
    let mut quoted = Vec::new();
    let mut i = start;
    while let Some(rest) = lines.get(i).and_then(|line| quote_line(line)) {
        quoted.push(rest.to_string());
        i += 1;
    }
    (quoted, i)
}

/// A table's column alignment, if `start` is a header row and `start + 1` is a
/// delimiter row.
fn table_delimiter(lines: &[&str], start: usize) -> Option<Vec<ColumnAlign>> {
    if !lines[start].contains('|') {
        return None;
    }
    parse_delimiter(lines.get(start + 1)?)
}

/// `| --- | :-: | ---: |` → `[Left, Center, Right]`.
fn parse_delimiter(line: &str) -> Option<Vec<ColumnAlign>> {
    let cells = split_row(line);
    if cells.is_empty() {
        return None;
    }

    let mut align = Vec::with_capacity(cells.len());
    for cell in cells {
        let cell = cell.trim();
        let left = cell.starts_with(':');
        let right = cell.ends_with(':');
        let dashes = cell.trim_start_matches(':').trim_end_matches(':');
        if dashes.is_empty() || !dashes.bytes().all(|byte| byte == b'-') {
            return None;
        }
        align.push(match (left, right) {
            (true, true) => ColumnAlign::Center,
            (false, true) => ColumnAlign::Right,
            _ => ColumnAlign::Left,
        });
    }
    Some(align)
}

/// A table's cells, split on `|` and with the outer pipes' empty cells dropped.
///
/// `\|` is a literal pipe, so a cell can hold one.
fn split_row(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cell)),
            _ => cell.push(ch),
        }
    }
    cells.push(cell);

    // `| a | b |` splits to `["", " a ", " b ", ""]`. Only the outer empties go;
    // an interior one is a real, empty column.
    if cells.first().is_some_and(|cell| cell.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|cell| cell.trim().is_empty()) {
        cells.pop();
    }
    cells
}

/// The header row at `start` and every following row, as a `Block::Table`.
///
/// Rows are cut or padded to the delimiter row's width so that a table with a
/// ragged source still has rectangular columns to draw. The source lines are
/// kept alongside the parsed cells, for the narrow-bubble fallback.
fn read_table(lines: &[&str], start: usize, align: Vec<ColumnAlign>) -> (Block, usize) {
    let width = align.len();
    let header = fit_row(split_row(lines[start]), width);

    let mut rows = Vec::new();
    let mut i = start + 2;
    while i < lines.len() && !lines[i].trim().is_empty() && lines[i].contains('|') {
        rows.push(fit_row(split_row(lines[i]), width));
        i += 1;
    }

    let text = lines[start..i].join("\n");
    (
        Block::Table {
            align,
            header,
            rows,
            text,
        },
        i,
    )
}

fn fit_row(cells: Vec<String>, width: usize) -> Vec<Vec<Span>> {
    let mut row: Vec<Vec<Span>> = cells
        .into_iter()
        .take(width)
        .map(|cell| parse_inline(cell.trim()))
        .collect();
    row.resize_with(width, Vec::new);
    row
}

/// A list item's `(depth, marker, text)`, if the line opens one.
fn list_item(line: &str) -> Option<(usize, Marker, &str)> {
    // A tab counts as four columns, which is what a model that indents with
    // tabs means by one level of nesting.
    let indent: usize = line
        .chars()
        .take_while(|ch| ch.is_whitespace())
        .map(|ch| if ch == '\t' { 4 } else { 1 })
        .sum();
    let trimmed = line.trim_start();

    let (marker, rest) = match trimmed.as_bytes().first()? {
        b'-' | b'*' | b'+' => (Marker::Bullet, &trimmed[1..]),
        b'0'..=b'9' => {
            let digits = trimmed
                .bytes()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            let rest = &trimmed[digits..];
            let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
            (Marker::Ordered(trimmed[..digits].parse().ok()?), rest)
        }
        _ => return None,
    };

    // The space is what separates `- item` from `-item`.
    Some((indent / 2, marker, rest.strip_prefix(' ')?))
}

/// An item's spans, plus any continuation lines that belong to it.
///
/// Models wrap list items across lines constantly, and a wrapped second half
/// that fell out of the list would read as a new paragraph. A following line
/// continues the item unless it is blank or opens a block of its own — which is
/// CommonMark's "lazy continuation", and why a flush-left line still belongs
/// here.
fn item_spans(lines: &[&str], start: usize, first: &str) -> (Vec<Span>, usize) {
    let (body, mut hard) = split_break(first);
    let mut text = body.to_string();
    let mut i = start + 1;

    while i < lines.len() {
        if lines[i].trim().is_empty() || starts_block(lines, i) {
            break;
        }
        let (body, next_hard) = split_break(lines[i]);
        // The break belongs to the line *before* it, so it is the previous
        // line's flag that decides this separator.
        text.push(if hard { '\n' } else { ' ' });
        text.push_str(body);
        hard = next_hard;
        i += 1;
    }

    (parse_inline(&text), i)
}

/// Consecutive non-blank lines up to the next block, as one paragraph.
fn paragraph(lines: &[&str], start: usize) -> (Vec<Span>, usize) {
    let mut spans = Vec::new();
    let mut separator: Option<&str> = None;
    let mut i = start;

    while i < lines.len() {
        if lines[i].trim().is_empty() || (i > start && starts_block(lines, i)) {
            break;
        }
        let (body, hard) = split_break(lines[i]);
        if let Some(separator) = separator {
            spans.push(Span::plain(separator));
        }
        spans.extend(parse_inline(body));
        separator = Some(if hard { "\n" } else { " " });
        i += 1;
    }

    (spans, i)
}

/// Splits a line's trailing hard break off, as `(text, is_break)`.
///
/// A soft break becomes a space, not a newline: a paragraph the model hard-
/// wrapped at 80 columns has to re-flow to the bubble's width rather than keep
/// the source's ragged right edge. An odd number of trailing backslashes is a
/// hard break; an even number is an escaped backslash, which `parse_inline`
/// turns back into one.
fn split_break(line: &str) -> (&str, bool) {
    let line = line.trim();
    let trailing = line.len() - line.trim_end_matches('\\').len();
    if trailing % 2 == 1 {
        (&line[..line.len() - 1], true)
    } else {
        (line, false)
    }
}

/// Splits inline text into runs.
///
/// Every arm advances by at least one character, which is what keeps a
/// half-arrived `**bold` from spinning: an unmatched delimiter is literal text,
/// never a reason to look at the same byte twice.
fn parse_inline(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    while i < text.len() {
        let rest = &text[i..];
        let ch = rest.chars().next().expect("i is a char boundary");

        match ch {
            '\\' => {
                // The escaped character is literal, so `\*` is an asterisk and
                // `\\` is one backslash. A trailing backslash is dropped, which
                // is the other half of `append_line`'s hard-break rule.
                let mut chars = rest.chars();
                chars.next();
                match chars.next() {
                    Some(next) => {
                        plain.push(next);
                        i += 1 + next.len_utf8();
                    }
                    None => i += 1,
                }
            }
            '`' => {
                // Code first, so `**not bold**` inside backticks stays literal.
                let ticks = rest.bytes().take_while(|byte| *byte == b'`').count();
                let open = &rest[ticks..];
                match open.find(&"`".repeat(ticks)) {
                    Some(offset) => {
                        flush(&mut spans, &mut plain);
                        spans.push(Span {
                            text: open[..offset].to_string(),
                            style: SpanStyle {
                                code: true,
                                ..Default::default()
                            },
                            link: None,
                        });
                        i += ticks + offset + ticks;
                    }
                    None => {
                        plain.push_str(&rest[..ticks]);
                        i += ticks;
                    }
                }
            }
            '*' | '_' => match emphasis(text, i, ch) {
                Some((inner, style, next)) => {
                    flush(&mut spans, &mut plain);
                    for mut span in parse_inline(inner) {
                        span.style.bold |= style.bold;
                        span.style.italic |= style.italic;
                        spans.push(span);
                    }
                    i = next;
                }
                None => {
                    plain.push(ch);
                    i += ch.len_utf8();
                }
            },
            '[' => match link(text, i) {
                Some((label, url, next)) => {
                    flush(&mut spans, &mut plain);
                    for mut span in parse_inline(label) {
                        span.link = Some(url.clone());
                        spans.push(span);
                    }
                    i = next;
                }
                None => {
                    plain.push('[');
                    i += 1;
                }
            },
            _ => {
                plain.push(ch);
                i += ch.len_utf8();
            }
        }
    }

    flush(&mut spans, &mut plain);
    spans
}

fn flush(spans: &mut Vec<Span>, plain: &mut String) {
    if !plain.is_empty() {
        spans.push(Span::plain(std::mem::take(plain)));
    }
}

/// Emphasis opening at `i`, as `(inner text, style, index after the close)`.
fn emphasis(text: &str, i: usize, ch: char) -> Option<(&str, SpanStyle, usize)> {
    // Three is as deep as the syntax goes, so `****` is a run of three and a
    // literal asterisk rather than something needing a fourth level.
    let run = text[i..].chars().take_while(|c| *c == ch).count().min(3);

    // `_` inside a word is not emphasis: `snake_case_name` and `__init__` are
    // both ordinary text. This is the one rule a naive scanner always misses.
    if ch == '_'
        && text[..i]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
    {
        return None;
    }

    let open_end = i + run * ch.len_utf8();
    let close = find_run(text, open_end, ch, run)?;
    let close_end = close + run * ch.len_utf8();

    if ch == '_'
        && text[close_end..]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric)
    {
        return None;
    }

    let inner = &text[open_end..close];
    if inner.is_empty() {
        return None;
    }

    Some((
        inner,
        SpanStyle {
            bold: run >= 2,
            italic: run != 2,
            code: false,
        },
        close_end,
    ))
}

/// The next run of `ch` that is at least `run` long, at or after `from`.
///
/// A shorter run is skipped rather than closed on, which is what lets
/// `**a *b* c**` keep its inner emphasis.
fn find_run(text: &str, from: usize, ch: char, run: usize) -> Option<usize> {
    let mut i = from;
    while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with(ch) {
            let len = rest.chars().take_while(|c| *c == ch).count();
            if len >= run {
                return Some(i);
            }
            i += len * ch.len_utf8();
        } else {
            i += rest.chars().next().map(char::len_utf8).unwrap_or(1);
        }
    }
    None
}

/// A link opening at `start`, as `(label, url, index after the close)`.
fn link(text: &str, start: usize) -> Option<(&str, String, usize)> {
    let rest = &text[start..];
    let label_end = rest.find("](")?;
    let label = &rest[1..label_end];

    let after = &rest[label_end + 2..];
    let url_end = after.find(')')?;
    let url = after[..url_end].trim();

    if label.is_empty() || url.is_empty() {
        return None;
    }

    Some((label, url.to_string(), start + label_end + 2 + url_end + 1))
}

/// Parses a body and draws it in one step, for a caller with nothing cached.
pub fn draw_text(ui: &mut egui::Ui, p: &Palette, text: &str, salt: impl Hash) {
    draw(ui, p, &parse(text), salt);
}

/// Draws parsed blocks, stacked down the current `Ui`.
///
/// The caller is expected to have put `ui` in a top-down layout. `draw_bubble`
/// does this explicitly, because `Frame::show` inherits its parent's layout and
/// the bubble sits inside a horizontal one — without it, two blocks would be
/// drawn side by side.
///
/// `salt` names the scroll area of every code block, so it has to be stable
/// across frames and unique per bubble. A salt that changed every frame would
/// reset those scroll offsets on every repaint.
pub fn draw(ui: &mut egui::Ui, p: &Palette, blocks: &[Block], salt: impl Hash) {
    draw_blocks(ui, p, blocks, salt_of(salt));
}

/// Folds a salt into one number.
///
/// The recursion below carries a `u64` rather than the caller's own salt type
/// on purpose. `draw_quote` hands a *new* salt to `draw_blocks`, so a salt that
/// stayed generic would grow a type at every level of nesting —
/// `S`, `(S, usize)`, `((S, usize), usize)`, … — and monomorphisation would
/// never terminate.
fn salt_of(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn draw_blocks(ui: &mut egui::Ui, p: &Palette, blocks: &[Block], salt: u64) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            ui.add_space(block_gap(block));
        }
        // Indexed, so that two code blocks in one bubble do not share a scroll
        // area — and so a quote's children cannot collide with its siblings.
        draw_block(ui, p, block, salt_of((salt, index)));
    }
}

/// The space above a block.
///
/// A blank line in the source separates two paragraphs but draws as nothing, so
/// without this the whole body would read as one wall of text.
fn block_gap(block: &Block) -> f32 {
    match block {
        Block::Heading { level, .. } if *level <= 2 => 10.0,
        Block::Heading { .. } => 7.0,
        _ => 6.0,
    }
}

fn draw_block(ui: &mut egui::Ui, p: &Palette, block: &Block, salt: u64) {
    match block {
        Block::Heading { level, spans } => {
            draw_spans(ui, p, spans, heading_size(*level), p.text);
        }
        Block::Paragraph { spans } => {
            draw_spans(ui, p, spans, BODY_SIZE, p.text);
        }
        Block::Item {
            depth,
            marker,
            spans,
        } => draw_item(ui, p, *depth, *marker, spans),
        Block::Code { lang, text } => {
            if !lang.is_empty() {
                ui.label(
                    RichText::new(lang)
                        .size(theme::font(11.0))
                        .color(p.text_muted),
                );
                ui.add_space(2.0);
            }
            // The slab is fixed-width, so a bubble holding a fence stretches to
            // the full bubble width. Measuring the longest line instead would
            // mean reaching into `code_view`'s galley, and a code block that
            // does not line up with the one above it reads worse than a wide
            // one.
            code_view::draw_slab(
                ui,
                p,
                salt,
                &code_view::text_lines(text),
                ui.available_width(),
            );
        }
        Block::Quote { blocks } => draw_quote(ui, p, blocks, salt),
        Block::Rule => {
            ui.add_space(3.0);
            ui.separator();
            ui.add_space(3.0);
        }
        Block::Table {
            align,
            header,
            rows,
            text,
        } => draw_table(ui, p, align, header, rows, text),
    }
}

/// A heading's size.
///
/// The top three levels are visibly larger; 4 to 6 are body size, because a
/// chat bubble is narrow and six distinct sizes do not fit in it.
fn heading_size(level: u8) -> f32 {
    match level {
        1 => theme::font(19.0),
        2 => theme::font(17.0),
        3 => theme::font(15.5),
        _ => BODY_SIZE,
    }
}

/// The format one run is drawn with.
///
/// The size is passed in rather than derived, because every run in a line has
/// to share it: `horizontal_wrapped` centres each widget on its row, so a run
/// at a different size would sit on a different baseline from the text beside
/// it.
fn span_format(p: &Palette, span: &Span, size: f32, colour: Color32) -> TextFormat {
    let font = if span.style.code {
        // Monospace glyphs are wider, so the same size reads larger.
        FontId::monospace(size * 0.92)
    } else {
        FontId::proportional(size)
    };

    let colour = if span.link.is_some() {
        p.accent
    } else {
        colour
    };
    let inline_code = span.style.code;

    TextFormat {
        font_id: font,
        color: colour,
        italics: span.style.italic,
        underline: if span.link.is_some() {
            Stroke::new(1.0, colour)
        } else {
            Stroke::NONE
        },
        background: if inline_code {
            p.code_bg
        } else {
            Color32::TRANSPARENT
        },
        // The only padding a background run has; there is no per-side control.
        expand_bg: if inline_code { 2.0 } else { 1.0 },
        ..Default::default()
    }
}

/// Draws a run of inline spans as one paragraph.
fn draw_spans(ui: &mut egui::Ui, p: &Palette, spans: &[Span], size: f32, colour: Color32) {
    if spans.is_empty() {
        return;
    }

    if spans.iter().any(|span| span.link.is_some()) {
        draw_linked_spans(ui, p, spans, size, colour);
        return;
    }

    // One job, so the paragraph is a single selectable run and egui caches its
    // galley between frames.
    let mut job = LayoutJob::default();
    for span in spans {
        job.append(&span.text, 0.0, span_format(p, span, size, colour));
    }
    ui.add(egui::Label::new(job).wrap().selectable(true));
}

/// A run that contains a link, drawn one span at a time.
///
/// A `LayoutJob` cannot carry a URL, so a paragraph with a link in it gives up
/// the single selectable galley in exchange for a link the user can click.
/// Paragraphs without links keep the fast path in `draw_spans`.
fn draw_linked_spans(ui: &mut egui::Ui, p: &Palette, spans: &[Span], size: f32, colour: Color32) {
    ui.horizontal_wrapped(|ui| {
        // The default spacing would open an 8 px gap at every run boundary, and
        // the spaces are already in the text.
        ui.spacing_mut().item_spacing.x = 0.0;

        for span in spans {
            let mut job = LayoutJob::default();
            job.append(&span.text, 0.0, span_format(p, span, size, colour));
            // A long path or identifier is one unbreakable token, and would
            // otherwise push the bubble wider than the transcript.
            job.wrap.break_anywhere = true;

            match &span.link {
                Some(url) => {
                    ui.hyperlink_to(job, url);
                }
                None => {
                    ui.add(egui::Label::new(job).wrap().selectable(true));
                }
            }
        }
    });
}

/// One list item, with its marker in a column of its own.
///
/// The column is what makes a wrapped item's second line start under its text
/// rather than under its bullet: the text `Ui` begins at the column's right
/// edge, and every row of it starts there.
fn draw_item(ui: &mut egui::Ui, p: &Palette, depth: usize, marker: Marker, spans: &[Span]) {
    let token = match marker {
        Marker::Bullet => "•".to_string(),
        Marker::Ordered(number) => format!("{number}."),
    };

    ui.horizontal_top(|ui| {
        ui.add_space(depth as f32 * LIST_INDENT);
        ui.allocate_ui_with_layout(
            Vec2::new(LIST_MARKER, 0.0),
            // Right-aligned, so `9.` and `10.` share an edge.
            Layout::top_down(Align::Max),
            |ui| {
                ui.label(RichText::new(token).size(BODY_SIZE).color(p.text_muted));
            },
        );
        draw_spans(ui, p, spans, BODY_SIZE, p.text);
    });
}

/// A quote, indented behind a bar down its left edge.
///
/// `Frame` has one stroke for all four sides, so the bar is painted rather than
/// framed — the same trick `code_view` uses for the rule under a panel title.
fn draw_quote(ui: &mut egui::Ui, p: &Palette, blocks: &[Block], salt: u64) {
    let inner = Frame::NONE
        .inner_margin(Margin {
            left: QUOTE_INDENT,
            right: 0,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            // Belt and braces: the bubble is already top-down, but a quote
            // drawn anywhere else must not silently go horizontal.
            ui.vertical(|ui| draw_blocks(ui, p, blocks, salt));
        });

    let rect = inner.response.rect;
    ui.painter()
        .vline(rect.left(), rect.y_range(), Stroke::new(3.0, p.text_muted));
}

/// How a table column reads, which decides what it gives up first when the
/// table is too wide for the bubble.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    /// Paths, URLs and hashes: a few very long unbreakable tokens.
    TokenHeavy,
    /// Prose. Wants its width most, but wraps cleanly when it does not get it.
    Narrative,
    /// Short values — a count, a status, a date.
    Compact,
}

/// A column's measured shape, the input to [`fit_widths`].
struct ColumnMetrics {
    /// The widest cell, so a column never grows past what it holds.
    max: f32,
    /// The widest single token. A column narrower than this breaks a word.
    token: f32,
    kind: ColumnKind,
}

/// A drawn cell: its laid-out galley and the width that galley needs.
struct CellText {
    galley: std::sync::Arc<egui::Galley>,
    width: f32,
    height: f32,
}

/// A table, laid out as a stack of cells of a computed width.
///
/// The widths follow the approach in `codex-rs/tui`: columns are measured,
/// classified by how they read, and shrunk by priority when the table is too
/// wide. A table that cannot be made legible as a grid — see
/// [`should_render_records`] — is transposed into label/value records instead,
/// and one too narrow even for that is redrawn as its source.
///
/// Hand-laid rather than an `egui::Grid`: the grid sizes columns from its
/// content, which lets a wide table overflow the bubble, and aligning a cell
/// inside its column is not something `Label::halign` can do.
fn draw_table(
    ui: &mut egui::Ui,
    p: &Palette,
    align: &[ColumnAlign],
    header: &[Vec<Span>],
    rows: &[Vec<Vec<Span>>],
    source: &str,
) {
    let columns = align.len();
    if columns == 0 {
        return;
    }

    let regular = FontId::proportional(TABLE_SIZE);
    let metrics = measure_columns(ui, header, rows, &regular, columns);

    let gap_total = TABLE_SPACING * (columns - 1) as f32;
    let available = (ui.available_width() - gap_total).max(1.0);

    // Three shapes for the same data, widest first. `codex-rs` picks between
    // them the same way; the grid is the goal, records are what a grid becomes
    // when a column would otherwise be a stub, and the source is what a table
    // becomes when the bubble is too narrow to hold even a record.
    let widths = fit_widths(&metrics, available);
    let Some(widths) = widths else {
        record_or_source(ui, p, header, rows, source, &regular);
        return;
    };
    if should_render_records(rows, &widths, &metrics) {
        record_or_source(ui, p, header, rows, source, &regular);
        return;
    }

    let total: f32 = widths.iter().sum::<f32>() + gap_total;
    // A sub-`Ui` of the table's own width, so the separator rules stop at the
    // table rather than running on to the edge of the bubble.
    ui.allocate_ui_with_layout(Vec2::new(total, 0.0), Layout::top_down(Align::Min), |ui| {
        draw_grid_row(ui, p, header, &widths, align, &regular);
        ui.add_space(TABLE_V_PAD);
        draw_rule(ui, p, total);
        ui.add_space(TABLE_V_PAD);
        for (index, row) in rows.iter().enumerate() {
            draw_grid_row(ui, p, row, &widths, align, &regular);
            if index < rows.len() - 1 {
                ui.add_space(TABLE_V_PAD);
                draw_rule(ui, p, total);
                ui.add_space(TABLE_V_PAD);
            }
        }
    });
}

/// Records when they fit, the source when even those do not.
fn record_or_source(
    ui: &mut egui::Ui,
    p: &Palette,
    header: &[Vec<Span>],
    rows: &[Vec<Vec<Span>>],
    source: &str,
    font: &FontId,
) {
    let labels: Vec<String> = header
        .iter()
        .map(|cell| cell.iter().map(|span| span.text.as_str()).collect())
        .collect();
    let label_width = labels
        .iter()
        .map(|label| measure_spans(ui, &[Span::plain(label.clone())], font))
        .fold(0.0f32, f32::max);
    // `  label  value` has to leave room for the value; below that, a record
    // is as unreadable as the grid was.
    if label_width + TABLE_SPACING + TABLE_MIN_COLUMN > ui.available_width() {
        draw_source(ui, p, source);
        return;
    }
    draw_records(
        ui,
        p,
        &labels,
        label_width.min(ui.available_width() * 0.5),
        rows,
    );
}

/// One grid row: cells of fixed width, each wrapped inside its column.
fn draw_grid_row(
    ui: &mut egui::Ui,
    p: &Palette,
    cells: &[Vec<Span>],
    widths: &[f32],
    align: &[ColumnAlign],
    font: &FontId,
) {
    let laid: Vec<Option<CellText>> = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| layout_cell(ui, p, cell, *width, font, p.text))
        .collect();
    let height = laid
        .iter()
        .flatten()
        .fold(0.0f32, |tallest, cell| tallest.max(cell.height));

    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = TABLE_SPACING;
        // Every column is given a fixed box, whether or not it has a galley to
        // put in it. A layout nested per column would size that box from the
        // cell's own galley, and an empty cell — an empty header column, say —
        // would report a zero width and pull every column after it out of line
        // with the body rows.
        for (column, cell) in laid.iter().enumerate() {
            let width = widths[column];
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(width, height.max(1.0)), egui::Sense::hover());
            let Some(cell) = cell else { continue };
            // `allocate_exact_size` ignores the column's alignment, so a centre
            // or right column is aligned by hand: the galley is drawn at the
            // rect's left plus the alignment slack. A wrapped cell is also
            // centred vertically, so a one-line cell beside a three-line one
            // sits on the middle line rather than the first.
            let left = match align[column] {
                ColumnAlign::Left => 0.0,
                ColumnAlign::Center => (width - cell.width) * 0.5,
                ColumnAlign::Right => width - cell.width,
            }
            .max(0.0);
            let top = ((height - cell.height) * 0.5).max(0.0);
            ui.painter().galley(
                rect.left_top() + Vec2::new(left, top),
                cell.galley.clone(),
                Color32::PLACEHOLDER,
            );
        }
    });
}

/// A hairline between two grid rows, the width of the table.
fn draw_rule(ui: &mut egui::Ui, p: &Palette, width: f32) {
    let y = ui.cursor().top() + 0.5;
    let x = ui.cursor().left();
    ui.painter()
        .hline(x..=x + width, y, Stroke::new(1.0, p.border));
    ui.add_space(1.0);
}

/// The last resort: the table's own source, monospace and wrapped.
///
/// Monospace because that is the only form in which the `|` separators of a
/// table line up, and muted because it is a fallback rather than the message.
fn draw_source(ui: &mut egui::Ui, p: &Palette, source: &str) {
    let mut job = LayoutJob::default();
    job.wrap.break_anywhere = true;
    job.append(
        source,
        0.0,
        TextFormat {
            font_id: FontId::monospace(TABLE_SIZE * 0.92),
            color: p.text_muted,
            ..Default::default()
        },
    );
    ui.add(egui::Label::new(job).wrap().selectable(true));
}

/// Lays a cell out at `width`, wrapped and with its inline styles applied.
///
/// Returns `None` for an empty cell, which has no galley worth drawing.
fn layout_cell(
    ui: &egui::Ui,
    p: &Palette,
    spans: &[Span],
    width: f32,
    font: &FontId,
    colour: Color32,
) -> Option<CellText> {
    if spans.iter().all(|span| span.text.is_empty()) {
        return None;
    }
    let mut job = LayoutJob::default();
    // A path or a URL has no spaces to wrap at, so a cell must be allowed to
    // break mid-token; the column floor is what keeps that from happening to
    // prose.
    job.wrap.break_anywhere = true;
    job.wrap.max_width = width;
    for span in spans {
        job.append(&span.text, 0.0, cell_format(p, span, font, colour));
    }
    let galley = ui.painter().layout_job(job);
    Some(CellText {
        width: galley.size().x,
        height: galley.size().y,
        galley,
    })
}

/// The format a cell run is drawn with: [`span_format`], but with the table's
/// own font as the base, so every run shares one baseline and one size.
fn cell_format(p: &Palette, span: &Span, font: &FontId, colour: Color32) -> TextFormat {
    let mut format = span_format(p, span, font.size, colour);
    if span.style.code {
        format.font_id = FontId::monospace(font.size * 0.92);
    } else {
        format.font_id = font.clone();
    }
    format
}

/// The measured shape of every column.
///
/// The same pass records each column's [`ColumnKind`], from the mix of very
/// long tokens and many-word cells in its body rows.
fn measure_columns(
    ui: &egui::Ui,
    header: &[Vec<Span>],
    rows: &[Vec<Vec<Span>>],
    regular: &FontId,
    columns: usize,
) -> Vec<ColumnMetrics> {
    let mut metrics = (0..columns)
        .map(|_| ColumnMetrics {
            max: 0.0,
            token: 0.0,
            kind: ColumnKind::Compact,
        })
        .collect::<Vec<_>>();

    for (column, cell) in header.iter().enumerate().take(columns) {
        let width = measure_spans(ui, cell, regular);
        metrics[column].max = metrics[column].max.max(width);
        metrics[column].token = metrics[column].token.max(longest_token(ui, cell, regular));
    }

    for row in rows {
        for (column, cell) in row.iter().enumerate().take(columns) {
            let width = measure_spans(ui, cell, regular);
            metrics[column].max = metrics[column].max.max(width);
            metrics[column].token = metrics[column].token.max(longest_token(ui, cell, regular));
        }
    }

    for (column, metric) in metrics.iter_mut().enumerate() {
        metric.kind = classify_column(rows, column);
    }
    metrics
}

/// The width `spans` occupy when drawn in `font`, with no wrapping.
fn measure_spans(ui: &egui::Ui, spans: &[Span], font: &FontId) -> f32 {
    spans
        .iter()
        .map(|span| {
            let font = if span.style.code {
                FontId::monospace(font.size * 0.92)
            } else {
                font.clone()
            };
            ui.painter()
                .layout_no_wrap(span.text.clone(), font, Color32::PLACEHOLDER)
                .size()
                .x
        })
        .sum()
}

/// The widest single whitespace-delimited token in a cell.
fn longest_token(ui: &egui::Ui, spans: &[Span], font: &FontId) -> f32 {
    spans
        .iter()
        .flat_map(|span| span.text.split_whitespace())
        .map(|token| {
            ui.painter()
                .layout_no_wrap(token.to_string(), font.clone(), Color32::PLACEHOLDER)
                .size()
                .x
        })
        .fold(0.0f32, f32::max)
}

/// How a column reads, from the shape of its body cells.
///
/// Follows `codex-rs`: a column of *unbreakable* tokens is `TokenHeavy` and
/// gives width up first; a column of many-word cells is `Narrative` and wraps;
/// everything else keeps its width.
fn classify_column(rows: &[Vec<Vec<Span>>], column: usize) -> ColumnKind {
    let mut long_tokens = 0usize;
    let mut words = 0usize;
    let mut cells_with_long_token = 0usize;
    let mut filled_cells = 0usize;

    for row in rows {
        let Some(cell) = row.get(column) else {
            continue;
        };
        let text = cell
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        if text.trim().is_empty() {
            continue;
        }
        filled_cells += 1;
        let mut has_long = false;
        for word in text.split_whitespace() {
            words += 1;
            if word.chars().count() >= TABLE_LONG_TOKEN {
                long_tokens += 1;
                has_long = true;
            }
        }
        cells_with_long_token += usize::from(has_long);
    }

    if filled_cells == 0 {
        return ColumnKind::Compact;
    }
    if long_tokens * 2 >= words || cells_with_long_token * 2 >= filled_cells {
        ColumnKind::TokenHeavy
    } else if words as f32 / filled_cells as f32 >= TABLE_PROSE_WORDS {
        ColumnKind::Narrative
    } else {
        ColumnKind::Compact
    }
}

impl ColumnMetrics {
    /// The narrowest this column may be before it stops being worth showing.
    ///
    /// A `TokenHeavy` or `Narrative` column keeps a readable floor; a `Compact`
    /// column keeps its longest token, so `200` never becomes `20`.
    fn minimum(&self) -> f32 {
        match self.kind {
            ColumnKind::Compact => self.token.min(TABLE_MIN_COLUMN * 2.0),
            _ => TABLE_MIN_COLUMN,
        }
        .max(TABLE_MIN_COLUMN)
        .min(self.max.max(TABLE_MIN_COLUMN))
    }
}

/// Column widths that fit `available`, or `None` if even the floors cannot.
///
/// Columns start at their widest cell and shrink by priority: `TokenHeavy`
/// gives up width first, then `Narrative`, and `Compact` last — a path column
/// collapsing to a few characters is a worse outcome than prose wrapping.
fn fit_widths(metrics: &[ColumnMetrics], available: f32) -> Option<Vec<f32>> {
    let mut widths: Vec<f32> = metrics
        .iter()
        .map(|column| column.max.clamp(TABLE_MIN_COLUMN, TABLE_CELL_CAP))
        .collect();
    let floors: Vec<f32> = metrics.iter().map(ColumnMetrics::minimum).collect();

    let total: f32 = widths.iter().sum();
    if total <= available {
        return Some(widths);
    }
    if floors.iter().sum::<f32>() > available {
        return None;
    }

    // Repeatedly take one cell of width from the column with the most slack,
    // cheapest first; a few hundred iterations is far below the cost of the
    // layout that follows.
    for _ in 0..TABLE_SHRINK_STEPS {
        let total: f32 = widths.iter().sum();
        if total <= available {
            return Some(widths);
        }
        let mut pick: Option<usize> = None;
        for (index, width) in widths.iter().enumerate() {
            if *width - floors[index] < 1.0 {
                continue;
            }
            let better = match pick {
                None => true,
                Some(chosen) => width - floors[index] > widths[chosen] - floors[chosen],
            };
            if better {
                pick = Some(index);
            }
        }
        let index = pick?;
        widths[index] -= 1.0;
    }
    (widths.iter().sum::<f32>() <= available).then_some(widths)
}

/// Whether the grid should become label/value records instead.
///
/// True once enough rows hold a value the grid cannot show whole: a token wider
/// than its column where the column is too narrow to keep it, or a `TokenHeavy`
/// cell shredded into fragments. One bad row is noise; a third is a shape.
fn should_render_records(
    rows: &[Vec<Vec<Span>>],
    widths: &[f32],
    metrics: &[ColumnMetrics],
) -> bool {
    if rows.is_empty() {
        return false;
    }
    let affected = rows
        .iter()
        .filter(|row| {
            row.iter()
                .zip(widths)
                .zip(metrics)
                .any(|((cell, width), metric)| {
                    if metric.kind == ColumnKind::Narrative {
                        return false;
                    }
                    let text = cell
                        .iter()
                        .map(|span| span.text.as_str())
                        .collect::<String>();
                    let fragment = text
                        .split_whitespace()
                        .any(|word| word.chars().count() as f32 * cell_char_width() > *width);
                    match metric.kind {
                        ColumnKind::Compact => fragment && *width < metric.token,
                        ColumnKind::TokenHeavy => fragment,
                        ColumnKind::Narrative => false,
                    }
                })
        })
        .count();
    let threshold = if rows.len() == 1 {
        1
    } else {
        2.max(rows.len().div_ceil(3))
    };
    affected >= threshold
}

/// A conservative width for one character of cell text.
///
/// The token measure in [`longest_token`] is exact but needs a `Painter`;
/// [`should_render_records`] is pure so it can be tested without one. Chinese
/// and other CJK glyphs are twice as wide as Latin ones, so the estimate is
/// built on the widest case to keep a `TokenHeavy` column from looking safe
/// when it is not.
fn cell_char_width() -> f32 {
    TABLE_SIZE * 0.6
}

/// The body as `label  value` records, one group per source row.
///
/// The grid is unusable at this width, but the data still is not: a label on
/// its own line beats a cell three characters wide. Every label keeps the same
/// width, so the values line up into a column.
fn draw_records(
    ui: &mut egui::Ui,
    p: &Palette,
    labels: &[String],
    label_width: f32,
    rows: &[Vec<Vec<Span>>],
) {
    let regular = FontId::proportional(TABLE_SIZE);
    let label_font = regular.clone();
    for (index, row) in rows.iter().enumerate() {
        for (head, value) in labels.iter().zip(row) {
            if value.is_empty() {
                continue;
            }
            // The label is laid right-aligned into its own box, and the value
            // is wrapped to whatever is left, so a long value breaks inside its
            // own column instead of running under the label.
            let value_width = (ui.available_width() - label_width - TABLE_SPACING).max(1.0);
            let label = layout_cell(
                ui,
                p,
                &[Span::plain(head.clone())],
                label_width,
                &label_font,
                p.text_muted,
            );
            let value = layout_cell(ui, p, value, value_width, &regular, p.text);
            let height = label
                .iter()
                .chain(value.iter())
                .fold(0.0f32, |tallest, cell| tallest.max(cell.height));
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = TABLE_SPACING;
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(label_width, height), egui::Sense::hover());
                if let Some(label) = &label {
                    ui.painter().galley(
                        egui::pos2(rect.right() - label.width, rect.top()),
                        label.galley.clone(),
                        Color32::PLACEHOLDER,
                    );
                }
                if let Some(value) = &value {
                    let (rect, _) = ui
                        .allocate_exact_size(Vec2::new(value_width, height), egui::Sense::hover());
                    ui.painter().galley(
                        rect.left_top(),
                        value.galley.clone(),
                        Color32::PLACEHOLDER,
                    );
                }
            });
        }
        if index < rows.len() - 1 {
            ui.add_space(TABLE_V_PAD);
            draw_rule(ui, p, ui.available_width());
            ui.add_space(TABLE_V_PAD);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str) -> Vec<Span> {
        parse_inline(text)
    }

    fn paragraph_of(text: &str) -> Vec<Span> {
        match parse(text).as_slice() {
            [Block::Paragraph { spans }] => spans.clone(),
            other => panic!("expected one paragraph, got {other:?}"),
        }
    }

    fn plain(text: &str) -> Span {
        Span::plain(text)
    }

    fn styled(text: &str, bold: bool, italic: bool) -> Span {
        Span {
            text: text.into(),
            style: SpanStyle {
                bold,
                italic,
                code: false,
            },
            link: None,
        }
    }

    fn code(text: &str) -> Span {
        Span {
            text: text.into(),
            style: SpanStyle {
                code: true,
                ..Default::default()
            },
            link: None,
        }
    }

    #[test]
    fn headings_keep_their_level() {
        for level in 1..=6u8 {
            let source = format!("{} title", "#".repeat(level as usize));
            match parse(&source).as_slice() {
                [Block::Heading { level: got, spans }] => {
                    assert_eq!(*got, level);
                    assert_eq!(spans, &[plain("title")]);
                }
                other => panic!("{source:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn a_heading_needs_a_space_and_at_most_six_hashes() {
        for source in ["####### seven", "#nospace", "#"] {
            assert!(
                matches!(parse(source).as_slice(), [Block::Paragraph { .. }]),
                "{source:?} should be a paragraph"
            );
        }
    }

    #[test]
    fn a_soft_break_joins_with_a_space() {
        assert_eq!(
            paragraph_of("one\ntwo"),
            vec![plain("one"), plain(" "), plain("two")]
        );
    }

    #[test]
    fn a_blank_line_splits_paragraphs() {
        let blocks = parse("one\n\ntwo");
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn a_rule_is_not_a_bullet() {
        for source in ["---", "***", "___", "- - -"] {
            assert_eq!(parse(source), vec![Block::Rule], "{source:?}");
        }
        assert!(matches!(
            parse("- item").as_slice(),
            [Block::Item {
                marker: Marker::Bullet,
                ..
            }]
        ));
        // Two dashes are not a rule, and `---x` is not one either.
        assert!(matches!(parse("--").as_slice(), [Block::Paragraph { .. }]));
        assert!(matches!(
            parse("---x").as_slice(),
            [Block::Paragraph { .. }]
        ));
    }

    #[test]
    fn a_fence_keeps_its_body_verbatim() {
        match parse("```rust\nfn f() {}\n```").as_slice() {
            [Block::Code { lang, text }] => {
                assert_eq!(lang, "rust");
                assert_eq!(text, "fn f() {}\n");
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_fence_body_is_not_inline_parsed() {
        match parse("```\n**not bold**\n```").as_slice() {
            [Block::Code { text, .. }] => assert_eq!(text, "**not bold**\n"),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn an_unterminated_fence_swallows_the_rest() {
        // The mid-stream case: the closing fence has not arrived yet.
        match parse("```\nlet x = 1;\nlet y = 2;").as_slice() {
            [Block::Code { lang, text }] => {
                assert_eq!(lang, "");
                assert_eq!(text, "let x = 1;\nlet y = 2;\n");
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_fence_after_a_paragraph_starts_its_own_block() {
        let blocks = parse("text\n```\ncode\n```");
        assert!(matches!(blocks[0], Block::Paragraph { .. }));
        assert!(matches!(blocks[1], Block::Code { .. }));
    }

    #[test]
    fn tildes_fence_too() {
        assert!(matches!(
            parse("~~~\ncode\n~~~").as_slice(),
            [Block::Code { .. }]
        ));
    }

    #[test]
    fn every_bullet_character_works() {
        for source in ["- a", "* a", "+ a"] {
            match parse(source).as_slice() {
                [Block::Item { marker, spans, .. }] => {
                    assert_eq!(*marker, Marker::Bullet);
                    assert_eq!(spans, &[plain("a")]);
                }
                other => panic!("{source:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn an_ordered_item_keeps_the_number_it_was_written_with() {
        for (source, number) in [("1. a", 1), ("3) b", 3), ("10. c", 10)] {
            match parse(source).as_slice() {
                [Block::Item { marker, .. }] => {
                    assert_eq!(*marker, Marker::Ordered(number), "{source:?}");
                }
                other => panic!("{source:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn indentation_is_depth() {
        let blocks = parse("- one\n  - two\n    - three");
        let depths: Vec<usize> = blocks
            .iter()
            .map(|block| match block {
                Block::Item { depth, .. } => *depth,
                other => panic!("parsed as {other:?}"),
            })
            .collect();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn a_wrapped_item_line_stays_in_the_item() {
        let blocks = parse("- first half\n  second half");
        match blocks.as_slice() {
            [Block::Item { spans, .. }] => {
                assert_eq!(spans, &[plain("first half second half")]);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_blank_line_ends_a_list_item() {
        let blocks = parse("- item\n\nseparate");
        assert_eq!(blocks.len(), 2);
        assert!(matches!(blocks[1], Block::Paragraph { .. }));
    }

    #[test]
    fn a_quote_run_is_one_quote() {
        match parse("> a\n> b").as_slice() {
            [Block::Quote { blocks }] => match blocks.as_slice() {
                [Block::Paragraph { spans }] => {
                    assert_eq!(spans, &[plain("a"), plain(" "), plain("b")]);
                }
                other => panic!("quote held {other:?}"),
            },
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn quotes_nest() {
        match parse(">> deep").as_slice() {
            [Block::Quote { blocks }] => {
                assert!(matches!(blocks.as_slice(), [Block::Quote { .. }]));
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_quote_can_hold_a_heading() {
        match parse("> # h").as_slice() {
            [Block::Quote { blocks }] => {
                assert!(matches!(blocks.as_slice(), [Block::Heading { .. }]));
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_blank_line_ends_a_quote() {
        let blocks = parse("> a\n\n> b");
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn a_table_reads_its_alignment_row() {
        match parse("| a | b | c |\n|:--|--:|:-:|\n| 1 | 2 | 3 |").as_slice() {
            [Block::Table {
                align,
                header,
                rows,
                ..
            }] => {
                assert_eq!(
                    align,
                    &[ColumnAlign::Left, ColumnAlign::Right, ColumnAlign::Center]
                );
                assert_eq!(header.len(), 3);
                assert_eq!(header[0], vec![plain("a")]);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][2], vec![plain("3")]);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    /// A bubble too narrow for the parsed cells has to fall back to the
    /// source, so the parser has to keep it.
    #[test]
    fn a_table_keeps_its_source() {
        const SOURCE: &str = "| a | b |\n|---|---|\n| 1 | 2 |";
        match parse(SOURCE).as_slice() {
            [Block::Table { text, .. }] => assert_eq!(text, SOURCE),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_ragged_table_is_made_rectangular() {
        match parse("| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |").as_slice() {
            [Block::Table { rows, .. }] => {
                assert_eq!(rows.len(), 2);
                assert!(rows.iter().all(|row| row.len() == 2));
                assert_eq!(rows[0][1], Vec::<Span>::new());
                assert_eq!(rows[1][1], vec![plain("2")]);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_pipe_line_without_a_delimiter_row_is_a_paragraph() {
        assert!(matches!(
            parse("| a | b |\n| c | d |").as_slice(),
            [Block::Paragraph { .. }]
        ));
    }

    #[test]
    fn a_table_is_not_swallowed_by_a_paragraph() {
        let blocks = parse("text\n| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(matches!(blocks[0], Block::Paragraph { .. }));
        assert!(matches!(blocks[1], Block::Table { .. }));
    }

    #[test]
    fn a_row_can_escape_a_pipe() {
        match parse("| a |\n|---|\n| x \\| y |").as_slice() {
            [Block::Table { rows, .. }] => assert_eq!(rows[0][0], vec![plain("x | y")]),
            other => panic!("parsed as {other:?}"),
        }
    }

    fn metric(max: f32, token: f32, kind: ColumnKind) -> ColumnMetrics {
        ColumnMetrics { max, token, kind }
    }

    #[test]
    fn a_table_that_fits_is_left_alone() {
        let metrics = [
            metric(42.0, 20.0, ColumnKind::Compact),
            metric(100.0, 40.0, ColumnKind::Compact),
        ];
        assert_eq!(fit_widths(&metrics, 200.0), Some(vec![42.0, 100.0]));
    }

    /// The bug this guards: a short commit id beside a paragraph of text used
    /// to be scaled down with everything else and wrapped into `cff` / `549` /
    /// `5`. The paragraph is the column that can afford to wrap.
    #[test]
    fn a_compact_column_is_not_squeezed_to_fit_a_wide_one() {
        let metrics = [
            metric(42.0, 42.0, ColumnKind::Compact),
            metric(600.0, 30.0, ColumnKind::Narrative),
        ];
        let widths = fit_widths(&metrics, 300.0).expect("the floors fit");

        assert_eq!(widths[0], 42.0, "the compact column keeps its width");
        assert!(
            widths[1] <= 258.0 + 0.01,
            "the prose column pays: {widths:?}"
        );
        assert!(widths.iter().sum::<f32>() <= 300.0 + 0.01);
    }

    /// The point of classifying columns at all: a path is what should give
    /// width up, not the prose or the status beside it.
    #[test]
    fn a_token_heavy_column_gives_width_up_first() {
        let metrics = [
            metric(200.0, 200.0, ColumnKind::TokenHeavy),
            metric(120.0, 60.0, ColumnKind::Narrative),
            metric(40.0, 40.0, ColumnKind::Compact),
        ];
        let widths = fit_widths(&metrics, 240.0).expect("the floors fit");

        assert_eq!(widths[2], 40.0, "the status column is untouched");
        assert!(
            widths[0] < 200.0 && widths[1] < 120.0,
            "both wide columns shrink: {widths:?}"
        );
        assert!(
            widths[1] >= widths[0],
            "the path gives up more than the prose: {widths:?}"
        );
    }

    /// Past the floor there is nothing left to give; the caller falls back to
    /// records rather than drawing a grid of stubs.
    #[test]
    fn an_impossible_table_reports_no_fit() {
        let metrics = [
            metric(24.0, 24.0, ColumnKind::Compact),
            metric(24.0, 24.0, ColumnKind::Compact),
            metric(24.0, 24.0, ColumnKind::Compact),
        ];
        assert_eq!(fit_widths(&metrics, 30.0), None);
    }

    /// A path column and a prose column must not classify the same, or the
    /// shrink has nothing to work with.
    #[test]
    fn columns_are_classified_by_their_body() {
        let path = vec![vec![
            vec![plain("/usr/local/lib/something/deep/inside/the/tree.rs")],
            vec![plain("another/very/long/unbroken/path/to/a/file.rs")],
        ]];
        let prose = vec![vec![
            vec![plain("This cell holds a sentence of several words")],
            vec![plain("and so does this one, which is prose too")],
        ]];
        let count = vec![vec![vec![plain("42")], vec![plain("7")]]];

        assert_eq!(classify_column(&path, 0), ColumnKind::TokenHeavy);
        assert_eq!(classify_column(&prose, 0), ColumnKind::Narrative);
        assert_eq!(classify_column(&count, 0), ColumnKind::Compact);
    }

    /// A single shredded value is noise; a table where most rows cannot be
    /// shown whole is the wrong shape and transposes to records.
    #[test]
    fn records_replace_a_grid_that_cannot_show_its_values() {
        let rows = vec![
            vec![vec![plain("aaaa-bbbb-cccc-dddd")], vec![plain("x")]],
            vec![vec![plain("eeee-ffff-gggg-hhhh")], vec![plain("y")]],
            vec![vec![plain("iiii-jjjj-kkkk-llll")], vec![plain("z")]],
        ];
        let metrics = [
            metric(200.0, 200.0, ColumnKind::Compact),
            metric(12.0, 12.0, ColumnKind::Compact),
        ];
        let widths = [18.0, 30.0];
        assert!(should_render_records(&rows, &widths, &metrics));

        let roomy = [200.0, 30.0];
        assert!(!should_render_records(&rows, &roomy, &metrics));
    }

    #[test]
    fn empty_input_has_no_blocks() {
        assert!(parse("").is_empty());
        assert!(parse("\n \n\t\n").is_empty());
    }

    #[test]
    fn emphasis_takes_its_weight_from_the_run_length() {
        assert_eq!(spans("**b**"), vec![styled("b", true, false)]);
        assert_eq!(spans("*i*"), vec![styled("i", false, true)]);
        assert_eq!(spans("***bi***"), vec![styled("bi", true, true)]);
        assert_eq!(spans("__b__"), vec![styled("b", true, false)]);
        assert_eq!(spans("_i_"), vec![styled("i", false, true)]);
    }

    #[test]
    fn emphasis_nests() {
        assert_eq!(
            spans("**a *b* c**"),
            vec![
                styled("a ", true, false),
                styled("b", true, true),
                styled(" c", true, false),
            ]
        );
    }

    #[test]
    fn an_unmatched_delimiter_is_literal() {
        assert_eq!(spans("**x"), vec![plain("**x")]);
        assert_eq!(spans("*x"), vec![plain("*x")]);
        assert_eq!(spans("a * b"), vec![plain("a * b")]);
    }

    #[test]
    fn an_underscore_inside_a_word_is_not_emphasis() {
        for source in ["snake_case_name", "a_b_c", "foo_bar_baz"] {
            assert_eq!(spans(source), vec![plain(source)], "{source:?}");
        }
        // At the start of a run there is no preceding word character, so the
        // opener is allowed — `__init__` is emphasis, exactly as it is in
        // CommonMark, despite reading like a Python dunder.
        assert_eq!(spans("__init__"), vec![styled("init", true, false)]);
    }

    #[test]
    fn a_code_span_wins_over_emphasis() {
        assert_eq!(spans("`**not bold**`"), vec![code("**not bold**")]);
    }

    #[test]
    fn an_unmatched_backtick_is_literal() {
        assert_eq!(spans("a ` b"), vec![plain("a ` b")]);
    }

    #[test]
    fn a_double_backtick_span_can_hold_a_backtick() {
        assert_eq!(spans("``a`b``")[0].text, "a`b");
    }

    #[test]
    fn a_backslash_escapes_the_next_character() {
        assert_eq!(spans("\\*x\\*"), vec![plain("*x*")]);
        assert_eq!(spans("\\\\"), vec![plain("\\")]);
        assert_eq!(spans("\\["), vec![plain("[")]);
    }

    #[test]
    fn a_link_carries_its_url_on_every_span() {
        assert_eq!(
            spans("[label](https://x)"),
            vec![Span {
                text: "label".into(),
                style: SpanStyle::default(),
                link: Some("https://x".into()),
            }]
        );
    }

    #[test]
    fn an_unclosed_link_is_literal() {
        assert_eq!(spans("[label]"), vec![plain("[label]")]);
        assert_eq!(spans("[label]("), vec![plain("[label](")]);
    }

    #[test]
    fn a_trailing_backslash_is_a_hard_break() {
        assert_eq!(
            paragraph_of("one\\\ntwo"),
            vec![plain("one"), plain("\n"), plain("two")]
        );
    }

    #[test]
    fn an_even_run_of_backslashes_is_an_escape_not_a_break() {
        // `one\\` is the word "one\" — the backslashes cancel, so the newline is
        // still a soft break.
        assert_eq!(
            paragraph_of("one\\\\\ntwo"),
            vec![plain("one\\"), plain(" "), plain("two")]
        );
    }

    #[test]
    fn cjk_survives_the_scanner() {
        // Guards the byte-versus-char advance: a byte-wise scanner would split
        // these and panic on the slice.
        assert_eq!(spans("中文测试"), vec![plain("中文测试")]);
        assert_eq!(
            spans("**粗体**中文"),
            vec![styled("粗体", true, false), plain("中文")]
        );
        assert_eq!(spans("`代码`中文"), vec![code("代码"), plain("中文")]);
        assert_eq!(spans("中文[链接](https://x)中文")[1].text, "链接");
    }

    #[test]
    fn a_paragraph_that_never_closes_terminates() {
        // The property that keeps a half-streamed message from hanging the UI:
        // every arm advances.
        for source in ["**", "*", "`", "[", "\\", "***", "[](", "_"] {
            let _ = parse(source);
        }
    }

    /// The bug this guards: an empty first header cell used to size its column
    /// from the cell's own (empty) galley, so every column after it shifted left
    /// and the header no longer sat over the body.
    #[test]
    fn an_empty_header_cell_does_not_shift_the_columns() {
        const MESSAGE: &str = "\
|  | left | right |
|---|---|---|
| a | b | c |
";
        let columns = drawn_columns(MESSAGE, 520.0);
        // Row 0 is the header, row 1 the body. The header's first cell is
        // empty, so it draws no shape and cannot be compared by index.
        let header = &columns[0];
        let body = &columns[1];
        assert_eq!(header.len(), 2, "the empty header cell draws nothing");
        for cell in header {
            assert!(
                body.iter().any(|body| (body.0 - cell.0).abs() < 0.5),
                "header cell at x={} has no body column: {header:?} vs {body:?}",
                cell.0
            );
        }
    }

    /// Draws `message` at `width` and returns each text shape as
    /// `(x, y, width)`, grouped by the row it lands on. Header and body cells
    /// that share a column must share an `x`.
    fn drawn_columns(message: &str, width: f32) -> Vec<Vec<(f32, f32, f32)>> {
        let ctx = egui::Context::default();
        crate::fonts::install(&ctx);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.vertical(|ui| {
                ui.set_max_width(width);
                draw_text(ui, &Palette::dark(), message, (0usize, 0usize));
            });
        });
        output.textures_delta.clear();

        let mut shapes: Vec<(f32, f32, f32)> = Vec::new();
        fn walk(shape: &egui::epaint::Shape, out: &mut Vec<(f32, f32, f32)>) {
            match shape {
                egui::epaint::Shape::Text(text) => {
                    out.push((text.pos.x, text.pos.y, text.galley.size().x));
                }
                egui::epaint::Shape::Vec(v) => {
                    for shape in v {
                        walk(shape, out);
                    }
                }
                _ => {}
            }
        }
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut shapes);
        }

        // Rows are a few pixels apart; two shapes within 12 px of each other in
        // `y` are on the same line.
        shapes.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let mut rows: Vec<Vec<(f32, f32, f32)>> = Vec::new();
        for shape in shapes {
            match rows.last_mut() {
                Some(row) if (row[0].1 - shape.1).abs() < 12.0 => row.push(shape),
                _ => rows.push(vec![shape]),
            }
        }
        for row in &mut rows {
            row.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        }
        rows
    }

    /// The only test here that draws, and it is here for the two failures that
    /// only exist at layout time: a `FontFamily` with nothing behind it panics
    /// on the first glyph lookup, and the sizes of a table and a code slab are
    /// both decided during layout rather than by the parser. One pass through a
    /// real context catches either, for a fraction of a second.
    #[test]
    fn a_message_of_every_block_draws() {
        const MESSAGE: &str = "\
# Heading

Body with **bold**, *italic*, `inline code` and [a link](https://example.com).

- bullet
  - nested
1. first
2. second

> quoted

| left | right |
|:-----|------:|
| a    |     1 |

```rust
fn main() {}
```

---
";

        let ctx = egui::Context::default();
        crate::fonts::install(&ctx);

        // Twice: the second pass is the one where any cached size from the
        // first is in play, which is where a self-referential layout shows up.
        for _ in 0..2 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                // The bubble's own shape: a fixed-width column, drawn top-down.
                ui.vertical(|ui| {
                    ui.set_max_width(400.0);
                    draw_text(ui, &Palette::dark(), MESSAGE, (0usize, 0usize));
                });
            });
            // There is no painter on the other end of this, so the font atlas
            // the first pass uploads has nowhere to go. `clear` is the
            // documented way to say so; dropping them instead panics.
            output.textures_delta.clear();
        }
    }
}
