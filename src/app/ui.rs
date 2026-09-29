//! The window's egui rendering.
//!
//! Every widget the app draws lives here; the view state it reads and the
//! [`Cmd`]s it sends are defined in the parent module.
//!
//! The layout, outside in — and the order of the `draw_*` calls is load-bearing,
//! because each panel claims its space out of what the previous ones left:
//!
//! Inside the central panel the composer comes *before* the transcript. A bottom
//! panel reserves space by pulling the parent cursor's `max.y` up, and the scroll
//! area sizes itself from what is left; drawn the other way round the two
//! overlap.

use super::*;

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;
use egui::text::LayoutJob;
use egui::{
    Align, Color32, ColorImage, CornerRadius, FontId, Frame, Layout, Margin, Pos2, RichText,
    Stroke, TextFormat, TextureHandle, TextureOptions, Vec2,
};
use serde_json::Value;
use uuid::Uuid;

use crate::attachments::{self, ImageRef};
use crate::code_view;
use crate::config::{self, InputModality};
use crate::icons;
use crate::image_ops;
use crate::ipc::{AuditOutcome, JobState, JobView, RunState};
use crate::llm::ThinkingLevel;
use crate::markdown;
use crate::plugins::{self};
use crate::session::{self, Session, Step, ToolResult};
use crate::theme::{self, Palette, ThemeChoice};

/// Width of the far-left icon rail.
const RAIL_WIDTH: f32 = 52.0;

/// Default width of the session sidebar.
const SIDEBAR_WIDTH: f32 = 300.0;

/// The composer stops growing past this, and is centred in the main area.
///
/// The transcript's message column is the same width, so the two line up: the
/// conversation and the box you type into read as one column rather than as
/// two blocks of different widths stacked on each other.
pub(super) const COMPOSER_MAX_WIDTH: f32 = 820.0;

/// The margin the chat column keeps from the panel's edges, shared by the
/// transcript and the composer so their columns start and end on the same x.
///
/// A column that is merely *centred* is not enough: the transcript used to
/// take a fraction of the panel instead, and on a wide window its left edge
/// landed well inside the composer's — the content looked pushed to the right
/// even though it was symmetric.
pub(super) const CHAT_MARGIN_X: f32 = 16.0;

/// Height of the round buttons in the composer.
const COMPOSER_BUTTON: f32 = 30.0;

/// How tall the slash-command picker grows before it scrolls.
///
/// Roughly six rows: enough that a plugin's whole command list is usually
/// visible at once, short enough that the picker cannot eat the transcript.
const COMMAND_PICKER_MAX_HEIGHT: f32 = 186.0;

/// One picker row's height.
const COMMAND_ROW_HEIGHT: f32 = 26.0;

/// Space between the gauge ring and its percentage.
const GAUGE_RING_GAP: f32 = 7.0;

/// Upper bound of the gauge box — ring, gap and a full "100%" label. Only the
/// editor's width budget spends it; the gauge itself is measured from its
/// label, so its box can never end up too small to hold what it draws.
const CONTEXT_GAUGE_RESERVE: f32 = 64.0;

/// Upper bound of the thinking picker on the composer's input row, spent from
/// the editor's width budget the same way [`CONTEXT_GAUGE_RESERVE`] is. The
/// picker itself sizes to its text; the reserve only keeps the editor from
/// pushing it and the gauge off the right edge.
const THINKING_PICKER_RESERVE: f32 = 84.0;

/// Straight-line segments used to draw the gauge ring.
///
/// At the gauge's ~11 px radius any more is invisible; any fewer and the
/// circle reads as a polygon.
const SEGMENTS_PER_RING: usize = 24;

/// Gap between a right-hugging bubble and the column's right edge, so the
/// bubble does not sit flush against the edge the replies stop at.
pub(super) const BUBBLE_EDGE_GAP: f32 = 14.0;

/// Horizontal padding inside a bubble, between its edge and the text. Named
/// because the measurement and the draw have to agree on it: the measured
/// width includes the two pads, and the drawn bubble is floored at the width
/// they leave for the content.
const BUBBLE_PADDING_X: f32 = 12.0;

/// How close to the bottom counts as "the user is following along".
const STICK_THRESHOLD: f32 = 24.0;

/// What a user-message attachment renders as in the transcript.
const TRANSCRIPT_THUMB: (f32, f32) = (180.0, 130.0);

/// Side of the square an attached image is shown at, in the composer strip and
/// in the transcript.
const OK_GREEN: Color32 = Color32::from_rgb(0x2e, 0xa0, 0x43);

const WARN_AMBER: Color32 = Color32::from_rgb(0xd9, 0x8a, 0x00);

const BAD_RED: Color32 = Color32::from_rgb(0xc0, 0x39, 0x2b);

/// The most job rows the composer draws. Running jobs are always kept; settled
/// ones fill the rest, newest first, so an old job cannot push out a live one.
const MAX_JOB_ROWS: usize = 5;

impl App {
    /// Draws one frame.
    ///
    /// Intake and polling run before the draw pass so this frame reflects the
    /// freshest state, and the deferred [`Actions`] are applied after it, once
    /// the borrows the widgets held have been released.
    pub fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let p = theme::palette(self.config.theme);
        let ctx = ui.ctx().clone();

        // Image intake is a whole-frame concern, not the composer widget's: the
        // chord works wherever the focus is, and a file can be dropped onto any
        // panel. Run before the draw pass so the chips appear this frame.
        self.intake_pasted_images(&ctx);
        self.intake_dropped_files(&ctx);

        // The composer's task list is a poll against the worker, throttled and
        // kept awake only while something is live. Before the draw pass so the
        // list reflects the freshest reply this frame.
        self.poll_jobs(&ctx);

        let mut actions = Actions::default();
        self.draw_menu_bar(ui, &p, &mut actions);
        self.draw_rail(ui, &p, &mut actions);
        if self.show_sidebar {
            self.draw_sidebar(ui, &p, &mut actions);
        }
        self.draw_main(ui, &p, &mut actions);

        self.draw_settings(&ctx, &p);
        self.draw_about(&ctx, &p);
        self.draw_plugins(&ctx, &mut actions);
        self.draw_subagent_window(&ctx, &p);

        self.apply_actions(actions, &ctx);
    }

    /// The top menu bar: 文件 / 编辑 / 视图 / 帮助.
    fn draw_menu_bar(&mut self, ui: &mut egui::Ui, p: &Palette, actions: &mut Actions) {
        let mut theme_choice = self.config.theme;
        let mut theme_changed = false;
        // Seeded from the real state, not `false`: a checkbox bound to a local
        // that always starts out unchecked shows the wrong thing every frame.
        let mut show_sidebar = self.show_sidebar;
        let mut copy_transcript = false;
        let mut delete_session = false;
        let has_session = self.selected_session().is_some();

        egui::Panel::top("menubar")
            .frame(
                Frame::NONE
                    .fill(p.sidebar_bg)
                    .inner_margin(Margin::symmetric(8, 2)),
            )
            .show(ui, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.menu_button("文件", |ui| {
                        if ui.button("新建会话").clicked() {
                            actions.new_session = true;
                            ui.close();
                        }
                        if ui.button("设置").clicked() {
                            actions.open_settings = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("退出").clicked() {
                            actions.quit = true;
                            ui.close();
                        }
                    });

                    ui.menu_button("编辑", |ui| {
                        if ui
                            .add_enabled(has_session, egui::Button::new("复制会话正文"))
                            .clicked()
                        {
                            copy_transcript = true;
                            ui.close();
                        }
                        if ui
                            .add_enabled(has_session, egui::Button::new("删除当前会话"))
                            .clicked()
                        {
                            delete_session = true;
                            ui.close();
                        }
                    });

                    ui.menu_button("视图", |ui| {
                        for choice in [ThemeChoice::Dark, ThemeChoice::Light] {
                            if ui
                                .selectable_value(&mut theme_choice, choice, choice.label())
                                .clicked()
                            {
                                theme_changed = true;
                                ui.close();
                            }
                        }
                        ui.separator();
                        ui.checkbox(&mut show_sidebar, "显示侧边栏");
                    });

                    ui.menu_button("帮助", |ui| {
                        if ui.button("关于").clicked() {
                            actions.open_about = true;
                            ui.close();
                        }
                    });
                });
            });

        if theme_changed && theme_choice != self.config.theme {
            self.config.theme = theme_choice;
            theme::apply(ui.ctx(), theme_choice);
            self.save_settings();
        }
        if show_sidebar != self.show_sidebar {
            self.show_sidebar = show_sidebar;
        }
        if copy_transcript {
            if let Some(session) = self.selected_session() {
                ui.ctx().copy_text(session.as_text());
            }
        }
        if delete_session {
            if let Some(id) = self.selected {
                self.delete_session(id);
            }
        }
    }

    /// The far-left icon rail: home, plugins, settings, about, quit.
    fn draw_rail(&mut self, ui: &mut egui::Ui, p: &Palette, actions: &mut Actions) {
        let selected = self.selected.is_some();
        let settings_open = self.show_settings;
        let about_open = self.show_about;
        let plugins_open = self.show_plugins;

        egui::Panel::left("rail")
            .exact_size(RAIL_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                Frame::NONE
                    .fill(p.rail_bg)
                    .inner_margin(Margin::symmetric(6, 10)),
            )
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    // The way back to the main screen when a session is open.
                    // Sessions are created lazily on first send, so leaving a
                    // session open and starting a fresh one are the same act:
                    // deselect and show the empty state.
                    if rail_button(ui, icons::HOUSE, !selected, "主界面").clicked() {
                        actions.new_session = true;
                    }
                    // What is installed: a plugin is only visible today through
                    // a slash command in the picker, which is no way to answer
                    // "did the one I just enabled load?".
                    if rail_button(ui, icons::PUZZLE_PIECE, plugins_open, "插件").clicked() {
                        actions.open_plugins = true;
                    }
                });

                // Laid out bottom-up so the pair stays pinned however tall the
                // window is.
                ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                    if rail_button(ui, icons::GEAR, settings_open, "设置").clicked() {
                        actions.open_settings = true;
                    }
                    if rail_button(ui, icons::QUESTION, about_open, "关于").clicked() {
                        actions.open_about = true;
                    }
                });
            });
    }

    /// The session sidebar: search, projects, recent conversations.
    fn draw_sidebar(&mut self, ui: &mut egui::Ui, p: &Palette, actions: &mut Actions) {
        let mut search = std::mem::take(&mut self.search);
        let now = session::now_unix();
        let query = search.trim().to_lowercase();

        // The explicit project list, most recently added first. Cloned because
        // the panel below holds `&self.sessions` and `&mut self.search` at the
        // same time, so `self.config` is out of reach inside it.
        let projects = self.config.projects.clone();

        let sessions = &self.sessions;
        let selected = self.selected;
        // The project whose sessions are shown: the one the user last opened.
        let expanded = self.active_project.clone();
        // Cloned so the panel closure below does not borrow `self` again.
        let sidebar_error = self.sidebar_error.clone();

        egui::Panel::left("sidebar")
            .default_size(SIDEBAR_WIDTH)
            .min_size(200.0)
            .max_size(460.0)
            .show_separator_line(false)
            .frame(
                Frame::NONE
                    .fill(p.sidebar_bg)
                    .inner_margin(Margin::symmetric(8, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Deluxe Agent")
                            .size(theme::font(14.0))
                            .strong(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if !search.is_empty()
                            && ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(icons::X).size(theme::font(12.0)),
                                    )
                                    .frame(false),
                                )
                                .on_hover_text("清空搜索")
                                .clicked()
                        {
                            search.clear();
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut search)
                                .hint_text(icons::MAGNIFYING_GLASS)
                                .desired_width(130.0)
                                // Flat, but not cramped: a custom frame opts out
                                // of the default padding as well as the border.
                                .frame(Frame::NONE.inner_margin(Margin::symmetric(4, 2))),
                        );
                    });
                });

                ui.add_space(8.0);
                if sidebar_row(ui, p, icons::NOTE_PENCIL, "新聊天", false, false).clicked() {
                    actions.new_session = true;
                }
                ui.add_space(12.0);

                if !query.is_empty() {
                    section_label(ui, p, "搜索结果");
                    let mut hits = 0;
                    for session in sessions.iter().rev() {
                        if !session.title().to_lowercase().contains(&query) {
                            continue;
                        }
                        hits += 1;
                        if session_row(ui, p, session, selected, now, false).clicked() {
                            actions.select = Some(session.id);
                        }
                    }
                    if hits == 0 {
                        ui.weak("没有匹配的会话。");
                    }
                    return;
                }

                egui::ScrollArea::vertical()
                    .id_salt("sidebar")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if section_header_with_add(ui, p, "项目") {
                            actions.add_project = true;
                        }
                        // A project list that could not be written is reported
                        // here, beside the projects it is about.
                        if let Some(error) = &sidebar_error {
                            ui.label(RichText::new(error).size(theme::font(11.0)).color(BAD_RED));
                        }
                        for project in &projects {
                            let name = project_name(project);
                            let is_open = expanded.as_deref() == Some(project.as_str());
                            let response =
                                sidebar_row(ui, p, icons::FOLDER_SIMPLE, &name, is_open, true);
                            if response.clicked() {
                                actions.select_project = Some(project.clone());
                            }
                            response.context_menu(|ui| {
                                if ui.button("复制路径").clicked() {
                                    ui.ctx().copy_text(project.clone());
                                    ui.close();
                                }
                                if ui.button("移除项目").clicked() {
                                    actions.remove_project = Some(project.clone());
                                    ui.close();
                                }
                            });

                            if is_open {
                                for session in
                                    sessions.iter().filter(|s| &s.project == project).rev()
                                {
                                    if session_row(ui, p, session, selected, now, true).clicked() {
                                        actions.select = Some(session.id);
                                    }
                                }
                            }
                        }

                        ui.add_space(12.0);
                        section_label(ui, p, "最近");
                        if sessions.is_empty() {
                            ui.weak("还没有会话。");
                        }
                        for session in sessions.iter().rev() {
                            let response = session_row(ui, p, session, selected, now, false);
                            if response.clicked() {
                                actions.select = Some(session.id);
                            }
                            response.context_menu(|ui| {
                                if ui.button("删除").clicked() {
                                    actions.delete = Some(session.id);
                                    ui.close();
                                }
                            });
                        }
                    });
            });

        self.search = search;
    }

    /// The central panel: the composer pinned below the transcript.
    ///
    /// The composer is drawn first so the bottom panel can claim its height
    /// before the transcript's scroll area sizes itself from what is left.
    fn draw_main(&mut self, ui: &mut egui::Ui, p: &Palette, actions: &mut Actions) {
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(p.main_bg))
            .show(ui, |ui| {
                self.draw_composer(ui, p, actions);
                self.draw_transcript(ui, p);
            });
    }

    /// The scrolling transcript for the open session, or the empty state.
    ///
    /// Draws a `Step::Reasoning` through [`draw_reasoning_block`] so the fold can
    /// be toggled, and every other step through [`draw_step`]; the salt folds in
    /// the session id so a code block's scroll offset cannot leak between
    /// sessions.
    pub(super) fn draw_transcript(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let stick = self.stick_to_bottom;

        let Some(index) = self
            .selected
            .and_then(|id| self.sessions.iter().position(|session| session.id == id))
        else {
            // Nothing selected: name the active project, which is where the
            // next prompt will run.
            let name = self.active_project.as_deref().map(project_name);
            draw_empty_state(ui, p, name.as_deref());
            return;
        };

        if self.sessions[index].steps.is_empty() {
            let name = self.sessions[index].project_name();
            draw_empty_state(ui, p, Some(&name));
            return;
        }

        // Borrowed as two separate fields rather than through `self`: expanding a
        // reasoning block writes back into `expanded_reasoning` while the steps
        // are still being read, and going through `self` for both would make that
        // a conflict.
        // Salt for the Markdown renderer's scroll areas. The session id is in
        // it because a step index is only unique within one session, and a
        // code block that inherited another session's scroll offset would open
        // scrolled to a line that is not in it.
        let session_id = self.sessions[index].id;
        let steps = &self.sessions[index].steps;
        let expanded_reasoning = &mut self.expanded_reasoning;
        // Same story as `expanded_reasoning`: a thumbnail decode on miss writes
        // into the cache while the steps are still being read.
        let thumbs = &mut self.thumbs;

        let output = egui::ScrollArea::vertical()
            .id_salt("transcript")
            .auto_shrink([false, false])
            .stick_to_bottom(stick)
            .show(ui, |ui| {
                ui.add_space(14.0);
                // The message column: the composer's column, to the pixel. The
                // composer has already spent `CHAT_MARGIN_X` on each side, so
                // the transcript spends the same here and both columns centre
                // on the same span — one column holding the conversation and
                // the box you type the next turn into.
                let max_width =
                    (ui.available_width() - 2.0 * CHAT_MARGIN_X).min(COMPOSER_MAX_WIDTH);
                ui.vertical_centered(|ui| {
                    ui.set_width(max_width);
                    for (step_index, step) in steps.iter().enumerate() {
                        let salt = (session_id, step_index);
                        match step {
                            Step::Reasoning { id, text } => {
                                let expanded = expanded_reasoning.contains(id);
                                if draw_reasoning_block(ui, p, text, expanded, max_width) {
                                    if expanded {
                                        expanded_reasoning.remove(id);
                                    } else {
                                        expanded_reasoning.insert(*id);
                                    }
                                }
                            }
                            _ => draw_step(ui, p, step, salt, max_width, thumbs),
                        }
                    }
                });
                ui.add_space(14.0);
            });

        // Only keep following the bottom if the view was already there. Pinning
        // unconditionally would yank the transcript down whenever an old tool
        // card is expanded, and would open every old session at its very end.
        self.stick_to_bottom = output.state.offset.y + output.inner_rect.height()
            >= output.content_size.y - STICK_THRESHOLD;
    }

    /// The composer: the command picker, the input row and the queued images.
    fn draw_composer(&mut self, ui: &mut egui::Ui, p: &Palette, actions: &mut Actions) {
        // Whether the *open* session is running, not whether anything is: the
        // composer is bound to that session, so another run in flight must leave
        // this one's Send button alone.
        let running = self.selected.and_then(|id| self.run_for(id)).is_some();
        let can_send = self.can_send();

        egui::Panel::bottom("composer")
            .min_size(76.0)
            .show_separator_line(false)
            .frame(Frame::NONE.fill(p.main_bg).inner_margin(Margin {
                left: CHAT_MARGIN_X as i8,
                right: CHAT_MARGIN_X as i8,
                top: 6,
                bottom: 14,
            }))
            .show(ui, |ui| {
                let width = ui.available_width().min(COMPOSER_MAX_WIDTH);
                ui.vertical_centered(|ui| {
                    Frame::NONE
                        .fill(p.composer_bg)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(22))
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.set_width(width);

                            // Everything the picker needs, in a block so the
                            // borrow of the catalogue ends before the input row
                            // below takes `self` mutably. A `Vec<&Command>` holds
                            // its borrow until it is dropped, so merely letting it
                            // fall out of scope later would not be soon enough.
                            let (clicked_name, highlighted_name, picker_open) = {
                                let query = command_query(&self.prompt).map(str::to_string);

                                // The picker's own keys, consumed before the
                                // editor sees them — otherwise Up and Down would
                                // move the caret as well as the highlight.
                                if query.is_some() {
                                    let (up, down, escape) = ui.input_mut(|input| {
                                        (
                                            input.consume_key(
                                                egui::Modifiers::NONE,
                                                egui::Key::ArrowUp,
                                            ),
                                            input.consume_key(
                                                egui::Modifiers::NONE,
                                                egui::Key::ArrowDown,
                                            ),
                                            input.consume_key(
                                                egui::Modifiers::NONE,
                                                egui::Key::Escape,
                                            ),
                                        )
                                    });
                                    if up {
                                        self.command_highlight =
                                            self.command_highlight.saturating_sub(1);
                                    }
                                    if down {
                                        self.command_highlight += 1;
                                    }
                                    if escape {
                                        self.command_picker_dismissed = query.clone();
                                    }
                                }

                                let project = self.sending_project();
                                let candidates = match query.as_deref() {
                                    Some(query)
                                        if self.command_picker_dismissed.as_deref()
                                            != Some(query) =>
                                    {
                                        matching_commands(&self.catalogue, &project, query)
                                    }
                                    _ => Vec::new(),
                                };

                                if candidates.is_empty() {
                                    (None, None, false)
                                } else {
                                    let clicked = draw_command_picker(
                                        ui,
                                        p,
                                        &candidates,
                                        &mut self.command_highlight,
                                    );
                                    ui.add_space(6.0);
                                    let clicked_name = clicked
                                        .and_then(|index| candidates.get(index))
                                        .map(|command| command.name.clone());
                                    // The highlight is what Enter takes; the click
                                    // is taken now, below.
                                    let highlighted_name = candidates
                                        .get(self.command_highlight)
                                        .map(|command| command.name.clone());
                                    (clicked_name, highlighted_name, true)
                                }
                            };
                            if let Some(name) = &clicked_name {
                                apply_command_choice(&mut self.prompt, name);
                            }

                            ui.horizontal(|ui| {
                                ui.menu_button(
                                    RichText::new(icons::PLUS).size(theme::font(16.0)),
                                    |ui| {
                                        if ui.button("新建会话").clicked() {
                                            actions.new_session = true;
                                            ui.close();
                                        }
                                        if ui.button("添加项目…").clicked() {
                                            actions.add_project = true;
                                            ui.close();
                                        }
                                        if ui.button("粘贴图片").clicked() {
                                            actions.paste_image = true;
                                            ui.close();
                                        }
                                        if ui.button("插入图片…").clicked() {
                                            actions.pick_image = true;
                                            ui.close();
                                        }
                                    },
                                );

                                let editor_width = (ui.available_width()
                                    - CONTEXT_GAUGE_RESERVE
                                    - THINKING_PICKER_RESERVE
                                    - COMPOSER_BUTTON
                                    - 8.0)
                                    .max(120.0);
                                let editor = ui.add(
                                    egui::TextEdit::multiline(&mut self.prompt)
                                        .frame(Frame::NONE)
                                        .desired_rows(1)
                                        .desired_width(editor_width)
                                        .hint_text("随心输入")
                                        .margin(Margin::symmetric(2, 6)),
                                );
                                // Enter completes the highlighted command while
                                // the picker is open, and sends otherwise — the
                                // two cannot both fire, or a keystroke meant to
                                // finish a name would also launch the run.
                                // Shift+Enter still breaks the line either way.
                                let enter = ui.input(|input| {
                                    input.key_pressed(egui::Key::Enter) && !input.modifiers.shift
                                });
                                if enter && picker_open {
                                    if let Some(name) = &highlighted_name {
                                        apply_command_choice(&mut self.prompt, name);
                                    }
                                } else if editor.has_focus() && enter {
                                    actions.send = true;
                                }

                                // Ctrl+V on an image is handled app-side (see
                                // `intake_pasted_images`) and drag-and-drop is
                                // whole-window; the chip row below the editor
                                // shows what is queued to go out. Where the old
                                // image button sat there is now a read-only
                                // context-status gauge.
                                // The thinking picker shares this row too, left
                                // of the gauge: one conversation bar instead of
                                // a row apiece.
                                self.draw_thinking_picker(ui);
                                self.draw_context_gauge(ui, p);

                                if running {
                                    if ui
                                        .add(circle_button(icons::STOP_CIRCLE, p))
                                        .on_hover_text("停止")
                                        .clicked()
                                    {
                                        actions.stop = true;
                                    }
                                } else if ui
                                    .add_enabled(can_send, circle_button(icons::ARROW_UP, p))
                                    .on_hover_text("发送")
                                    .clicked()
                                {
                                    actions.send = true;
                                }
                            });

                            // The queued images get their own row under the
                            // input. Drawn inside the row above they would fight
                            // the editor for its width budget — the editor takes
                            // almost all of it — and push Send off the right
                            // edge of the window.
                            if !self.pending_images.is_empty() {
                                self.draw_pending_image_strip(ui, p);
                            }
                        });

                    // The background-task / sub-agent list. It takes the
                    // status line's place: instead of a one-shot "已提交", the
                    // composer shows what is actually running — and nothing at
                    // all when nothing is.
                    self.draw_jobs(ui, p);
                });
            });
    }

    /// The composer's background-task and sub-agent list.
    ///
    /// Drawn under the composer, one row per job: a sub-agent (`kind ==
    /// "subagent"`, started by `task` with `runInBackground`) or a background
    /// shell command. Running jobs come first so a live one is never pushed off
    /// by older, settled rows; the rest fill up to [`MAX_JOB_ROWS`], newest
    /// first. With no jobs the list draws nothing and the composer keeps its
    /// height.
    fn draw_jobs(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.jobs.is_empty() {
            return;
        }

        // Running jobs are always shown; settled ones fill what is left of the
        // budget, newest first, and are then put back in registration order so
        // the list reads oldest-to-newest.
        let mut indices: Vec<usize> = (0..self.jobs.len())
            .filter(|&i| !self.jobs[i].is_settled())
            .collect();
        let remaining = MAX_JOB_ROWS.saturating_sub(indices.len());
        let settled: Vec<usize> = (0..self.jobs.len())
            .filter(|&i| self.jobs[i].is_settled())
            .rev()
            .take(remaining)
            .collect();
        indices.extend(settled);
        indices.sort_unstable();
        let hidden = self.jobs.len() - indices.len();

        ui.add_space(4.0);
        // The rows are drawn first and acted on after, so the closure that
        // reads `self.jobs` never also has to write to `self`.
        let mut open: Option<String> = None;
        let mut stop: Option<String> = None;
        let open_now = self.open_subagent.clone();

        ui.vertical(|ui| {
            ui.set_width(ui.available_width().min(COMPOSER_MAX_WIDTH));
            for index in indices {
                let job = &self.jobs[index];
                let selected = open_now.as_deref() == Some(job.id.as_str());
                let click = draw_job_row(ui, p, job, selected);
                if click.open {
                    open = Some(job.id.clone());
                }
                if click.stop {
                    stop = Some(job.id.clone());
                }
            }
            if hidden > 0 {
                ui.label(
                    RichText::new(format!("…还有 {hidden} 个更早的任务"))
                        .size(theme::font(11.0))
                        .color(p.text_muted),
                );
            }
        });

        if let Some(job_id) = stop {
            self.kill_job(&job_id);
        }
        if let Some(job_id) = open {
            // The button toggles, so a second press closes the window it opened.
            self.open_subagent = (open_now.as_deref() != Some(job_id.as_str())).then_some(job_id);
        }
    }

    /// The window showing one sub-agent's own conversation.
    ///
    /// This is what forwarding a delegated agent's events buys: its reasoning,
    /// tool calls and answer are readable while they happen, and none of them
    /// enters the parent's context. The steps render through the same
    /// [`draw_step`] the transcript uses, so a tool card looks the same here as
    /// it does in a conversation.
    fn draw_subagent_window(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(job_id) = self.open_subagent.clone() else {
            return;
        };
        // The row was opened in the same frame the delegation started, before
        // any event has arrived. The next frame will have one; until then there
        // is no transcript to show.
        let Some(index) = self
            .subagent_runs
            .iter()
            .position(|run| run.job_id == job_id)
        else {
            return;
        };

        // The row's label names the role and the first line of the brief, so
        // the window's title repeats it and the two read as one thing. A job
        // the list has not reported yet falls back to its id.
        let job = self.jobs.iter().find(|job| job.id == job_id);
        let title = job
            .map(|job| job.label.clone())
            .unwrap_or_else(|| job_id.clone());
        let state = job.map(|job| job.state);
        let agent = self.subagent_runs[index].agent.clone();

        let mut open = true;
        let mut stop = false;
        let salt = self.subagent_runs[index].salt;
        // Borrowed apart from `self`, like `draw_transcript` does: folding a
        // reasoning block writes back into `expanded_reasoning` while the steps
        // are still being read.
        let steps = &self.subagent_runs[index].steps;
        let expanded_reasoning = &mut self.expanded_reasoning;
        let thumbs = &mut self.thumbs;

        egui::Window::new(title)
            .id(egui::Id::new(("subagent-window", &job_id)))
            .open(&mut open)
            .default_width(620.0)
            .default_height(460.0)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if !agent.is_empty() {
                        ui.label(RichText::new(agent).size(theme::font(12.0)).strong());
                    }
                    match state {
                        Some(state) if !state.is_settled() => {
                            ui.label(
                                RichText::new(state.label())
                                    .size(theme::font(11.0))
                                    .color(OK_GREEN),
                            );
                            if ui.button("停止").clicked() {
                                stop = true;
                            }
                        }
                        Some(state) => {
                            ui.label(
                                RichText::new(state.label())
                                    .size(theme::font(11.0))
                                    .color(p.text_muted),
                            );
                        }
                        None => {}
                    }
                });
                ui.separator();

                egui::ScrollArea::vertical()
                    .id_salt(("subagent-transcript", salt))
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let max_width = ui.available_width();
                        for (step_index, step) in steps.iter().enumerate() {
                            let step_salt = (salt, step_index);
                            match step {
                                // Reasoning is drawn here rather than through
                                // `draw_step` so the fold can be opened and
                                // closed, the same way the transcript does it.
                                Step::Reasoning { id, text } => {
                                    let expanded = expanded_reasoning.contains(id);
                                    if draw_reasoning_block(ui, p, text, expanded, max_width) {
                                        if expanded {
                                            expanded_reasoning.remove(id);
                                        } else {
                                            expanded_reasoning.insert(*id);
                                        }
                                    }
                                }
                                _ => draw_step(ui, p, step, step_salt, max_width, thumbs),
                            }
                        }
                    });
            });

        if stop {
            self.kill_job(&job_id);
        }
        if !open {
            self.open_subagent = None;
        }
    }

    /// The per-conversation reasoning-effort picker, drawn in the composer.
    ///
    /// A property of the chat rather than the endpoint, so it sits beside the
    /// input the same way pi puts its shift+tab indicator there. The chosen value
    /// is stamped onto the session on send (see [`App::start_run`]) and reloaded
    /// when a session is opened, so it survives switching chats and a restart.
    ///
    /// Drawn inline on the editor's row, just left of the context gauge, so
    /// composer + picker + gauge read as one control strip instead of two.
    /// The full "思考强度：高（high）" text was the old own-row form; inline it
    /// shrinks to the label alone, with the full wording in the hover and in
    /// the dropdown.
    fn draw_thinking_picker(&mut self, ui: &mut egui::Ui) {
        let selected = match self.thinking {
            // The wire spelling stays in the dropdown and the hover text; on
            // the row itself the two-character label is what fits beside the
            // gauge without crowding the editor.
            Some(level) => level.label().to_string(),
            None => "默认".to_string(),
        };
        let hover = match self.thinking {
            Some(level) => format!("思考强度：{}（{}）", level.label(), level.wire()),
            None => "思考强度：默认".to_string(),
        };
        egui::ComboBox::from_id_salt("composer-thinking-level")
            .selected_text(RichText::new(selected).size(theme::font(12.0)))
            .width(72.0)
            .show_ui(ui, |ui| {
                // Unset first: leaving the parameter out is a real choice,
                // not the absence of one — and the safe default, because an
                // endpoint that never saw `reasoning_effort` must not be
                // handed one.
                ui.selectable_value(&mut self.thinking, None, "默认（不发送 reasoning_effort）");
                for level in ThinkingLevel::ALL {
                    ui.selectable_value(
                        &mut self.thinking,
                        Some(level),
                        format!("{}（{}）", level.label(), level.wire()),
                    );
                }
            })
            .response
            .on_hover_text(hover);
    }

    /// The context-status gauge, where the composer's image button used to be.
    ///
    /// Pasting and picking images moved fully to Ctrl+V and drag-and-drop, so
    /// the button the mouse used to reach became free. What the composer had no
    /// room to show before was the one number that decides when a conversation
    /// will be compacted: how much of the model's context window this chat has
    /// already filled.
    ///
    /// The figure is the provider's own prompt-token count for the last request
    /// (`Session::context_measurement`), never a local estimate — the same
    /// measurement `context::ContextWindow` compacts from, so the gauge and the
    /// compaction can never disagree. Nothing measured yet (a fresh chat, or an
    /// endpoint that never reported usage) leaves the arc unpainted, and no
    /// window configured (limit 0) greys the whole thing out.
    ///
    /// Deliberately not a button: it is a gauge. The mouse paths to the image
    /// intake stay where they already were, in the plus menu — duplicating them
    /// here is what this slot used to do.
    fn draw_context_gauge(&self, ui: &mut egui::Ui, p: &Palette) {
        let measured = self
            .selected_session()
            .and_then(|session| session.context_measurement)
            .map(|(tokens, _)| tokens);
        let limit = self.config.context.context_limit;

        let used = measured.unwrap_or(0);
        let ratio = if limit > 0 {
            (used as f32 / limit as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let compaction = self.config.context.threshold_ratio();
        // The amber line sits at four fifths of the way to the trigger, so a
        // share the user sets low cannot put it above the red one.
        let warning = compaction * 0.8;
        let colour = if limit == 0 {
            p.text_muted
        } else if ratio >= compaction {
            BAD_RED
        } else if ratio >= warning {
            WARN_AMBER
        } else {
            OK_GREEN
        };

        let mut summary = match (measured, limit > 0) {
            (Some(tokens), true) => format!(
                "上下文：{} / {}（{:.0}%）",
                format_tokens(tokens),
                format_tokens(limit),
                ratio * 100.0
            ),
            (Some(tokens), false) => format!(
                "上下文：已用 {} tokens；设置里未配置窗口大小",
                format_tokens(tokens)
            ),
            (None, true) => format!("上下文：窗口 {}，还没有用量数据", format_tokens(limit)),
            (None, false) => "上下文：还没有用量数据；设置里未配置窗口大小".to_string(),
        };
        if limit > 0 && ratio >= compaction && measured.is_some() {
            summary.push_str(" —— 达到压缩阈值，下一条消息会先压缩历史");
        }

        // The label decides the box, not the other way round: the painter clips
        // to whatever rect is allocated here, so a box guessed too small is what
        // cut "89%" in half. Laying the text out first makes the box exactly as
        // wide as ring + gap + label, at any type size or digit count.
        let label = if measured.is_some() && limit > 0 {
            format!("{:.0}%", ratio * 100.0)
        } else {
            "–".to_string()
        };
        let label_font = egui::FontId::proportional(theme::font(12.0));
        let label_width = ui
            .painter()
            .layout_no_wrap(label.clone(), label_font.clone(), Color32::PLACEHOLDER)
            .size()
            .x;
        let gauge_width = COMPOSER_BUTTON + GAUGE_RING_GAP + label_width;
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(gauge_width, COMPOSER_BUTTON),
            egui::Sense::hover(),
        );

        // The ring: a full muted track with the measured share painted over it
        // in the state colour, starting from twelve o'clock. With nothing
        // measured the arc stays hidden and the track reads as an idle dial —
        // which is the "nothing claimed yet" the old icon button conveyed.
        let painter = ui.painter_at(rect);
        // Anchored to the box's left edge — the box is `ring | gap | label`
        // laid out from the left, so centring the ring in the whole box would
        // push the label past the clip and trim it, the original bug.
        let centre = Pos2::new(rect.min.x + COMPOSER_BUTTON / 2.0, rect.center().y);
        let radius = COMPOSER_BUTTON / 2.0 - 4.0;
        let track = Stroke::new(2.0, p.border);
        let active = Stroke::new(2.0, colour);
        let notch_start = -std::f32::consts::FRAC_PI_2;
        let points = |from: f32, to: f32| {
            (0..=SEGMENTS_PER_RING)
                .map(|step| {
                    let angle = from + (to - from) * step as f32 / SEGMENTS_PER_RING as f32;
                    Pos2::new(
                        centre.x + radius * angle.cos(),
                        centre.y + radius * angle.sin(),
                    )
                })
                .collect::<Vec<Pos2>>()
        };
        painter.add(egui::Shape::line(
            points(notch_start, notch_start + std::f32::consts::TAU),
            track,
        ));
        if measured.is_some() {
            let sweep = std::f32::consts::TAU * ratio.max(1.0 / SEGMENTS_PER_RING as f32);
            painter.add(egui::Shape::line(
                points(notch_start, notch_start + sweep),
                active,
            ));
        }

        // The number beside the ring, at the position the box was sized from —
        // or a dash where there is no share to show (nothing measured, or no
        // window to be a share of).
        painter.text(
            Pos2::new(centre.x + radius + GAUGE_RING_GAP, centre.y),
            egui::Align2::LEFT_CENTER,
            label,
            label_font,
            if measured.is_some() && limit > 0 {
                colour
            } else {
                p.text_muted
            },
        );

        response.on_hover_text(summary);
    }

    /// The queued images inside the composer, each removable.
    fn draw_pending_image_strip(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for index in (0..self.pending_images.len()).rev() {
                let id = format!("pending-image-{}", self.pending_images[index].id);
                if remove_chip(ui, p, &id, &self.pending_images[index]).clicked() {
                    self.pending_images.remove(index);
                }
            }
        });
    }

    /// The 设置 window: endpoint, key, context and safety settings.
    fn draw_settings(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.show_settings {
            return;
        }

        let mut open = true;
        let mut save = false;

        egui::Window::new("设置")
            .open(&mut open)
            .default_width(620.0)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.heading("模型");
                ui.weak("任何兼容 OpenAI /chat/completions 的服务都可以。");
                egui::Grid::new("llm-settings")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Base URL");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.llm.base_url)
                                .desired_width(400.0),
                        );
                        ui.end_row();

                        ui.label("模型名");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config.llm.model)
                                .desired_width(400.0),
                        );
                        ui.end_row();

                        ui.label("输入模态");
                        ui.horizontal(|ui| {
                            // Text is what every chat model accepts, so it is
                            // shown ticked and disabled: the pair reads as a
                            // complete answer rather than a choice that can be
                            // left blank. Ticking 图片 is what registers
                            // `read_image`.
                            let mut text = true;
                            ui.add_enabled(false, egui::Checkbox::new(&mut text, "文本"));

                            let mut supports_image = self.config.llm.supports_images();
                            if ui.checkbox(&mut supports_image, "图片").changed() {
                                self.config.llm.input = if supports_image {
                                    vec![InputModality::Text, InputModality::Image]
                                } else {
                                    vec![InputModality::Text]
                                };
                            }
                        });
                        ui.end_row();

                        ui.label("API Key");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.api_key)
                                .password(true)
                                .desired_width(400.0),
                        );
                        ui.end_row();

                        ui.label("上下文长度");
                        ui.horizontal(|ui| {
                            // A text edit rather than a DragValue: the value
                            // is huge and the interesting edits are the
                            // `1M`-style shorthands, neither of which a drag
                            // is any good for.
                            let mut limit = self.context_limit_text.clone();
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut limit)
                                    .desired_width(160.0)
                                    .hint_text("0 = 不压缩"),
                            );
                            if response.changed() && !limit.trim().is_empty() {
                                if let Some(tokens) = parse_token_count(&limit) {
                                    self.config.context.context_limit = tokens;
                                    self.context_limit_text = limit;
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("最大输出长度");
                        ui.horizontal(|ui| {
                            // Same shorthand as the context window above. An
                            // empty field clears the budget, leaving the
                            // provider's own ceiling in force; the text buffer
                            // is cleared with it, so the field stays blank
                            // rather than silently reverting to a stale figure.
                            let mut budget = self.max_output_tokens_text.clone();
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut budget)
                                    .desired_width(160.0)
                                    .hint_text("留空 = 不限制"),
                            );
                            if response.changed() {
                                self.max_output_tokens_text = budget.clone();
                                self.config.llm.max_output_tokens = parse_token_count(&budget)
                                    .filter(|tokens| *tokens > 0)
                                    .and_then(|tokens| u32::try_from(tokens).ok());
                            }
                        });
                        ui.end_row();

                        ui.label("失败重试次数");
                        ui.horizontal(|ui| {
                            ui.add_enabled(
                                !self.config.llm.retry_forever,
                                egui::DragValue::new(&mut self.config.llm.retry_count)
                                    .range(0..=crate::config::MAX_RETRY_COUNT),
                            );
                            ui.checkbox(&mut self.config.llm.retry_forever, "无限重试");
                        });
                        ui.end_row();

                        ui.label("压缩阈值");
                        ui.horizontal(|ui| {
                            let mut threshold = self.config.context.threshold_percent as f64;
                            let response = ui.add(
                                egui::DragValue::new(&mut threshold)
                                    .speed(1.0)
                                    .range(0.0..=100.0)
                                    .suffix("%"),
                            );
                            if response.changed() {
                                self.config.context.threshold_percent = threshold as u32;
                            }
                        });
                        ui.end_row();
                    });

                ui.add_space(10.0);
                ui.separator();
                ui.heading("安全");
                ui.add_space(4.0);
                ui.checkbox(
                    &mut self.config.tools.block_destructive_commands,
                    "拦截不可逆的破坏性命令",
                );
                ui.weak(
                    "开启时会拒绝 rm -rf、格式化磁盘、dd 写裸设备、关机一类命令；\
                     其余操作一律放行。关闭后模型拥有完全权限。",
                );

                ui.add_space(10.0);
                ui.separator();
                if ui.button("保存").clicked() {
                    save = true;
                }

                // Where a save failure is shown: on the page whose save failed,
                // not in some shared corner of the window.
                if let Some(error) = &self.settings_error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(error).color(BAD_RED));
                }
            });

        if save {
            // 点「保存」就顺带存凭据库，这样 key 不会只活在内存里：
            // 重启后凭据库里的 key 会自动被读回来。失败不影响其余设置。
            if !self.api_key.trim().is_empty() {
                if let Err(error) = config::store_api_key(&self.api_key) {
                    tracing::warn!(%error, "failed to store the API key");
                    self.settings_error = Some(format!("存储 API Key 失败：{error}"));
                    return;
                }
            }
            self.save_settings();
        }

        let _ = p;
        if !open {
            self.show_settings = false;
            self.settings_error = None;
        }
    }

    /// The 关于 window: version and the paths this app reads and writes.
    fn draw_about(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.show_about {
            return;
        }

        let mut open = true;
        egui::Window::new("关于")
            .open(&mut open)
            .default_width(460.0)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    draw_logo(ui, p);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Deluxe Agent")
                                .size(theme::font(16.0))
                                .strong(),
                        );
                        ui.weak(concat!("版本 ", env!("CARGO_PKG_VERSION")));
                    });
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);

                let mut path_row = |label: &str, path: Option<PathBuf>| {
                    ui.horizontal(|ui| {
                        ui.label(label);
                        match path {
                            Some(path) => {
                                ui.monospace(path.display().to_string());
                            }
                            None => {
                                ui.weak("不可用");
                            }
                        }
                    });
                };
                path_row("配置文件", config::config_path());
                path_row("会话记录", session::store_path());

                ui.add_space(6.0);
                ui.weak("模型拥有本机完全读写权限，仅拦截不可逆的破坏性命令。");
            });

        if !open {
            self.show_about = false;
        }
    }

    /// What is installed, as a window.
    ///
    /// Discovery reads the personal marketplace and the ones bundled with Codex,
    /// but the only other trace of the result on screen is a slash command's
    /// name in the composer's picker — so a plugin whose commands you had not
    /// typed a `/` for was invisible, and the log was the only place to find out
    /// whether an id had resolved at all.
    ///
    /// Global scope only. A project's own plugins are a property of that
    /// repository, and listing them here would answer a question about a project
    /// the user may not be looking at.
    ///
    /// Each row expands to what the plugin brings and carries the two acts on
    /// it: a switch, which is reversible, and an uninstall, which asks first.
    fn draw_plugins(&mut self, ctx: &egui::Context, actions: &mut Actions) {
        if !self.show_plugins {
            return;
        }

        // Taken out of `self` for the duration so the window can be drawn
        // against a borrow of the catalogue; written back below. The buttons
        // land in `actions` and are applied once every borrow has been released,
        // the same shape the rest of the app uses.
        let mut pending = self.pending_uninstall.clone();

        let mut open = true;
        egui::Window::new("插件")
            .open(&mut open)
            .default_width(620.0)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.weak(
                    "全局生效的插件，来自个人 marketplace 与 Codex 自带的 bundled marketplace。",
                );

                // Where a plugin action's failure is shown: on the page that
                // performed it.
                if let Some(error) = &self.plugins_error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(error).color(BAD_RED));
                }
                ui.add_space(6.0);

                let enabled = self.catalogue.global();
                let disabled = self.catalogue.disabled();

                if enabled.is_empty() && disabled.is_empty() {
                    ui.weak("没有已安装的插件。");
                    ui.add_space(4.0);
                    ui.weak("用 Codex 安装一个插件后，它就会出现在这里。");
                    return;
                }

                if !enabled.is_empty() {
                    ui.label(RichText::new(format!("已启用（{}）", enabled.len())).strong());
                    for plugin in enabled {
                        draw_plugin_row(ui, plugin, true, &mut pending, actions);
                    }
                }

                if !disabled.is_empty() {
                    ui.add_space(10.0);
                    ui.label(RichText::new(format!("已停用（{}）", disabled.len())).strong());
                    for plugin in disabled {
                        draw_plugin_row(ui, plugin, false, &mut pending, actions);
                    }
                }
            });

        self.pending_uninstall = pending;
        if !open {
            self.show_plugins = false;
            self.pending_uninstall = None;
            self.plugins_error = None;
        }
    }
}

/// One plugin's row: a header that expands to its contents, and the acts on it.
///
/// `enabled` only picks the label and the emphasis — the two buttons are the
/// same either way, one flipping the switch and one removing the plugin. An
/// uninstall is asked about inline rather than through a second dialog, so the
/// question sits beside the plugin it is about.
fn draw_plugin_row(
    ui: &mut egui::Ui,
    plugin: &plugins::LoadedPlugin,
    enabled: bool,
    pending: &mut Option<String>,
    actions: &mut Actions,
) {
    let version = plugin.manifest.version.as_deref().unwrap_or("版本未知");
    let title = format!("{}  {version}", plugin.display_name());
    let header = if enabled {
        RichText::new(title).strong()
    } else {
        RichText::new(title).weak()
    };

    egui::CollapsingHeader::new(header)
        .id_salt(&plugin.id)
        .default_open(false)
        .show(ui, |ui| {
            if let Some(summary) = plugin.summary() {
                ui.label(summary);
            }
            ui.horizontal(|ui| {
                ui.weak("id");
                ui.monospace(&plugin.id);
            });
            ui.horizontal(|ui| {
                ui.weak("目录");
                ui.monospace(plugin.root.display().to_string());
            });

            draw_plugin_contents(ui, plugin);

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(if enabled { "停用" } else { "启用" }).clicked() {
                    actions.set_plugin = Some((plugin.id.clone(), !enabled));
                }
                if ui.button("卸载").clicked() {
                    *pending = Some(plugin.id.clone());
                }
            });

            if pending.as_deref() == Some(plugin.id.as_str()) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("确认卸载？").color(BAD_RED));
                    if ui.button("确认").clicked() {
                        actions.uninstall_plugin = Some(plugin.id.clone());
                        *pending = None;
                    }
                    if ui.button("取消").clicked() {
                        *pending = None;
                    }
                });
                ui.weak("只删除 Codex 缓存里的副本；bundled 插件和本地工作副本不会被删。");
            }
        });
}

/// What one plugin brought, listed by name.
///
/// Names rather than just counts: the question this window answers is "did the
/// thing I enabled actually load?", and `技能 2` cannot answer it — the two
/// skills might be the wrong two. A plugin that brings nothing still says so,
/// so an empty body is never mistaken for a failed load.
fn draw_plugin_contents(ui: &mut egui::Ui, plugin: &plugins::LoadedPlugin) {
    let sections: [(&str, Vec<&str>); 5] = [
        (
            "技能",
            plugin.skills.iter().map(|s| s.name.as_str()).collect(),
        ),
        (
            "命令",
            plugin.commands.iter().map(|c| c.name.as_str()).collect(),
        ),
        (
            "钩子",
            plugin.hooks.iter().map(|h| h.pattern.as_str()).collect(),
        ),
        (
            "子代理",
            plugin.agents.iter().map(|a| a.name.as_str()).collect(),
        ),
        (
            "MCP",
            plugin.mcp_servers.keys().map(String::as_str).collect(),
        ),
    ];

    let mut any = false;
    for (label, items) in sections {
        if items.is_empty() {
            continue;
        }
        any = true;
        ui.horizontal_wrapped(|ui| {
            ui.weak(format!("{label}："));
            ui.label(items.join("、"));
        });
    }
    if !any {
        ui.weak("不含技能、命令、钩子、子代理或 MCP server");
    }
}

/// What a click on a task row asked the composer to do.
///
/// Both are buttons rather than a clickable row: a row that opened a window on
/// a stray click would fight the stop button sitting inside it, and the two
/// affordances are different enough to be worth naming on screen.
#[derive(Default)]
struct JobRowClick {
    /// Open (or close) that sub-agent's transcript window.
    open: bool,
    /// Ask the worker to stop the job.
    stop: bool,
}

/// One row of the composer's background-task list.
///
/// A sub-agent and a background command share the row; the glyph, the tag and
/// the buttons differ, because the label and status read the same way for both.
/// `selected` is whether this row's sub-agent window is the one on screen.
fn draw_job_row(ui: &mut egui::Ui, p: &Palette, job: &JobView, selected: bool) -> JobRowClick {
    let (icon, tag) = if job.is_subagent() {
        (icons::ROBOT, "子代理")
    } else {
        (icons::TERMINAL_WINDOW, "后台任务")
    };
    let colour = job_colour(job, p);
    let mut click = JobRowClick::default();

    let response = ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(theme::font(12.0)).color(colour));
        ui.label(
            RichText::new(tag)
                .size(theme::font(11.0))
                .color(p.text_muted)
                .strong(),
        );
        ui.label(
            RichText::new(shorten(&job.label, 60))
                .size(theme::font(11.0))
                .color(p.text),
        );

        // Right-aligned: the status, then the buttons. Laid out right-to-left,
        // so the order here is the reverse of how they read on screen.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !job.is_settled()
                && job_button(ui, icons::STOP_CIRCLE, false, "停止这个任务").clicked()
            {
                click.stop = true;
            }
            // Only a sub-agent has a transcript to show; a command's output is
            // read with `job_output` and has no window of its own.
            if job.is_subagent() && job_button(ui, icons::EYE, selected, "查看它的过程").clicked()
            {
                click.open = true;
            }
            ui.label(
                RichText::new(job_status_text(job))
                    .size(theme::font(11.0))
                    .color(colour),
            );
        });
    });

    // The full label and the kind-specific detail, which the one-line row has
    // to cut: a hover is where a truncated command and an exit code live.
    let mut hover = format!("{} · {}", job.id, job.label);
    if let Some(detail) = &job.detail {
        hover.push('\n');
        hover.push_str(detail);
    }
    response.response.on_hover_text(hover);

    click
}

fn job_button(ui: &mut egui::Ui, icon: &str, selected: bool, tooltip: &str) -> egui::Response {
    ui.add_sized(
        [20.0, 18.0],
        egui::Button::selectable(selected, RichText::new(icon).size(theme::font(11.0)))
            .frame(false),
    )
    .on_hover_text(tooltip)
}

fn job_colour(job: &JobView, p: &Palette) -> Color32 {
    match job.state {
        JobState::Running => OK_GREEN,
        JobState::Stopping => WARN_AMBER,
        JobState::Completed => p.text_muted,
        JobState::Killed => WARN_AMBER,
        JobState::Failed => BAD_RED,
    }
}

fn rail_button(ui: &mut egui::Ui, icon: &str, selected: bool, tooltip: &str) -> egui::Response {
    ui.add_sized(
        [36.0, 36.0],
        egui::Button::selectable(selected, RichText::new(icon).size(theme::font(17.0))),
    )
    .on_hover_text(tooltip)
}

/// One full-width row in the sidebar. A single truncated line, like
/// [`session_row`], so a long project path cannot make the row two lines tall.
fn sidebar_row(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: &str,
    label: &str,
    selected: bool,
    muted: bool,
) -> egui::Response {
    let text = RichText::new(format!("{icon}  {label}")).size(theme::font(13.0));
    let text = if muted && !selected {
        text.color(p.text_muted)
    } else {
        text
    };

    ui.add_sized(
        [ui.available_width(), 28.0],
        // A growing atom soaks up the slack, which is what pins the label to the
        // left; a `Button` centres its contents otherwise.
        egui::Button::selectable(selected, (text, egui::Atom::grow())).truncate(),
    )
}

/// One session row: a single truncated line, so a long prompt cannot push the
/// rest of the list off screen.
fn session_row(
    ui: &mut egui::Ui,
    p: &Palette,
    session: &Session,
    selected: Option<Uuid>,
    now: u64,
    nested: bool,
) -> egui::Response {
    let indent = if nested { "    " } else { "" };
    let label = format!(
        "{indent}{}",
        shorten(&session.title(), if nested { 26 } else { 32 })
    );

    let text = RichText::new(label).size(theme::font(13.0));
    let text = if selected == Some(session.id) {
        text
    } else {
        text.color(p.text_muted)
    };

    let marker = match session.state {
        RunState::Running => "●",
        RunState::Finished | RunState::Failed => "",
    };

    let response = ui.add_sized(
        [ui.available_width(), 26.0],
        // `shorten` caps the text and flattens its newlines; `truncate` is what
        // makes the row a *single* line — the cap is a character count, and a
        // wide CJK title blows past the row's width long before it hits it.
        egui::Button::selectable(selected == Some(session.id), (text, egui::Atom::grow()))
            .truncate(),
    );

    if !marker.is_empty() {
        // A dot on the right edge, the way a running chat is marked.
        let rect = response.rect;
        ui.painter().circle_filled(
            egui::pos2(rect.right() - 12.0, rect.center().y),
            3.5,
            if session.state == RunState::Running {
                p.accent
            } else {
                BAD_RED
            },
        );
    }

    response.on_hover_text(format!(
        "{}\n{}",
        session.title(),
        session::age_label(session.created_at, now)
    ))
}

fn section_label(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(text)
            .size(theme::font(11.0))
            .color(p.text_muted),
    );
    ui.add_space(2.0);
}

/// A section label with a trailing `+` button, for a section that can be added
/// to. Returns whether `+` was clicked.
///
/// Laid out as a row rather than label-then-button so the action shares the
/// header's line: a `+` that dropped to its own row would read as a control
/// belonging to the first item rather than to the section.
fn section_header_with_add(ui: &mut egui::Ui, p: &Palette, text: &str) -> bool {
    let mut clicked = false;
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(text)
                .size(theme::font(11.0))
                .color(p.text_muted),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            clicked = ui
                .add(
                    egui::Button::new(
                        RichText::new(icons::PLUS)
                            .size(theme::font(12.0))
                            .color(p.text_muted),
                    )
                    .frame(false),
                )
                .on_hover_text("添加项目")
                .clicked();
        });
    });
    ui.add_space(2.0);
    clicked
}

fn circle_button<'a>(icon: &'a str, p: &Palette) -> egui::Button<'a> {
    egui::Button::new(RichText::new(icon).size(theme::font(15.0)).color(p.main_bg))
        .fill(p.text)
        .corner_radius(CornerRadius::same(COMPOSER_BUTTON as u8 / 2))
        .min_size(Vec2::splat(COMPOSER_BUTTON))
}

/// One picker row: the command's name, then its summary in muted type.
///
/// A single `LayoutJob` rather than two widgets, because the row has to be one
/// clickable target — two would leave the gap between them dead to the mouse.
fn command_row_job(command: &plugins::Command, p: &Palette) -> LayoutJob {
    let name_font = FontId::proportional(theme::font(13.0));
    let summary_font = FontId::proportional(theme::font(11.0));
    let mut job = LayoutJob::default();

    append_run(&mut job, &format!("/{}", command.name), &name_font, p.text);
    if let Some(description) = &command.description {
        append_run(&mut job, "  ", &summary_font, p.text_muted);
        append_run(
            &mut job,
            &shorten(description, 64),
            &summary_font,
            p.text_muted,
        );
    }
    job
}

/// The command picker: what a `/` in the composer can become.
///
/// A row of the composer card rather than a floating overlay. An overlay
/// anchored above a bottom panel has to be positioned by hand against a rect the
/// panel is still reserving, and is clipped when the window is short; a row in
/// the card is always visible and needs no positioning at all. The cost is that
/// it pushes the transcript up while it is open, which is honest about the space
/// it takes.
///
/// Returns the row the user clicked, if any. The highlight is clamped here rather
/// than by the caller so a list that shrinks as the user types cannot leave it
/// pointing past the end.
fn draw_command_picker(
    ui: &mut egui::Ui,
    p: &Palette,
    candidates: &[&plugins::Command],
    highlight: &mut usize,
) -> Option<usize> {
    *highlight = (*highlight).min(candidates.len().saturating_sub(1));

    let mut clicked = None;
    Frame::NONE
        .fill(p.main_bg)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(6, 4))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("composer-command-picker")
                .max_height(COMMAND_PICKER_MAX_HEIGHT)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (index, command) in candidates.iter().enumerate() {
                        let row = ui.add_sized(
                            [ui.available_width(), COMMAND_ROW_HEIGHT],
                            // The growing atom soaks up the slack, which pins the
                            // text left; a `Button` centres its contents otherwise.
                            egui::Button::selectable(
                                index == *highlight,
                                (command_row_job(command, p), egui::Atom::grow()),
                            )
                            .truncate(),
                        );
                        if row.clicked() {
                            clicked = Some(index);
                        }
                    }
                });
        });
    clicked
}

/// The placeholder mark: a rounded square with a terminal glyph in it, which is
/// the shape the reference uses and needs no image asset.
fn draw_logo(ui: &mut egui::Ui, p: &Palette) {
    let size = 56.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let radius = CornerRadius::same(14);
    ui.painter().rect_filled(rect, radius, p.hover_bg);
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, p.border),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        icons::TERMINAL_WINDOW,
        egui::FontId::proportional(theme::font(26.0)),
        p.text_muted,
    );
}

fn draw_empty_state(ui: &mut egui::Ui, p: &Palette, project: Option<&str>) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.20).max(20.0));
        draw_logo(ui, p);
        ui.add_space(20.0);
        ui.set_max_width(620.0);
        let text = match project {
            Some(name) => format!("你想让我们在 {name} 中构建什么?"),
            None => "你想让我们构建什么?".to_string(),
        };
        ui.add(
            egui::Label::new(RichText::new(text).size(theme::font(24.0)).color(p.text))
                .wrap()
                .halign(Align::Center),
        );
    });
}

/// Renders one transcript step.
///
/// `max_width` is the message column every step is laid out in — the user's
/// bubble hugs its right edge, everything the agent produces fills it.
fn draw_step(
    ui: &mut egui::Ui,
    p: &Palette,
    step: &Step,
    salt: (Uuid, usize),
    max_width: f32,
    thumbs: &mut HashMap<String, TextureHandle>,
) {
    match step {
        Step::User { text, images } => {
            draw_bubble(ui, p, text, salt, p.bubble_user, Align::Max, max_width);
            draw_user_images(ui, salt, images, thumbs);
        }
        Step::Assistant { text } => draw_agent_message(ui, p, text, salt, max_width),
        Step::Notice { text } => {
            draw_bubble(ui, p, text, salt, p.bubble_notice, Align::Center, max_width);
        }
        // Reasoning needs the expanded set, which this function has no access to;
        // `draw_transcript` intercepts it before calling here. This arm only
        // exists to keep the match exhaustive, and renders collapsed. (The id
        // lives on the step; without the expanded set there is nothing to look
        // up.)
        Step::Reasoning { text, .. } => {
            draw_reasoning_block(ui, p, text, false, max_width);
        }
        Step::Tool {
            call_id,
            name,
            arguments,
            result,
        } => draw_tool_card(ui, p, call_id, name, arguments, result.as_ref(), max_width),
        Step::Compaction { summary } => {
            let text = if summary.trim().is_empty() {
                "上下文压缩失败，对话按原样继续".to_string()
            } else {
                format!("上下文已压缩。此前对话的摘要：\n\n{summary}")
            };
            draw_bubble(
                ui,
                p,
                &text,
                salt,
                p.bubble_notice,
                Align::Center,
                max_width,
            );
        }
    }
}

/// The images one user step was sent with, as thumbnails under its bubble.
///
/// Right-aligned like the bubble they belong to: they are part of the same
/// turn, and a row of thumbnails left hanging under a bubble pushed to the
/// other side would read as somebody else's message.
///
/// Only the *sent* turn renders them: the wire replay resolves the same
/// references again, so the model keeps seeing the picture, but redrawing a
/// thumbnail in every later step would be noise. An image whose bytes can no
/// longer be read back is skipped with a log line — the transcript keeps its
/// text, which is the part that survives.
fn draw_user_images(
    ui: &mut egui::Ui,
    salt: (Uuid, usize),
    images: &[ImageRef],
    thumbs: &mut HashMap<String, TextureHandle>,
) {
    if images.is_empty() {
        return;
    }
    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
        ui.add_space(10.0);
        for (image_index, image) in images.iter().enumerate() {
            let Some(texture) = transcript_thumb(thumbs, ui.ctx(), image) else {
                tracing::warn!(id = %image.id, "an attached image could no longer be rendered");
                continue;
            };
            ui.push_id((salt, image_index), |ui| {
                ui.add(
                    egui::Image::from_texture(&texture)
                        .max_size(Vec2::new(TRANSCRIPT_THUMB.0, TRANSCRIPT_THUMB.1))
                        .corner_radius(CornerRadius::same(8)),
                )
                .on_hover_text(attachment_caption(image));
            });
        }
    });
}

/// The stored bytes of an attachment, as a cached texture.
///
/// Cached on the `App` rather than through egui's image loaders: the loaders
/// need `egui_extras`' file feature (and a registered loader) to resolve a
/// URI, while the bytes here come from the attachment store. `load_texture`
/// allocates a fresh texture on every call, so the map on `App` is what keeps
/// the decode to once per image per process. The entries live as long as the
/// window does — thumbnails are small, and a session that scrolled away costs
/// nothing until it is drawn again.
fn transcript_thumb(
    thumbs: &mut HashMap<String, TextureHandle>,
    ctx: &egui::Context,
    image: &ImageRef,
) -> Option<TextureHandle> {
    if let Some(cached) = thumbs.get(&image.id) {
        return Some(cached.clone());
    }

    let bytes = attachments::load_bytes(image).ok()?;
    let raster = if image.media_type == "image/jpeg" {
        image_ops::decode_jpeg(&bytes).ok()?
    } else {
        image_ops::decode_png(&bytes).ok()?
    };
    let texture = ctx.load_texture(
        format!("thumb://{}", image.id),
        ColorImage::from_rgba_unmultiplied(
            [raster.width as usize, raster.height as usize],
            &raster.rgba,
        ),
        TextureOptions::default(),
    );
    thumbs.insert(image.id.clone(), texture.clone());
    Some(texture)
}

/// The model's chain of thought, collapsed until clicked.
///
/// Returns whether the toggle was clicked. Streaming grows the block below the
/// user's cursor, so thinking turns out to answer without ever yanking the
/// transcript around; `stick_to_bottom` only re-engages near the bottom.
fn draw_reasoning_block(
    ui: &mut egui::Ui,
    p: &Palette,
    text: &str,
    expanded: bool,
    max_width: f32,
) -> bool {
    let marker = if expanded {
        icons::CARET_DOWN
    } else {
        icons::CARET_RIGHT
    };
    let header = format!("{marker}  {} 思维链", icons::BRAIN);

    let mut clicked = false;
    // The chain of thought is part of the agent's turn, so it lines up with the
    // reply it precedes: the same centred column, hugged to its left edge. Left
    // to itself the chip would sit out at the transcript's edge while the answer
    // began a sixth of the window further in.
    ui.vertical_centered(|ui| {
        Frame::NONE.show(ui, |ui| {
            ui.set_width(max_width);
            Frame::NONE
                .fill(p.bubble_assistant)
                .corner_radius(CornerRadius::same(14))
                .inner_margin(Margin::symmetric(12, 6))
                .show(ui, |ui| {
                    // Toggle, then the body *under* it. The widgets used to be
                    // laid out side by side, so the reasoning text sat to the
                    // right of the button and the pair drifted across the
                    // column — an expanded block opened with a wide blank gap
                    // between its own halves.
                    ui.vertical(|ui| {
                        let response = ui
                            .add(
                                egui::Button::new(
                                    RichText::new(header)
                                        .size(theme::font(13.0))
                                        .color(p.text_muted),
                                )
                                .frame(false),
                            )
                            .on_hover_text(if expanded {
                                "点击折叠"
                            } else {
                                "点击展开模型的思考过程"
                            });
                        clicked = response.clicked();
                        if expanded {
                            selectable_code(ui, text);
                        }
                    });
                });
        });
    });
    ui.add_space(8.0);

    clicked
}

/// The assistant's reply, as plain Markdown in a centred column.
///
/// Deliberately not a bubble. A fill around every reply only adds a box to read
/// past, and the column is already narrower than the panel, so the two sides
/// stay tellable apart: the user's turns are filled and hug the right, the
/// agent's sit open in the column.
///
/// The column is a fixed width rather than content-sized so consecutive replies
/// start at the same x — a column that shrank to each message would drift
/// around the middle of the window as the answer streamed in.
fn draw_agent_message(
    ui: &mut egui::Ui,
    p: &Palette,
    text: &str,
    salt: (Uuid, usize),
    max_width: f32,
) {
    ui.vertical_centered(|ui| {
        // No fill and no corner radius: `Frame` is here only to give the column
        // a width for `vertical_centered` to centre, the way the composer's
        // frame is used. `Frame::show` inherits `vertical_centered`'s layout,
        // which centres across the column — fine for a single block, wrong for a
        // stack of them, hence the explicit `vertical` that left-aligns.
        Frame::NONE.show(ui, |ui| {
            ui.set_width(max_width);
            ui.vertical(|ui| {
                markdown::draw_text(ui, p, text, salt);
            });
        });
    });
    ui.add_space(8.0);
}

/// One message, as a bubble of rendered Markdown.
///
/// `side` is where the bubble sits: the user's turns go `Align::Max` so they
/// hang off the right, and the system's notices `Align::Center`. Nothing uses
/// `Align::Min` now that the agent's replies have stopped being bubbles, but it
/// is still the honest answer for "hug the left" and costs one arm.
///
/// `salt` is passed straight to the renderer, which uses it to name the scroll
/// area of each code block the message holds.
///
/// The bubble shrinks to its content. egui will not do that on its own — a
/// frame on a right-to-left row is handed the whole row (measured: a two-word
/// message drew 576 px wide) — so the width is measured first with an invisible
/// pass, then the visible bubble is allocated exactly that much, aligned to
/// `side`. The measuring pass is given twice the band so it lays the message
/// out *unwrapped*: what comes back is the width the content wants, not the
/// width a line happened to wrap to. A message too wide to fit still measures
/// over the band and is clamped to it, so it fills the column the way a long
/// reply does instead of shrinking to its longest line.
///
/// Returns the visible bubble's rect, for tests.
pub(super) fn draw_bubble(
    ui: &mut egui::Ui,
    p: &Palette,
    text: &str,
    salt: (Uuid, usize),
    fill: Color32,
    side: Align,
    max_width: f32,
) -> egui::Rect {
    // The band the bubble may draw in. The gap is only spent on the side the
    // bubble hugs; the agent's column already keeps the user's turns off the
    // left, and a centred notice is away from both edges by construction.
    let band = match side {
        Align::Max => max_width - BUBBLE_EDGE_GAP,
        _ => max_width,
    };
    let band_rect = egui::Rect::from_min_size(
        egui::pos2(ui.cursor().left(), ui.cursor().top()),
        egui::vec2(band, f32::INFINITY),
    );

    // Pass one, invisible and measure-only: the bubble with room to spare, so
    // the message lays out on one line and the result is its natural width.
    // The salt is suffixed so this pass cannot share scroll-area state with the
    // visible one — only the visible bubble's scroll offsets should stick.
    let measured = {
        let mut probe = ui.new_child(
            egui::UiBuilder::new()
                .invisible()
                .sizing_pass()
                .max_rect(egui::Rect::from_min_size(
                    band_rect.min,
                    egui::vec2(band * 2.0, f32::INFINITY),
                ))
                .layout(Layout::left_to_right(Align::Min)),
        );
        Frame::NONE
            .fill(fill)
            .corner_radius(CornerRadius::same(14))
            .inner_margin(Margin::symmetric(BUBBLE_PADDING_X as i8, 9))
            .show(&mut probe, |frame| {
                frame.vertical(|ui| {
                    markdown::draw_text(ui, p, text, (salt, "measure"));
                });
            });
        // A message wider than the band is clamped to it; rounding inside the
        // frame's margins can also leave the measurement a hair over, and a
        // bubble wider than its band would spill past the column edge.
        probe.min_rect().width().min(band)
    };

    // Pass two: the visible bubble, in a rect exactly `measured` wide, aligned
    // to its side of the band.
    let x = match side {
        Align::Max => band_rect.left() + band - measured,
        Align::Center => band_rect.left() + (band - measured) / 2.0,
        _ => band_rect.left(),
    };
    let rect = egui::Rect::from_min_max(
        egui::pos2(x, band_rect.top()),
        egui::pos2(x + measured, band_rect.bottom()),
    );
    let mut drawn = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Min)),
    );
    Frame::NONE
        .fill(fill)
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(BUBBLE_PADDING_X as i8, 9))
        .show(&mut drawn, |frame| {
            // Hold the bubble at the width that was measured. A long message
            // re-wraps a little narrower here than it did while measuring, and
            // without the floor that would shrink the frame — and with it the
            // bubble's right edge, which is the one thing a right-hugging
            // bubble promises.
            frame.set_min_width(measured - 2.0 * BUBBLE_PADDING_X);
            frame.vertical(|ui| {
                markdown::draw_text(ui, p, text, salt);
            });
        });
    // Hand the vertical space the bubble occupied back to the caller's layout.
    ui.advance_cursor_after_rect(egui::Rect::from_min_size(
        egui::pos2(band_rect.left(), band_rect.top()),
        egui::vec2(band, drawn.min_rect().height()),
    ));
    ui.add_space(8.0);
    drawn.min_rect()
}

/// One tool call, collapsed by default.
///
/// The collapsed row carries no bubble fill: a run of calls should read as a
/// log, not as a stack of cards. Expanding it hands the body to `code_view`,
/// which shapes it by tool — a patch becomes a diff, an `exec` becomes a
/// terminal, anything else is plain output.
///
/// The row's glyph is the *outcome* (✓ / ⚠ / ✕) rather than the tool's own, so
/// a failure is visible without expanding anything; the tool's own glyph heads
/// the panel instead.
fn draw_tool_card(
    ui: &mut egui::Ui,
    p: &Palette,
    call_id: &str,
    name: &str,
    arguments: &Value,
    result: Option<&ToolResult>,
    max_width: f32,
) {
    let accent = match result {
        None => p.text_muted,
        Some(result) => outcome_colour(result.outcome),
    };

    // The same centred column the replies use: left to itself the row hugs the
    // transcript's edge, and a run of calls reads as a second flow beside the
    // prose it belongs to. The expanded panel fills the column — still slab
    // wide for a diff, just not wider than the conversation it edits.
    ui.vertical_centered(|ui| {
        Frame::NONE.show(ui, |ui| {
            ui.set_width(max_width);
            egui::CollapsingHeader::new(header_job(p, name, arguments, result, accent))
                .id_salt(call_id)
                .default_open(false)
                .show_background(false)
                .show(ui, |ui| {
                    let spec = tool_panel(name, arguments, result, accent);
                    code_view::draw(ui, p, call_id, &spec, max_width);

                    // A tool this build does not know gets a panel like any
                    // other, but nothing on screen then says what it was
                    // *asked* to do, so its arguments stay reachable.
                    if !code_view::is_known(name) {
                        egui::CollapsingHeader::new(
                            RichText::new("参数")
                                .size(theme::font(11.0))
                                .color(p.text_muted),
                        )
                        .id_salt((call_id, "arguments"))
                        .default_open(false)
                        .show_background(false)
                        .show(ui, |ui| selectable_code(ui, &pretty(arguments)));
                    }
                });
        });
    });
    ui.add_space(8.0);
}

/// The collapsed row as one multi-coloured galley: status glyph, verb, then a
/// dimmed summary and duration.
///
/// A `LayoutJob` rather than a `RichText`, because the parts are coloured
/// differently — and still a `CollapsingHeader`, rather than a hand-built row,
/// so the caret, the open/close animation and the accessibility node stay
/// egui's problem.
fn header_job(
    p: &Palette,
    name: &str,
    arguments: &Value,
    result: Option<&ToolResult>,
    accent: Color32,
) -> LayoutJob {
    let font = FontId::proportional(theme::font(13.0));
    // Dimmer than `text_muted`: the summary is a hint, not a label, and the row
    // has to stay quiet next to the transcript's prose.
    let dim = p.text_muted.gamma_multiply(0.72);
    let mut job = LayoutJob::default();

    let glyph = match result {
        None => icons::SPINNER_GAP,
        Some(result) => result.outcome.icon(),
    };
    append_run(&mut job, &format!("{glyph}  "), &font, accent);
    append_run(&mut job, code_view::tool_label(name), &font, p.text_muted);
    if let Some(summary) = summarize(name, arguments) {
        append_run(&mut job, "  ", &font, dim);
        append_run(&mut job, &shorten(&summary, 64), &font, dim);
    }
    if let Some(result) = result {
        append_run(
            &mut job,
            &format!("  ·  {} ms", result.duration_ms),
            &font,
            dim,
        );
    }

    job
}

fn append_run(job: &mut LayoutJob, text: &str, font: &FontId, colour: Color32) {
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: font.clone(),
            color: colour,
            ..Default::default()
        },
    );
}

/// Shapes the expanded body for the tool that produced it.
///
/// Known tools do not repeat their arguments: a patch's are the diff, an
/// `exec`'s are the prompt line. `draw_tool_card` keeps the raw JSON for the
/// tools that fall through to the last arm.
fn tool_panel(
    name: &str,
    arguments: &Value,
    result: Option<&ToolResult>,
    accent: Color32,
) -> code_view::PanelSpec {
    let running = result.is_none();
    let output = result
        .map(|result| result.output.as_str())
        .unwrap_or_default();
    let icon = code_view::tool_icon(name);

    match name {
        "apply_patch" => {
            let patch = arguments
                .get("patch")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (added, removed) = code_view::patch_stats(patch);
            // The numbers in `hunks` were resolved when the tool read the
            // target file, so they are the file's real line numbers. A result
            // from before that field existed (or a still-running call) has
            // none, and falls back to counting within the patch.
            // One flat table across the whole patch: `hunks` records a section
            // per file operation in the order the patch lists them, and the
            // body's hunk lines draw from the table in that same order.
            let numbers = result.map(|result| {
                result
                    .hunks
                    .iter()
                    .flat_map(|section| section.lines.iter().copied())
                    .collect::<Vec<_>>()
            });
            let mut lines = code_view::patch_lines(patch, numbers.as_deref());
            // The tool's own report names the paths it resolved, which the
            // patch's relative paths do not, so it rides along as a footer
            // rather than being dropped.
            lines.extend(code_view::meta_lines(output));
            code_view::PanelSpec {
                title: shorten(&code_view::patch_title(patch), 80),
                icon,
                added,
                removed,
                lines,
                copy: patch.to_string(),
                accent,
                running,
            }
        }
        "exec" => {
            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            code_view::PanelSpec {
                title: "Shell".to_string(),
                icon,
                added: 0,
                removed: 0,
                lines: code_view::command_lines(command, output),
                copy: output.to_string(),
                accent,
                running,
            }
        }
        // `read_file`, `list_dir`, `read_image` — and any tool this build has
        // never heard of, which then titles itself with its own name.
        _ => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(name);
            code_view::PanelSpec {
                title: shorten(path, 80),
                icon,
                added: 0,
                removed: 0,
                lines: code_view::text_lines(output),
                copy: output.to_string(),
                accent,
                running,
            }
        }
    }
}

fn outcome_colour(outcome: AuditOutcome) -> Color32 {
    match outcome {
        AuditOutcome::Executed => OK_GREEN,
        AuditOutcome::Denied => WARN_AMBER,
        AuditOutcome::Failed => BAD_RED,
    }
}

/// Monospace text the user can select and copy, which is what makes a path or a
/// stack trace in a tool result usable.
fn selectable_code(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(RichText::new(text).monospace().size(theme::font(12.0)))
            .wrap()
            .selectable(true),
    );
}

/// One queued image, as a chip with a button that drops it.
///
/// Returns the remove button's response, so the caller can act on the click.
/// `id` salts the widgets: the strip is drawn from a loop over the queue, and
/// without it two chips would share an egui id and one button would answer for
/// the other.
fn remove_chip(ui: &mut egui::Ui, p: &Palette, id: &str, image: &ImageRef) -> egui::Response {
    ui.push_id(id, |ui| {
        Frame::NONE
            .fill(p.hover_bg)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(8, 3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(icons::IMAGE)
                            .size(theme::font(12.0))
                            .color(p.text_muted),
                    );
                    ui.label(
                        RichText::new(chip_label(image))
                            .size(theme::font(12.0))
                            .color(p.text),
                    );
                    ui.add(
                        egui::Button::new(RichText::new(icons::X).size(theme::font(11.0)))
                            .frame(false),
                    )
                    .on_hover_text("移除")
                })
                .inner
            })
            .inner
    })
    .inner
}
