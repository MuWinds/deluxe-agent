//! How one tool call is drawn.
//!
//! `app::ui` owns the collapsed row and the fold state; everything below the
//! fold lives here, because a panel's shape depends on which tool produced it —
//! a patch is a diff, an `exec` is a terminal, a file read is just text. Keeping
//! that mapping in one module is what stops the window from growing an arm per
//! tool.
//!
//! The body is shared with `markdown`: a fenced code block in a message is the
//! same slab a tool result is, drawn by [`draw_slab`] without the title bar.
//! Only the title bar and the spec that feeds it belong to tool calls.
//!
//! Two deliberate departures from the Codex reference this is modelled on:
//!
//! * a patch's line numbers are relative, not absolute. OpenAI's patch format
//!   carries no line ranges in its `@@` markers, so [`patch_lines`] counts from
//!   the top of each file section instead — true for a fresh file, honest about
//!   being positional for an edit.
//! * a diff line's tint covers the text it holds rather than the full row. That
//!   is the price of drawing the body as one selectable `LayoutJob` instead of
//!   hand-painting a rect per row, and selectable output is worth more than a
//!   flush right edge on a short line.

use std::fmt::Debug;
use std::hash::Hash;

use eframe::egui;
use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align, Button, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, ScrollArea,
    Stroke, TextFormat, Vec2,
};

use crate::icons;
use crate::theme::{self, Palette};

/// A body taller than this scrolls instead of pushing the transcript down.
const PANEL_MAX_HEIGHT: f32 = 320.0;
const PANEL_RADIUS: u8 = 10;
const CODE_SIZE: f32 = theme::font(12.0);
const TITLE_SIZE: f32 = theme::font(12.0);
/// Padding between the body's text and the panel's frame.
const BODY_PAD_X: i8 = 10;
const BODY_PAD_Y: i8 = 8;
/// The two-column marker that stands in for a gutter.
const MARKER_WIDTH: &str = "  ";

/// What a line of a panel means, which is all the drawing code needs to pick a
/// colour and a marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Unchanged context, or plain output.
    Plain,
    /// A line the call introduced.
    Add,
    /// A line the call removed.
    Del,
    /// A patch hunk boundary.
    Hunk,
    /// Structural text: a file header inside a patch, or a shell prompt.
    Meta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
    /// The line's position in the file it edits, one-based, for a diff body.
    ///
    /// Context and removed lines advance the count; an added line sits *between*
    /// counted lines, so it shows the number of the line it precedes without
    /// moving the count. `None` for scaffolding — file headers, `@@` markers,
    /// plain output — which has no position to show.
    pub num: Option<usize>,
}

impl Line {
    /// A line with no file position — a header, a marker or a plain output line.
    fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            num: None,
        }
    }

    /// A line carrying the real file number it resolved to.
    fn numbered(kind: LineKind, text: impl Into<String>, num: usize) -> Self {
        Self {
            kind,
            text: text.into(),
            num: Some(num),
        }
    }
}

/// The verb a collapsed row shows, which reads better than the tool's own name.
///
/// An unknown tool falls back to its name rather than a generic word, so a
/// tool this build has never heard of is still identifiable.
pub fn tool_label(name: &str) -> &str {
    match name {
        "apply_patch" => "编辑了文件",
        "exec" => "运行了命令",
        "read_file" => "读取了文件",
        "list_dir" => "列出了目录",
        "read_image" => "查看了图片",
        "job_output" => "读取了后台任务",
        "job_list" => "列出了后台任务",
        "job_kill" => "停止了后台任务",
        other => other,
    }
}

/// Whether this build knows how to shape the call's body.
///
/// An unknown tool still gets a panel — its output is all there is to show —
/// but the caller also keeps the raw arguments behind a fold, because nothing
/// else on screen explains what the call was asked to do.
pub fn is_known(name: &str) -> bool {
    matches!(
        name,
        "apply_patch"
            | "exec"
            | "read_file"
            | "list_dir"
            | "read_image"
            | "job_output"
            | "job_list"
            | "job_kill"
    )
}

/// The glyph that heads a call's row and its panel title bar.
pub fn tool_icon(name: &str) -> &'static str {
    match name {
        "apply_patch" => icons::NOTE_PENCIL,
        "exec" => icons::TERMINAL_WINDOW,
        "read_file" => icons::MAGNIFYING_GLASS,
        "list_dir" => icons::FOLDER_SIMPLE,
        "read_image" => icons::IMAGE,
        "job_output" => icons::TERMINAL_WINDOW,
        "job_list" => icons::DOTS_THREE,
        "job_kill" => icons::STOP_CIRCLE,
        _ => icons::GEAR,
    }
}

/// Splits a `*** Begin Patch` document into typed lines.
///
/// The envelope's own `*** Begin Patch` / `*** End Patch` markers are dropped —
/// they are noise. The per-file markers stay, as `Meta`, because they are what
/// tells the reader which file a hunk belongs to.
///
/// `numbers` is the real file line numbers the patch's hunks resolved to at
/// execution time — the tool reads the target before applying anything, and
/// that is the only moment the bare `@@` markers can be given positions. It is
/// one flat table for the whole patch, in the order the hunk body lines appear;
/// the section headers do not consume an entry. When it is missing (a session
/// recorded before the table existed), each file section falls back to counting
/// its own lines from one, which is a guess the header never claims.
pub fn patch_lines(patch: &str, numbers: Option<&[Option<usize>]>) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line_no = 0usize;
    // Where the next inserted line will land, in the same terms the
    // execution-time table uses. Only the no-table fallback consumes it, but it
    // is kept in step either way so a partial table degrades cleanly.
    let mut next_add = 1usize;
    // The next entry in the precomputed table, if one was supplied. Only hunk
    // body lines draw from it, and they draw in order.
    let mut computed = numbers.into_iter().flatten();
    for raw in patch.lines() {
        if raw == "*** Begin Patch" || raw == "*** End Patch" {
            continue;
        }
        if raw.starts_with("*** ") {
            // A new file section restarts the count. `*** Move to:` belongs to
            // the section it sits in and must not reset it.
            if raw.starts_with("*** Add File: ") || raw.starts_with("*** Update File: ") {
                line_no = 0;
                next_add = 1;
            }
            lines.push(Line::new(LineKind::Meta, raw));
            continue;
        }
        if raw.starts_with("@@") {
            // Kept verbatim: the text after `@@` is a locator hint, and the
            // marker itself is bare, so there is nothing to strip.
            lines.push(Line::new(LineKind::Hunk, raw));
            continue;
        }
        // A hunk body line's first character is its marker. The first byte is
        // ASCII whenever the match fires, so the byte index is a char boundary.
        let (kind, text) = match raw.as_bytes().first() {
            Some(b'+') => (LineKind::Add, &raw[1..]),
            Some(b'-') => (LineKind::Del, &raw[1..]),
            Some(b' ') => (LineKind::Plain, &raw[1..]),
            // A blank separator between hunks, or a patch that never marked a
            // line: show it as-is rather than dropping it.
            _ => (LineKind::Plain, raw),
        };
        // A removed line exists at this position; an added line replaces the
        // one just counted, so a `-a` / `+b` pair shows one number — the
        // position the pair rewrites — rather than two.
        match raw.as_bytes().first() {
            Some(b'+') | Some(b'-') | Some(b' ') => {
                // The execution-time table wins over any local counting; the
                // fallback's number is still shown when the table ran dry,
                // because a partial table would be worse than a stale one.
                let num = match computed.next().copied().flatten() {
                    Some(num) => {
                        line_no = num;
                        next_add = match raw.as_bytes().first() {
                            Some(b'-') => num,
                            _ => num + 1,
                        };
                        num
                    }
                    None => match raw.as_bytes().first() {
                        // The fallback mirrors `hunk_line_numbers`' model: a
                        // context line counts and reopens the insertion point
                        // after itself, a removed line counts and leaves the
                        // slot it vacated open, and an add takes that slot and
                        // numbers on from there.
                        Some(b'+') => {
                            let num = next_add;
                            next_add = num + 1;
                            num
                        }
                        _ => {
                            line_no += 1;
                            let num = line_no;
                            next_add = match raw.as_bytes().first() {
                                // A deletion leaves its slot open for the add
                                // that replaces it; a context line closes the
                                // slot behind itself.
                                Some(b'-') => num,
                                _ => num + 1,
                            };
                            num
                        }
                    },
                };
                lines.push(Line::numbered(kind, text, num));
            }
            // An unmarked line is a blank separator between hunks. The
            // execution-time table spent a `None` on it, so the cursor must
            // advance here too or every later number would be off by one.
            _ => {
                computed.next();
                lines.push(Line::new(kind, text));
            }
        }
    }
    lines
}

/// How many lines the patch adds and removes, for the title bar's `+n -m`.
pub fn patch_stats(patch: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in patch_lines(patch, None) {
        match line.kind {
            LineKind::Add => added += 1,
            LineKind::Del => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

/// What a patch touches, for the title bar.
///
/// One file is named outright. Several collapse to a count, because the bar is
/// one line and a list of paths would push the stats and the copy button off
/// the end of it.
pub fn patch_title(patch: &str) -> String {
    let mut files: Vec<&str> = Vec::new();
    for raw in patch.lines() {
        let path = raw
            .strip_prefix("*** Add File: ")
            .or_else(|| raw.strip_prefix("*** Update File: "))
            .or_else(|| raw.strip_prefix("*** Delete File: "));
        if let Some(path) = path {
            let path = path.trim();
            // A file named twice is still one file. `*** Move to:` is
            // deliberately not a candidate: the hunk belongs to its source.
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    match files.as_slice() {
        [] => "补丁".to_string(),
        [only] => (*only).to_string(),
        many => format!("{} 个文件", many.len()),
    }
}

pub fn text_lines(text: &str) -> Vec<Line> {
    text.lines()
        .map(|line| Line::new(LineKind::Plain, line))
        .collect()
}

/// Text that is part of the panel's scaffolding rather than its payload — a
/// tool's own report of what it did, for instance. Drawn muted, like a file
/// header inside a patch.
pub fn meta_lines(text: &str) -> Vec<Line> {
    text.lines()
        .map(|line| Line::new(LineKind::Meta, line))
        .collect()
}

/// An `exec` panel: the command as a prompt line, then whatever it printed.
///
/// The command is repeated here even though the collapsed row summarises it,
/// because the row is truncated and a panel you have to expand is exactly where
/// you want the whole thing.
pub fn command_lines(command: &str, output: &str) -> Vec<Line> {
    let mut lines = vec![Line::new(LineKind::Meta, format!("$ {command}"))];
    lines.extend(text_lines(output));
    lines
}

/// Everything the panel draws, assembled by the caller.
pub struct PanelSpec {
    /// Shown in the title bar: a file path, a directory, or `Shell`.
    ///
    /// Owned rather than borrowed because the patch case derives it (a file
    /// name, or a count of them) instead of having one to hand.
    pub title: String,
    pub icon: &'static str,
    /// `+n -m`. Both zero hides the pair, which is what every non-patch wants.
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<Line>,
    /// What the copy button puts on the clipboard — the unadorned original, not
    /// the panel's rendering of it.
    pub copy: String,
    /// The status colour, carried over from the collapsed row so the icon still
    /// says how the call ended.
    pub accent: Color32,
    pub running: bool,
}

/// Draws the panel below an expanded tool row.
///
/// `call_id` salts the body's scroll area, so two open panels scroll
/// independently instead of sharing one offset.
pub fn draw(ui: &mut egui::Ui, p: &Palette, call_id: &str, spec: &PanelSpec, width: f32) {
    Frame::NONE
        .fill(p.code_bg)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(PANEL_RADIUS))
        .inner_margin(Margin::ZERO)
        .show(ui, |ui| {
            // The panel is a fixed slab: without a floor it would shrink to the
            // width of its longest line, and every panel would be a different
            // size.
            ui.set_min_width(width);
            ui.set_max_width(width);
            // Flush, so the hairline under the title bar sits exactly on the
            // boundary rather than floating in a gap.
            ui.spacing_mut().item_spacing.y = 0.0;

            draw_title_bar(ui, p, spec);
            draw_body(ui, p, call_id, spec);
        });
}

fn draw_title_bar(ui: &mut egui::Ui, p: &Palette, spec: &PanelSpec) {
    let bar = Frame::NONE
        .fill(p.code_header_bg)
        .corner_radius(CornerRadius {
            nw: PANEL_RADIUS,
            ne: PANEL_RADIUS,
            sw: 0,
            se: 0,
        })
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            // A frame sizes itself to its content, so the bar has to be told to
            // span the panel: otherwise its fill and the rule below it would
            // stop wherever the title happened to end.
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(spec.icon).size(TITLE_SIZE).color(spec.accent));
                ui.label(RichText::new(&spec.title).size(TITLE_SIZE).color(p.text));

                // Each count stands on its own: a patch that only deletes
                // should not advertise `+0`.
                if spec.added > 0 {
                    ui.label(
                        RichText::new(format!("+{}", spec.added))
                            .size(theme::font(11.0))
                            .color(p.diff_add_fg)
                            .monospace(),
                    );
                }
                if spec.removed > 0 {
                    ui.label(
                        RichText::new(format!("-{}", spec.removed))
                            .size(theme::font(11.0))
                            .color(p.diff_del_fg)
                            .monospace(),
                    );
                }

                // Right-to-left, so the copy button is pinned to the far edge
                // however long the title turns out to be.
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            Button::new(
                                RichText::new(icons::COPY)
                                    .size(TITLE_SIZE)
                                    .color(p.text_muted),
                            )
                            .frame(false),
                        )
                        .on_hover_text("复制到剪贴板")
                        .clicked()
                    {
                        ui.ctx().copy_text(spec.copy.clone());
                    }
                });
            });
        });

    let rect = bar.response.rect;
    ui.painter()
        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, p.border));
}

fn draw_body(ui: &mut egui::Ui, p: &Palette, call_id: &str, spec: &PanelSpec) {
    if spec.running {
        Frame::NONE
            .inner_margin(Margin::symmetric(BODY_PAD_X, BODY_PAD_Y))
            .show(ui, |ui| {
                ui.label(RichText::new("运行中…").size(CODE_SIZE).color(p.text_muted));
            });
        return;
    }

    draw_body_lines(ui, p, call_id, &spec.lines);
}

/// A slab of lines with no title bar, for a body that is not a tool call.
///
/// This is `draw_body`'s payload without the `PanelSpec` around it, so a
/// Markdown fence can be the same slab a tool result is without inventing a
/// title and a copy button it has no use for. `salt` names the body's scroll
/// area; two slabs in one bubble must not share one, or they share an offset.
pub fn draw_slab(
    ui: &mut egui::Ui,
    p: &Palette,
    salt: impl Hash + Debug,
    lines: &[Line],
    width: f32,
) {
    Frame::NONE
        .fill(p.code_bg)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(PANEL_RADIUS))
        .inner_margin(Margin::ZERO)
        .show(ui, |ui| {
            ui.set_min_width(width);
            ui.set_max_width(width);
            draw_body_lines(ui, p, salt, lines);
        });
}

/// The body itself: one galley, in a scroll area, at a height measured from the
/// galley rather than from the room left in the transcript.
fn draw_body_lines(ui: &mut egui::Ui, p: &Palette, salt: impl Hash + Debug, lines: &[Line]) {
    let inset = Margin::symmetric(BODY_PAD_X, BODY_PAD_Y);

    // The galley is laid out *before* the panel's height is chosen, because a
    // `ScrollArea` sizes itself from the room left in the transcript viewport:
    // left to choose, the same panel would come out a different height
    // depending on where it sat in the scroll, and would be squeezed to almost
    // nothing when expanded below the fold. Measuring first pins the height to
    // the content, capped.
    let galley = ui.painter().layout_job(body_job(p, lines));
    let pad = 2.0 * BODY_PAD_Y as f32;
    // A horizontal scrollbar is allocated *inside* the box, so a body too wide
    // for the panel has to pay for its own bar — otherwise the missing strip
    // would also push the content into a vertical scroll.
    let bar = if galley.size().x + 2.0 * BODY_PAD_X as f32 > ui.available_width() {
        ui.spacing().scroll.allocated_width()
    } else {
        0.0
    };
    let height = (galley.size().y + pad + bar).min(PANEL_MAX_HEIGHT + bar);

    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), height),
        Layout::top_down(Align::Min),
        |ui| {
            ScrollArea::both()
                .id_salt(salt)
                // Both axes filled, so the slab is exactly the size just
                // computed rather than re-deriving it from the content.
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Inset so the first line is not glued to the title bar's
                    // rule.
                    Frame::NONE.inner_margin(inset).show(ui, |ui| {
                        ui.add(egui::Label::new(galley).selectable(true));
                    });
                });
        },
    );
}

/// The panel body as one galley, so the whole thing is a single selectable
/// run and egui can cache the layout between frames.
fn body_job(p: &Palette, lines: &[Line]) -> LayoutJob {
    let font = FontId::monospace(CODE_SIZE);
    // The wrap is set here rather than through a `Label`, because this job is
    // laid out by hand: lines stay as long as they are and the panel scrolls
    // sideways to reach the rest.
    let mut job = LayoutJob {
        wrap: TextWrapping::no_max_width(),
        ..Default::default()
    };
    let newline = TextFormat {
        font_id: font.clone(),
        color: p.text,
        ..Default::default()
    };

    for line in lines {
        let (marker, colour, background) = match line.kind {
            LineKind::Add => ("+ ", p.diff_add_fg, p.diff_add_bg),
            LineKind::Del => ("- ", p.diff_del_fg, p.diff_del_bg),
            LineKind::Hunk => (MARKER_WIDTH, p.text_muted, Color32::TRANSPARENT),
            LineKind::Meta => (MARKER_WIDTH, p.text_muted, Color32::TRANSPARENT),
            LineKind::Plain => (MARKER_WIDTH, p.text, Color32::TRANSPARENT),
        };
        let format = TextFormat {
            font_id: font.clone(),
            color: colour,
            background,
            ..Default::default()
        };
        // The gutter is part of the galley rather than painted beside it, so a
        // numbered line carries its number under its own muted format — the
        // row's tint must not claim it.
        if let Some(num) = line.num {
            job.append(&format!("{num:>4}  "), 0.0, gutter_format(p, &font));
        }
        job.append(marker, 0.0, format.clone());
        job.append(&line.text, 0.0, format);
        // Appended under its own format so a tinted row does not drag its
        // terminator along with it.
        job.append("\n", 0.0, newline.clone());
    }

    job
}

/// The format of a gutter number: the body's font in the muted ink, so it reads
/// as scaffolding rather than as content.
fn gutter_format(p: &Palette, font: &FontId) -> TextFormat {
    TextFormat {
        font_id: font.clone(),
        color: p.text_muted,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
*** Begin Patch
*** Update File: src/agent.rs
@@
 history:
-old
+new
*** Add File: src/new.rs
+fn main() {}
*** Delete File: src/gone.rs
*** End Patch";

    #[test]
    fn patch_lines_drops_the_envelope_and_keeps_the_file_headers() {
        let lines = patch_lines(PATCH, None);
        assert!(!lines.iter().any(|line| line.text.contains("Begin Patch")));
        assert!(!lines.iter().any(|line| line.text.contains("End Patch")));
        assert!(lines.iter().any(
            |line| line.kind == LineKind::Meta && line.text == "*** Update File: src/agent.rs"
        ));
    }

    #[test]
    fn patch_lines_strips_the_hunk_markers() {
        let lines = patch_lines(PATCH, None);
        let add = lines
            .iter()
            .find(|line| line.text == "new")
            .expect("the added line");
        assert_eq!(add.kind, LineKind::Add);
        let del = lines
            .iter()
            .find(|line| line.text == "old")
            .expect("the removed line");
        assert_eq!(del.kind, LineKind::Del);
        // The context line's leading space is a marker too, and must go.
        let context = lines
            .iter()
            .find(|line| line.text == "history:")
            .expect("the context line");
        assert_eq!(context.kind, LineKind::Plain);
        assert!(lines.iter().any(|line| line.kind == LineKind::Hunk));
    }

    #[test]
    fn a_blank_line_is_kept_rather_than_treated_as_a_marker() {
        let lines = patch_lines(
            "*** Begin Patch\n*** Update File: a\n@@\n\n+x\n*** End Patch",
            None,
        );
        assert!(lines.iter().any(|line| line.text.is_empty()));
    }

    #[test]
    fn patch_lines_numbers_context_and_del_lines_from_one_per_file() {
        let lines = patch_lines(PATCH, None);
        let context = lines
            .iter()
            .find(|line| line.text == "history:")
            .expect("the context line");
        assert_eq!(context.num, Some(1));
        let del = lines
            .iter()
            .find(|line| line.text == "old")
            .expect("the removed line");
        assert_eq!(del.num, Some(2));
    }

    #[test]
    fn an_added_line_takes_the_number_it_sits_at_without_moving_the_count() {
        let lines = patch_lines(PATCH, None);
        let add = lines
            .iter()
            .find(|line| line.text == "new")
            .expect("the added line");
        // It replaces the removed line at position 2, so it carries 2 — and the
        // next section's count is not pushed on by it.
        assert_eq!(add.num, Some(2));
    }

    #[test]
    fn a_new_file_section_restarts_the_count() {
        let lines = patch_lines(PATCH, None);
        let added_file_line = lines
            .iter()
            .find(|line| line.text == "fn main() {}")
            .expect("the added file's line");
        assert_eq!(added_file_line.num, Some(1));
    }

    #[test]
    fn scaffolding_carries_no_number() {
        let lines = patch_lines(PATCH, None);
        let header = lines
            .iter()
            .find(|line| line.kind == LineKind::Meta)
            .expect("a header");
        assert_eq!(header.num, None);
        assert!(lines
            .iter()
            .filter(|line| line.kind == LineKind::Hunk)
            .all(|line| line.num.is_none()));
    }

    #[test]
    fn patch_stats_counts_adds_and_dels() {
        // One `+` in the update, one in the added file; one `-`.
        assert_eq!(patch_stats(PATCH), (2, 1));
    }

    #[test]
    fn patch_title_names_one_file() {
        let patch = "*** Begin Patch\n*** Update File: src/agent.rs\n@@\n-a\n+b\n*** End Patch";
        assert_eq!(patch_title(patch), "src/agent.rs");
    }

    #[test]
    fn patch_title_counts_several_and_ignores_repeats() {
        assert_eq!(patch_title(PATCH), "3 个文件");
        let twice = "*** Begin Patch\n*** Update File: a\n@@\n-a\n+b\n*** Update File: a\n@@\n-c\n+d\n*** End Patch";
        assert_eq!(patch_title(twice), "a");
    }

    #[test]
    fn patch_title_survives_a_patch_it_cannot_read() {
        assert_eq!(patch_title("not a patch"), "补丁");
    }

    #[test]
    fn a_move_is_not_a_second_file() {
        let patch =
            "*** Begin Patch\n*** Update File: a\n*** Move to: b\n@@\n-x\n+y\n*** End Patch";
        assert_eq!(patch_title(patch), "a");
    }

    #[test]
    fn the_execution_time_table_supplies_real_file_numbers() {
        // A hunk matched at line 40: context is 40, the removal 41, the two
        // inserted lines take 41 and 42.
        let patch =
            "*** Begin Patch\n*** Update File: a\n@@\n ctx\n-old\n+new\n+newer\n*** End Patch";
        let lines = patch_lines(patch, Some(&[Some(40), Some(41), Some(41), Some(42)]));
        let numbered: Vec<Option<usize>> = lines
            .iter()
            .filter(|line| !matches!(line.kind, LineKind::Meta | LineKind::Hunk))
            .map(|line| line.num)
            .collect();
        assert_eq!(numbered, vec![Some(40), Some(41), Some(41), Some(42)]);
    }

    #[test]
    fn a_table_shorter_than_the_patch_falls_back_to_counting() {
        // The table covers the first line only; the rest are counted from it.
        let patch = "*** Begin Patch\n*** Update File: a\n@@\n ctx\n-old\n+new\n*** End Patch";
        let lines = patch_lines(patch, Some(&[Some(30)]));
        let nums: Vec<Option<usize>> = lines
            .iter()
            .filter(|line| !matches!(line.kind, LineKind::Meta | LineKind::Hunk))
            .map(|line| line.num)
            .collect();
        assert_eq!(nums, vec![Some(30), Some(31), Some(31)]);
    }

    #[test]
    fn without_a_table_the_sections_count_from_one() {
        let lines = patch_lines(PATCH, None);
        let del = lines
            .iter()
            .find(|line| line.text == "old")
            .expect("the removed line");
        // Second line of the patch's first hunk.
        assert_eq!(del.num, Some(2));
    }

    #[test]
    fn command_lines_lead_with_the_prompt() {
        let lines = command_lines("ls -la", "total 0\n");
        assert_eq!(lines[0].kind, LineKind::Meta);
        assert_eq!(lines[0].text, "$ ls -la");
        assert_eq!(lines[1].text, "total 0");
    }

    #[test]
    fn text_lines_keeps_every_line_including_blanks() {
        let lines = text_lines("a\n\nb");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| line.kind == LineKind::Plain));
    }

    #[test]
    fn every_registered_tool_is_translated() {
        // The registry in `tools::ToolRegistry` — a tool added there without a
        // label here would silently fall back to its raw name.
        for name in ["read_file", "list_dir", "exec", "apply_patch", "read_image"] {
            assert_ne!(tool_label(name), name, "{name} has no label");
            assert!(!tool_icon(name).is_empty(), "{name} has no icon");
        }
    }

    #[test]
    fn an_unknown_tool_still_draws() {
        assert_eq!(tool_label("brand_new_tool"), "brand_new_tool");
        assert!(!tool_icon("brand_new_tool").is_empty());
    }
}
