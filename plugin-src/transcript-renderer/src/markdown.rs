//! Markdown → display list.
//!
//! The parser turns text into blocks; [`to_display`] then shapes those blocks
//! into the generic presentation nodes the host draws. All the visual decisions
//! — heading sizes, list indents, quote bars, code slabs, and the grid/record/
//! source choice for a table — are made here, so the host never learns what
//! Markdown is.
//!
//! Deliberate divergences from CommonMark:
//!
//! * no setext headings, indented code blocks, HTML, or images;
//! * no reference links, and no autolinking of bare URLs — only `[text](url)`
//!   and `<url>` are links;
//! * a hard break is a trailing backslash only;
//! * list depth comes from a fixed two-space indent;
//! * an emphasis run is matched against the next run of the same delimiter *at
//!   least* as long;
//! * a code span keeps its interior spaces;
//! * a table is recognised only when the delimiter row is the very next line.

use serde::Serialize;

use crate::display::{
    Border, ColorRole, CornerSpec, EdgeInsets, GridColumn, HorizontalAlign, Node, Run,
};
use crate::protocol::Metrics;

/// How one run of text is decorated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
}

/// One inline run: its text, how it is decorated, and where it links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
    /// `null` when the span is not a link. Always emitted, never skipped, so the
    /// host decoder sees a stable shape.
    pub link: Option<String>,
}

impl Span {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: SpanStyle::default(),
            link: None,
        }
    }
}

/// A list item's leading token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Marker {
    Bullet,
    /// Carries the number the source wrote, so `3.` does not silently become
    /// `1.`.
    Ordered { value: u64 },
}

/// A table column's alignment, from its delimiter row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ColumnAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// One block of a message body, tagged for the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Block {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph {
        spans: Vec<Span>,
    },
    /// Flat, with a depth, rather than a nested `List`.
    Item {
        depth: usize,
        marker: Marker,
        spans: Vec<Span>,
    },
    /// A fence that was never closed keeps everything to the end of the text.
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
        /// The table as it was written, for the narrow-bubble fallback. Renamed
        /// `source` on the wire.
        #[serde(rename = "source")]
        text: String,
    },
}

/// Parses a message body into blocks.
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

        // The order is load-bearing: a fence and a rule both start with a
        // character a list item could start with, and the paragraph arm has to
        // come last so it cannot swallow a table's header row.
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
/// character.
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

    if cells.first().is_some_and(|cell| cell.trim().is_empty()) {
        cells.remove(0);
    }
    if cells.last().is_some_and(|cell| cell.trim().is_empty()) {
        cells.pop();
    }
    cells
}

/// The header row at `start` and every following row, as a `Block::Table`.
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
            (
                Marker::Ordered {
                    value: trimmed[..digits].parse().ok()?,
                },
                rest,
            )
        }
        _ => return None,
    };

    Some((indent / 2, marker, rest.strip_prefix(' ')?))
}

/// An item's spans, plus any continuation lines that belong to it.
fn item_spans(lines: &[&str], start: usize, first: &str) -> (Vec<Span>, usize) {
    let (body, mut hard) = split_break(first);
    let mut text = body.to_string();
    let mut i = start + 1;

    while i < lines.len() {
        if lines[i].trim().is_empty() || starts_block(lines, i) {
            break;
        }
        let (body, next_hard) = split_break(lines[i]);
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
fn parse_inline(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    while i < text.len() {
        let rest = &text[i..];
        let ch = rest.chars().next().expect("i is a char boundary");

        match ch {
            '\\' => {
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
    let run = text[i..].chars().take_while(|c| *c == ch).count().min(3);

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

// ---------------------------------------------------------------------------
// Display-list shaping
// ---------------------------------------------------------------------------

/// Body text size, in design units.
const BODY_SIZE: f32 = 14.0;
/// Table text size.
const TABLE_SIZE: f32 = 13.0;
/// The narrowest a table column may shrink to.
const TABLE_MIN_COLUMN: f32 = 24.0;
/// The widest a table column may grow to.
const TABLE_CELL_CAP: f32 = 320.0;
/// A token at least this many characters long makes a column `TokenHeavy`.
const TABLE_LONG_TOKEN: usize = 20;
/// Average words per filled cell that makes a column `Narrative`.
const TABLE_PROSE_WORDS: f32 = 4.0;
/// The shrink loop's iteration cap.
const TABLE_SHRINK_STEPS: usize = 512;
/// The space above and below a table's rules.
const TABLE_V_PAD: f32 = 4.0;
/// One list level's indent.
const LIST_INDENT: f32 = 16.0;
/// The width of a list item's marker column.
const LIST_MARKER: f32 = 22.0;
/// A quote's left inset, in front of its bar.
const QUOTE_INDENT: f32 = 12.0;
/// The tallest a code slab or panel body grows before it scrolls.
const SLAB_MAX_HEIGHT: f32 = 320.0;

/// Shapes blocks into the display list the host draws.
pub fn to_display(blocks: &[Block], metrics: &Metrics) -> Vec<Node> {
    blocks_to_nodes(blocks, metrics)
}

fn blocks_to_nodes(blocks: &[Block], metrics: &Metrics) -> Vec<Node> {
    let mut nodes = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            nodes.push(Node::spacer(block_gap(block)));
        }
        nodes.extend(block_to_nodes(block, metrics));
    }
    nodes
}

/// The space above a block.
fn block_gap(block: &Block) -> f32 {
    match block {
        Block::Heading { level, .. } if *level <= 2 => 10.0,
        Block::Heading { .. } => 7.0,
        _ => 6.0,
    }
}

/// A heading's size. The top three levels are visibly larger; 4 to 6 are body
/// size, because a chat bubble is narrow and six sizes do not fit in it.
fn heading_size(level: u8) -> f32 {
    match level {
        1 => 19.0,
        2 => 17.0,
        3 => 15.5,
        _ => BODY_SIZE,
    }
}

fn block_to_nodes(block: &Block, metrics: &Metrics) -> Vec<Node> {
    match block {
        Block::Heading { level, spans } => {
            vec![Node::text(spans_to_runs(spans, heading_size(*level), ColorRole::Text))]
        }
        Block::Paragraph { spans } => {
            vec![Node::text(spans_to_runs(spans, BODY_SIZE, ColorRole::Text))]
        }
        Block::Item {
            depth,
            marker,
            spans,
        } => {
            let token = match marker {
                Marker::Bullet => "•".to_string(),
                Marker::Ordered { value } => format!("{value}."),
            };
            let marker_box = Node::Align {
                align: HorizontalAlign::Right,
                width: Some(LIST_MARKER),
                child: Box::new(Node::text(vec![Run::text(token, BODY_SIZE, ColorRole::Muted)])),
            };
            vec![Node::row(
                vec![
                    Node::Indent {
                        amount: *depth as f32 * LIST_INDENT,
                        child: Box::new(marker_box),
                    },
                    Node::text(spans_to_runs(spans, BODY_SIZE, ColorRole::Text)),
                ],
                8.0,
            )]
        }
        Block::Code { lang, text } => {
            let mut nodes = Vec::new();
            if !lang.is_empty() {
                nodes.push(Node::text(vec![Run::text(
                    lang.clone(),
                    11.0,
                    ColorRole::Muted,
                )]));
                nodes.push(Node::spacer(2.0));
            }
            nodes.push(code_slab(text));
            nodes
        }
        Block::Quote { blocks } => vec![Node::Frame {
            fill: None,
            stroke: None,
            left_bar: Some(Border {
                width: 3.0,
                color: ColorRole::Muted,
            }),
            radius: CornerSpec::same(0.0),
            padding: EdgeInsets {
                left: QUOTE_INDENT,
                right: 0.0,
                top: 2.0,
                bottom: 2.0,
            },
            stretch: false,
            child: Box::new(Node::column(blocks_to_nodes(blocks, metrics))),
        }],
        Block::Rule => vec![Node::column(vec![
            Node::spacer(3.0),
            Node::Rule,
            Node::spacer(3.0),
        ])],
        Block::Table {
            align,
            header,
            rows,
            text,
        } => vec![table_node(align, header, rows, text, metrics)],
    }
}

/// A title-less code slab, the same shape a tool panel body uses.
fn code_slab(text: &str) -> Node {
    let mut runs = Vec::new();
    for line in text.lines() {
        // The two-space gutter marker is what a panel body gives a plain line.
        runs.push(Run::mono(format!("  {line}"), 12.0, ColorRole::Text));
        runs.push(Run::mono("\n", 12.0, ColorRole::Text));
    }
    Node::Frame {
        fill: Some(ColorRole::CodeBg),
        stroke: Some(ColorRole::Border),
        left_bar: None,
        radius: CornerSpec::same(10.0),
        padding: EdgeInsets::zero(),
        stretch: true,
        child: Box::new(Node::Scroll {
            max_height: SLAB_MAX_HEIGHT,
            both_axes: true,
            child: Box::new(Node::Frame {
                fill: None,
                stroke: None,
                left_bar: None,
                radius: CornerSpec::same(0.0),
                padding: EdgeInsets::symmetric(10.0, 8.0),
                stretch: false,
                child: Box::new(Node::code(runs)),
            }),
        }),
    }
}

/// One run per inline span, at the block's size.
fn spans_to_runs(spans: &[Span], size: f32, colour: ColorRole) -> Vec<Run> {
    spans
        .iter()
        .map(|span| {
            let (font, run_size) = if span.style.code {
                (true, size * 0.92)
            } else {
                (false, size)
            };
            let color = if span.link.is_some() {
                ColorRole::Accent
            } else {
                colour
            };
            let mut run = if font {
                Run::mono(span.text.clone(), run_size, color)
            } else {
                Run::text(span.text.clone(), run_size, color)
            };
            if span.style.italic {
                run = run.italic();
            }
            if span.style.code {
                run = run.tinted(ColorRole::CodeBg);
            }
            if let Some(url) = &span.link {
                run = run.underlined().linked(url.clone());
            }
            run
        })
        .collect()
}

/// How a table column reads, which decides what it gives up first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    TokenHeavy,
    Narrative,
    Compact,
}

struct ColumnMetric {
    max: f32,
    token: f32,
    kind: ColumnKind,
}

/// A rough width for `text`, from the host's average character advance.
///
/// The renderer has no fonts, so it cannot measure. The host passes the advance
/// of one average character; every glyph is charged at that rate, which is the
/// conservative case the host's own fallback uses.
fn estimate(text: &str, char_width: f32) -> f32 {
    text.chars().count() as f32 * char_width
}

fn cell_text(cell: &[Span]) -> String {
    cell.iter().map(|span| span.text.as_str()).collect()
}

fn table_node(
    align: &[ColumnAlign],
    header: &[Vec<Span>],
    rows: &[Vec<Vec<Span>>],
    source: &str,
    metrics: &Metrics,
) -> Node {
    let columns = align.len();
    if columns == 0 {
        return Node::text(vec![]);
    }

    let metric = measure_columns(header, rows, columns, metrics.char_width);
    let gap_total = metrics.column_gap * (columns - 1) as f32;
    let available = (metrics.available_width - gap_total).max(1.0);

    let Some(widths) = fit_widths(&metric, available) else {
        return source_node(source);
    };
    if should_render_records(rows, &widths, &metric, metrics.char_width) {
        return records_node(header, rows, source, metrics, &widths);
    }

    Node::Grid {
        columns: align
            .iter()
            .zip(&widths)
            .map(|(align, width)| GridColumn {
                align: horizontal(*align),
                width: *width,
            })
            .collect(),
        gap: metrics.column_gap,
        header: header
            .iter()
            .map(|cell| spans_to_runs(cell, TABLE_SIZE, ColorRole::Text))
            .collect(),
        rows: rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| spans_to_runs(cell, TABLE_SIZE, ColorRole::Text))
                    .collect()
            })
            .collect(),
        rules: true,
    }
}

fn horizontal(align: ColumnAlign) -> HorizontalAlign {
    match align {
        ColumnAlign::Left => HorizontalAlign::Left,
        ColumnAlign::Center => HorizontalAlign::Center,
        ColumnAlign::Right => HorizontalAlign::Right,
    }
}

fn measure_columns(
    header: &[Vec<Span>],
    rows: &[Vec<Vec<Span>>],
    columns: usize,
    char_width: f32,
) -> Vec<ColumnMetric> {
    let mut metrics: Vec<ColumnMetric> = (0..columns)
        .map(|_| ColumnMetric {
            max: 0.0,
            token: 0.0,
            kind: ColumnKind::Compact,
        })
        .collect();

    for (column, cell) in header.iter().enumerate().take(columns) {
        let text = cell_text(cell);
        metrics[column].max = metrics[column].max.max(estimate(&text, char_width));
        metrics[column].token = metrics[column]
            .token
            .max(longest_token(&text, char_width));
    }
    for row in rows {
        for (column, cell) in row.iter().enumerate().take(columns) {
            let text = cell_text(cell);
            metrics[column].max = metrics[column].max.max(estimate(&text, char_width));
            metrics[column].token = metrics[column]
                .token
                .max(longest_token(&text, char_width));
        }
    }
    for (column, metric) in metrics.iter_mut().enumerate() {
        metric.kind = classify_column(rows, column);
    }
    metrics
}

fn longest_token(text: &str, char_width: f32) -> f32 {
    text.split_whitespace()
        .map(|token| estimate(token, char_width))
        .fold(0.0f32, f32::max)
}

fn classify_column(rows: &[Vec<Vec<Span>>], column: usize) -> ColumnKind {
    let mut long_tokens = 0usize;
    let mut words = 0usize;
    let mut cells_with_long_token = 0usize;
    let mut filled_cells = 0usize;

    for row in rows {
        let Some(cell) = row.get(column) else {
            continue;
        };
        let text = cell_text(cell);
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

fn minimum(metric: &ColumnMetric) -> f32 {
    match metric.kind {
        ColumnKind::Compact => metric.token.min(TABLE_MIN_COLUMN * 2.0),
        _ => TABLE_MIN_COLUMN,
    }
    .max(TABLE_MIN_COLUMN)
    .min(metric.max.max(TABLE_MIN_COLUMN))
}

fn fit_widths(metrics: &[ColumnMetric], available: f32) -> Option<Vec<f32>> {
    let mut widths: Vec<f32> = metrics
        .iter()
        .map(|column| column.max.clamp(TABLE_MIN_COLUMN, TABLE_CELL_CAP))
        .collect();
    let floors: Vec<f32> = metrics.iter().map(minimum).collect();

    let total: f32 = widths.iter().sum();
    if total <= available {
        return Some(widths);
    }
    if floors.iter().sum::<f32>() > available {
        return None;
    }

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

fn should_render_records(
    rows: &[Vec<Vec<Span>>],
    widths: &[f32],
    metrics: &[ColumnMetric],
    char_width: f32,
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
                    let text = cell_text(cell);
                    let fragment = text
                        .split_whitespace()
                        .any(|word| word.chars().count() as f32 * char_width > *width);
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

fn source_node(source: &str) -> Node {
    Node::text(vec![Run::mono(
        source,
        TABLE_SIZE * 0.92,
        ColorRole::Muted,
    )])
}

fn records_node(
    header: &[Vec<Span>],
    rows: &[Vec<Vec<Span>>],
    source: &str,
    metrics: &Metrics,
    _widths: &[f32],
) -> Node {
    let labels: Vec<String> = header.iter().map(|cell| cell_text(cell)).collect();
    let raw_label_width = labels
        .iter()
        .map(|label| estimate(label, metrics.char_width))
        .fold(0.0f32, f32::max);
    if raw_label_width + metrics.column_gap + TABLE_MIN_COLUMN > metrics.available_width {
        return source_node(source);
    }
    let label_width = raw_label_width.min(metrics.available_width * 0.5);

    let mut children = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let mut group = Vec::new();
        for (label, cell) in labels.iter().zip(row) {
            if cell.iter().all(|span| span.text.is_empty()) {
                continue;
            }
            group.push(Node::row(
                vec![
                    Node::Align {
                        align: HorizontalAlign::Right,
                        width: Some(label_width),
                        child: Box::new(Node::text(vec![Run::text(
                            label.clone(),
                            TABLE_SIZE,
                            ColorRole::Muted,
                        )])),
                    },
                    Node::text(spans_to_runs(cell, TABLE_SIZE, ColorRole::Text)),
                ],
                metrics.column_gap,
            ));
        }
        children.push(Node::column(group));
        if index < rows.len() - 1 {
            children.push(Node::spacer(TABLE_V_PAD));
            children.push(Node::Rule);
            children.push(Node::spacer(TABLE_V_PAD));
        }
    }
    Node::column(children)
}
