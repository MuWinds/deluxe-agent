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
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use egui::text::LayoutJob;
use egui::{
    Align, Color32, ColorImage, CornerRadius, FontId, Frame, Layout, Margin, Pos2, RichText,
    Stroke, TextFormat, TextureHandle, TextureOptions, Vec2,
};
use serde_json::Value;
use uuid::Uuid;

use crate::attachments::{self, ImageRef};
use crate::config::InputModality;
use crate::icons;
use crate::image_ops;
use crate::ipc::{AuditOutcome, JobState, JobView, RunState};
use crate::llm::ThinkingLevel;
use crate::plugins::{self};
use crate::renderer::present;
use crate::renderer::protocol::{
    HunkLines, Node, RenderKey, RenderMetrics, ToolRenderRequest, ToolRenderResult,
};
use crate::session::{self, Session, Step, ToolResult};
use crate::theme::{self, Palette, ThemeChoice};

use super::render_cache;

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

/// The most job rows the composer draws. Running jobs are always kept; settled
/// ones fill the rest, newest first, so an old job cannot push out a live one.
const MAX_JOB_ROWS: usize = 5;

impl App {
    /// Draws one frame.
    ///
    /// Intake and polling run before the draw pass so this frame reflects the
    /// freshest state, and the deferred [`UiIntent`] values are applied after it, once
    /// the borrows the widgets held have been released.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        _frame: &mut eframe::Frame,
        resources: &mut GuiResources,
    ) {
        let p = theme::palette(self.config.theme);
        let ctx = ui.ctx().clone();
        let mut intents = Vec::new();

        // Image intake is a whole-frame concern, not the composer widget's: the
        // chord works wherever the focus is, and a file can be dropped onto any
        // panel. Run before the draw pass so the chips appear this frame.
        self.intake_pasted_images(&ctx);
        self.intake_dropped_files(&ctx);

        // The composer's task list is a poll against the worker, throttled and
        // kept awake only while something is live. Before the draw pass so the
        // list reflects the freshest reply this frame.
        if let Some(intent) = self.poll_jobs_intent() {
            intents.push(intent);
        }

        self.draw_menu_bar(ui, &p, &mut intents);
        self.draw_rail(ui, &p, &mut intents);
        if self.show_sidebar {
            self.draw_sidebar(ui, &p, &mut intents);
        }
        self.draw_main(ui, &p, &mut intents, resources);

        self.draw_settings(&ctx, &p, &mut intents);
        self.draw_about(&ctx, &p);
        self.draw_plugins(&ctx, &mut intents);
        super::plugin_ui::draw(&ctx, self, &mut intents);
        self.draw_subagent_window(&ctx, &p, resources, &mut intents);

        let effects = self.apply_intents(intents);
        if effects.close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if let Some(theme) = effects.theme {
            theme::apply(&ctx, theme);
        }
        if let Some(text) = effects.clipboard_text {
            ctx.copy_text(text);
        }
        if effects.repaint_after {
            ctx.request_repaint_after(JOBS_POLL_INTERVAL);
        }
    }

    /// The top menu bar: 文件 / 编辑 / 视图 / 帮助.
    fn draw_menu_bar(&mut self, ui: &mut egui::Ui, p: &Palette, intents: &mut Vec<UiIntent>) {
        let mut theme_choice = self.config.theme;
        // Seeded from the real state, not `false`: a checkbox bound to a local
        // that always starts out unchecked shows the wrong thing every frame.
        let mut show_sidebar = self.show_sidebar;
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
                            intents.push(UiIntent::NewSession);
                            ui.close();
                        }
                        if ui.button("设置").clicked() {
                            intents.push(UiIntent::OpenSettings);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("退出").clicked() {
                            intents.push(UiIntent::Quit);
                            ui.close();
                        }
                    });

                    ui.menu_button("编辑", |ui| {
                        if ui
                            .add_enabled(has_session, egui::Button::new("复制会话正文"))
                            .clicked()
                        {
                            intents.push(UiIntent::CopyTranscript);
                            ui.close();
                        }
                        if ui
                            .add_enabled(has_session, egui::Button::new("删除当前会话"))
                            .clicked()
                        {
                            if let Some(id) = self.selected {
                                intents.push(UiIntent::DeleteSession(id));
                            }
                            ui.close();
                        }
                    });

                    ui.menu_button("视图", |ui| {
                        for choice in [ThemeChoice::Dark, ThemeChoice::Light] {
                            if ui
                                .selectable_value(&mut theme_choice, choice, choice.label())
                                .clicked()
                            {
                                intents.push(UiIntent::SetTheme(choice));
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.checkbox(&mut show_sidebar, "显示侧边栏").changed() {
                            intents.push(UiIntent::SetSidebarVisible(show_sidebar));
                        }
                    });

                    ui.menu_button("帮助", |ui| {
                        if ui.button("关于").clicked() {
                            intents.push(UiIntent::OpenAbout);
                            ui.close();
                        }
                    });
                });
            });
    }

    /// The far-left icon rail: home, plugins, settings, about, quit.
    fn draw_rail(&mut self, ui: &mut egui::Ui, p: &Palette, intents: &mut Vec<UiIntent>) {
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
                        intents.push(UiIntent::NewSession);
                    }
                    // What is installed: a plugin is only visible today through
                    // a slash command in the picker, which is no way to answer
                    // "did the one I just enabled load?".
                    if rail_button(ui, icons::PUZZLE_PIECE, plugins_open, "插件").clicked() {
                        intents.push(UiIntent::OpenPlugins);
                    }
                });

                // Laid out bottom-up so the pair stays pinned however tall the
                // window is.
                ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                    if rail_button(ui, icons::GEAR, settings_open, "设置").clicked() {
                        intents.push(UiIntent::OpenSettings);
                    }
                    if rail_button(ui, icons::QUESTION, about_open, "关于").clicked() {
                        intents.push(UiIntent::OpenAbout);
                    }
                });
            });
    }

    /// The session sidebar: search, projects, recent conversations.
    fn draw_sidebar(&mut self, ui: &mut egui::Ui, p: &Palette, intents: &mut Vec<UiIntent>) {
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
                    intents.push(UiIntent::NewSession);
                }
                if sidebar_row(ui, p, icons::FOLDER_SIMPLE, "新增项目", false, false).clicked()
                {
                    intents.push(UiIntent::AddProject);
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
                            intents.push(UiIntent::SelectSession(session.id));
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
                        section_label(ui, p, "项目");
                        // A project list that could not be written is reported
                        // here, beside the projects it is about.
                        if let Some(error) = &sidebar_error {
                            ui.label(
                                RichText::new(error)
                                    .size(theme::font(11.0))
                                    .color(theme::BAD_RED),
                            );
                        }
                        for project in &projects {
                            let name = project_name(project);
                            let is_open = expanded.as_deref() == Some(project.as_str());
                            let response =
                                sidebar_row(ui, p, icons::FOLDER_SIMPLE, &name, is_open, true);
                            if response.clicked() {
                                intents.push(UiIntent::SelectProject(project.clone()));
                            }
                            response.context_menu(|ui| {
                                if ui.button("复制路径").clicked() {
                                    ui.ctx().copy_text(project.clone());
                                    ui.close();
                                }
                                if ui.button("移除项目").clicked() {
                                    intents.push(UiIntent::RemoveProject(project.clone()));
                                    ui.close();
                                }
                            });

                            if is_open {
                                for session in
                                    sessions.iter().filter(|s| &s.project == project).rev()
                                {
                                    let response = session_row(ui, p, session, selected, now, true);
                                    if response.clicked() {
                                        intents.push(UiIntent::SelectSession(session.id));
                                    }
                                    response.context_menu(|ui| {
                                        if ui.button("删除").clicked() {
                                            intents.push(UiIntent::DeleteSession(session.id));
                                            ui.close();
                                        }
                                    });
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
                                intents.push(UiIntent::SelectSession(session.id));
                            }
                            response.context_menu(|ui| {
                                if ui.button("删除").clicked() {
                                    intents.push(UiIntent::DeleteSession(session.id));
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
    fn draw_main(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        intents: &mut Vec<UiIntent>,
        resources: &mut GuiResources,
    ) {
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(p.main_bg))
            .show(ui, |ui| {
                self.draw_composer(ui, p, intents);
                self.draw_transcript(ui, p, resources);
            });
    }

    /// The scrolling transcript for the open session, or the empty state.
    ///
    /// Draws a `Step::Reasoning` through [`draw_reasoning_block`] so the fold can
    /// be toggled, and every other step through [`draw_step`]; the salt folds in
    /// the session id so a code block's scroll offset cannot leak between
    /// sessions.
    /// Decides, for every visible step, whether the renderer's IR is ready and
    /// queues a request for the ones whose input changed.
    ///
    /// Runs before the transcript borrows the session, so it can send commands
    /// and touch the cache. A step whose render is pending or failed is simply
    /// left out of the result and drawn with the fallback.
    fn collect_rendered(
        &mut self,
        session_id: Uuid,
        index: usize,
        metrics: RenderMetrics,
    ) -> Rendered {
        let mut rendered = Rendered::default();
        if !self.renderer_available {
            return rendered;
        }
        // The layout width is part of the input: the renderer picks a table's
        // form from it, so a resize has to invalidate the cached list. Bucketed
        // to 16 px so a drag does not re-render on every pixel.
        let bucket = (metrics.available_width / 16.0).round() as u32;
        let mut requests = Vec::new();
        {
            let steps = &self.sessions[index].steps;
            for (step_index, step) in steps.iter().enumerate() {
                match step {
                    Step::Assistant { text }
                    | Step::Notice { text }
                    | Step::HostMessage { text } => {
                        let key = RenderKey::Message {
                            session: session_id,
                            step: step_index,
                        };
                        let fp = render_cache::fingerprint((text, bucket));
                        match self.render_cache.rendered(&key, fp) {
                            Some(nodes) => {
                                rendered.message.insert((session_id, step_index), nodes);
                            }
                            None if !self.render_cache.was_requested(&key, fp) => {
                                requests.push(PendingRender::Message {
                                    key,
                                    fp,
                                    text: text.clone(),
                                    metrics,
                                });
                            }
                            None => {}
                        }
                    }
                    Step::Tool {
                        call_id,
                        name,
                        arguments,
                        result,
                        ..
                    } => {
                        let key = RenderKey::Tool {
                            session: session_id,
                            call_id: call_id.clone(),
                        };
                        let fp = render_cache::fingerprint((
                            tool_fingerprint(name, arguments, result.as_ref()),
                            bucket,
                        ));
                        match self.render_cache.rendered(&key, fp) {
                            Some(nodes) => {
                                rendered.tool.insert((session_id, call_id.clone()), nodes);
                            }
                            None if !self.render_cache.was_requested(&key, fp) => {
                                requests.push(PendingRender::Tool {
                                    key,
                                    fp,
                                    name: name.clone(),
                                    arguments: arguments.clone(),
                                    result: result.clone(),
                                    metrics,
                                });
                            }
                            None => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        for request in requests {
            self.dispatch_render(request);
        }
        rendered
    }

    /// Sends one render request at the next revision for its key.
    fn dispatch_render(&mut self, request: PendingRender) {
        match request {
            PendingRender::Message {
                key,
                fp,
                text,
                metrics,
            } => {
                let revision = self.next_revision(&key);
                self.render_cache.begin(&key, revision, fp);
                let _ = self.cmd_tx.send(Cmd::RenderMessage {
                    key,
                    revision,
                    text,
                    metrics,
                });
            }
            PendingRender::Tool {
                key,
                fp,
                name,
                arguments,
                result,
                metrics,
            } => {
                let revision = self.next_revision(&key);
                self.render_cache.begin(&key, revision, fp);
                let request =
                    ToolRenderRequest::new(revision, name, arguments, tool_result(result), metrics);
                let _ = self.cmd_tx.send(Cmd::RenderTool {
                    key,
                    revision,
                    request,
                });
            }
        }
    }

    fn next_revision(&self, key: &RenderKey) -> u64 {
        self.render_cache
            .get(key)
            .map(|entry| entry.latest_revision)
            .unwrap_or(0)
            + 1
    }

    pub(super) fn draw_transcript(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        resources: &mut GuiResources,
    ) {
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
        // Salt for the renderer's scroll areas. The session id is in it because a
        // step index is only unique within one session, and a code block that
        // inherited another session's scroll offset would open scrolled to a
        // line that is not in it.
        let session_id = self.sessions[index].id;
        // The renderer has no fonts, so it is told the column width and the
        // advance of one character; it decides a table's form from them.
        let metrics = render_metrics(ui, ui.available_width() - 2.0 * CHAT_MARGIN_X);
        // Decide renderer-vs-fallback and queue requests before the session is
        // borrowed immutably below.
        let rendered = self.collect_rendered(session_id, index, metrics);
        let steps = &self.sessions[index].steps;
        let expanded_reasoning = &mut self.expanded_reasoning;
        // Same story as `expanded_reasoning`: a thumbnail decode on miss writes
        // into the cache while the steps are still being read.
        let thumbs = &mut resources.thumbs;

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
                            _ => draw_step(ui, p, step, salt, max_width, thumbs, &rendered),
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

    /// The composer: the input row and the queued images.
    fn draw_composer(&mut self, ui: &mut egui::Ui, p: &Palette, intents: &mut Vec<UiIntent>) {
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

                            ui.horizontal(|ui| {
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
                                // Enter sends while the editor has focus.
                                // Shift+Enter still breaks the line.
                                let enter = ui.input(|input| {
                                    input.key_pressed(egui::Key::Enter) && !input.modifiers.shift
                                });
                                if editor.has_focus() && enter {
                                    intents.push(UiIntent::SendPrompt);
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
                                        intents.push(UiIntent::CancelRun);
                                    }
                                } else if ui
                                    .add_enabled(can_send, circle_button(icons::ARROW_UP, p))
                                    .on_hover_text("发送")
                                    .clicked()
                                {
                                    intents.push(UiIntent::SendPrompt);
                                }
                            });

                            // The queued images get their own row under the
                            // input. Drawn inside the row above they would fight
                            // the editor for its width budget — the editor takes
                            // almost all of it — and push Send off the right
                            // edge of the window.
                            if !self.pending_images.is_empty() {
                                self.draw_pending_image_strip(ui, p, intents);
                            }
                        });

                    // The background-task / sub-agent list. It takes the
                    // status line's place: instead of a one-shot "已提交", the
                    // composer shows what is actually running — and nothing at
                    // all when nothing is.
                    self.draw_jobs(ui, p, intents);
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
    fn draw_jobs(&mut self, ui: &mut egui::Ui, p: &Palette, intents: &mut Vec<UiIntent>) {
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
            intents.push(UiIntent::KillJob(job_id));
        }
        if let Some(job_id) = open {
            intents.push(UiIntent::ToggleSubagent(job_id));
        }
    }

    /// The window showing one sub-agent's own conversation.
    ///
    /// This is what forwarding a delegated agent's events buys: its reasoning,
    /// tool calls and answer are readable while they happen, and none of them
    /// enters the parent's context. The steps render through the same
    /// [`draw_step`] the transcript uses, so a tool card looks the same here as
    /// it does in a conversation.
    fn draw_subagent_window(
        &mut self,
        ctx: &egui::Context,
        p: &Palette,
        resources: &mut GuiResources,
        intents: &mut Vec<UiIntent>,
    ) {
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
        let thumbs = &mut resources.thumbs;

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
                                    .color(theme::OK_GREEN),
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
                                _ => draw_step(
                                    ui,
                                    p,
                                    step,
                                    step_salt,
                                    max_width,
                                    thumbs,
                                    &Rendered::default(),
                                ),
                            }
                        }
                    });
            });

        if stop {
            intents.push(UiIntent::KillJob(job_id.clone()));
        }
        if !open {
            intents.push(UiIntent::CloseSubagent);
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
    /// Deliberately not a button: it is a gauge. The image intake is Ctrl+V and
    /// drag-and-drop only, so there is no mouse path to duplicate here.
    fn draw_context_gauge(&self, ui: &mut egui::Ui, p: &Palette) {
        let session = self.selected_session();
        let measured = session
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
            theme::BAD_RED
        } else if ratio >= warning {
            theme::WARN_AMBER
        } else {
            theme::OK_GREEN
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

        // The cache hit rate rides the same tooltip. The most recent request's
        // share first, then the conversation's token-weighted share; both are
        // omitted for a provider that never reported a cache figure, so the
        // line never claims a rate it cannot back.
        let mut cache = Vec::new();
        if let Some(rate) = session
            .and_then(|session| session.usage.as_ref())
            .and_then(|usage| usage.cache_hit_rate())
        {
            cache.push(format!("最近一次 {:.0}%", rate * 100.0));
        }
        if let Some(rate) = session.and_then(Session::cache_hit_rate) {
            cache.push(format!("本会话 {:.0}%", rate * 100.0));
        }
        if !cache.is_empty() {
            summary.push_str(&format!("\n缓存命中：{}", cache.join("，")));
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
    fn draw_pending_image_strip(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        intents: &mut Vec<UiIntent>,
    ) {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for index in (0..self.pending_images.len()).rev() {
                let id = format!("pending-image-{}", self.pending_images[index].id);
                if remove_chip(ui, p, &id, &self.pending_images[index]).clicked() {
                    intents.push(UiIntent::RemovePendingImage(
                        self.pending_images[index].id.clone(),
                    ));
                }
            }
        });
    }

    /// The 设置 window: endpoint, key, context and safety settings.
    fn draw_settings(&mut self, ctx: &egui::Context, p: &Palette, intents: &mut Vec<UiIntent>) {
        if !self.show_settings {
            return;
        }

        let mut open = true;
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

                        ui.label("保留最近轮次");
                        ui.horizontal(|ui| {
                            // How many recent user turns compaction keeps
                            // verbatim; the rest is folded into the brief.
                            ui.add(
                                egui::DragValue::new(&mut self.config.context.keep_recent_turns)
                                    .speed(1.0)
                                    .range(0..=20),
                            );
                            ui.weak("压缩时逐字保留的最近用户轮次");
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
                    intents.push(UiIntent::SaveSettings);
                }

                // Where a save failure is shown: on the page whose save failed,
                // not in some shared corner of the window.
                if let Some(error) = &self.settings_error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(error).color(theme::BAD_RED));
                }
            });

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
                path_row("配置文件", self.paths.config_path.clone());
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
    /// Lists the global plugins and the plugins assigned to the displayed
    /// project. Each row carries its exact scope so equal ids cannot operate on
    /// the wrong installation.
    fn draw_plugins(&mut self, ctx: &egui::Context, intents: &mut Vec<UiIntent>) {
        if !self.show_plugins {
            return;
        }

        // Taken out of `self` for the duration so the window can be drawn
        // against a borrow of the catalogue; written back below. The buttons
        // land in `actions` and are applied once every borrow has been released,
        // the same shape the rest of the app uses.
        let mut pending = self.pending_uninstall.clone();
        let project = self.displayed_project();

        let mut open = true;
        egui::Window::new("插件")
            .open(&mut open)
            .default_width(620.0)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.weak("所有插件都通过 Wasmtime Component 加载。");
                    let refreshing = self.pending_plugin_request.is_some();
                    let icon = if refreshing {
                        icons::SPINNER_GAP
                    } else {
                        icons::ARROW_CLOCKWISE
                    };
                    if ui
                        .add_enabled(
                            !refreshing,
                            egui::Button::new(RichText::new(icon).size(theme::font(15.0))),
                        )
                        .on_hover_text("重新扫描并加载 Wasmtime 插件")
                        .clicked()
                    {
                        intents.push(UiIntent::RefreshPlugins);
                    }
                });
                ui.horizontal(|ui| {
                    let refreshing = self.pending_plugin_request.is_some();
                    if ui
                        .add_enabled(
                            !refreshing,
                            egui::Button::new(format!("{} 添加到全局", icons::PLUS)),
                        )
                        .on_hover_text("选择插件包中的 .wasm Component 并安装到全局")
                        .clicked()
                    {
                        intents.push(UiIntent::AddPlugin {
                            scope: plugins::Scope::Global,
                        });
                    }
                    if ui
                        .add_enabled(
                            !refreshing,
                            egui::Button::new(format!("{} 添加到当前项目", icons::PLUS)),
                        )
                        .on_hover_text("选择插件包中的 .wasm Component 并安装到当前项目")
                        .clicked()
                    {
                        intents.push(UiIntent::AddPlugin {
                            scope: plugins::Scope::Project(PathBuf::from(&project)),
                        });
                    }
                });
                if self.pending_plugin_request.is_some() {
                    ui.weak("正在导入 Wasmtime Component…");
                }

                // Where a plugin action's failure is shown: on the page that
                // performed it.
                if let Some(error) = &self.plugins_error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(error).color(theme::BAD_RED));
                }
                ui.add_space(6.0);

                let surfaces =
                    super::view_model::plugin_surfaces(&self.catalogue, Path::new(&project));
                if !surfaces.is_empty() {
                    ui.label(
                        RichText::new(format!("项目界面 · {}", project_name(&project))).strong(),
                    );
                    for surface in surfaces {
                        if ui
                            .button(format!("{} · {}", surface.display_name, surface.surface_id))
                            .clicked()
                        {
                            intents.push(UiIntent::OpenPluginSurface {
                                plugin_id: surface.plugin_id,
                                surface_id: surface.surface_id,
                            });
                        }
                    }
                    ui.separator();
                }
                let global_enabled = self.catalogue.global();
                let global_disabled = self.catalogue.disabled();
                let project_enabled = self.catalogue.project(Path::new(&project));
                let project_disabled = self.catalogue.disabled_for_project(Path::new(&project));

                if global_enabled.is_empty()
                    && global_disabled.is_empty()
                    && project_enabled.is_empty()
                    && project_disabled.is_empty()
                {
                    ui.weak("没有已安装的插件。");
                    ui.add_space(4.0);
                    return;
                }

                if !global_enabled.is_empty() {
                    ui.label(
                        RichText::new(format!("全局已启用（{}）", global_enabled.len())).strong(),
                    );
                    for plugin in global_enabled {
                        draw_plugin_row(ui, plugin, true, &mut pending, intents);
                    }
                }

                if !global_disabled.is_empty() {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("全局已停用（{}）", global_disabled.len())).strong(),
                    );
                    for plugin in global_disabled {
                        draw_plugin_row(ui, plugin, false, &mut pending, intents);
                    }
                }

                if !project_enabled.is_empty() || !project_disabled.is_empty() {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("项目已启用 · {}", project_name(&project))).strong(),
                    );
                    for plugin in project_enabled {
                        draw_plugin_row(ui, plugin, true, &mut pending, intents);
                    }
                    if !project_disabled.is_empty() {
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!("项目已停用 · {}", project_name(&project)))
                                .strong(),
                        );
                        for plugin in project_disabled {
                            draw_plugin_row(ui, plugin, false, &mut pending, intents);
                        }
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
    pending: &mut Option<(String, plugins::Scope)>,
    intents: &mut Vec<UiIntent>,
) {
    let version = plugin.manifest.version.as_deref().unwrap_or("版本未知");
    let title = format!("{}  {version}", plugin.display_name());
    let header = if enabled {
        RichText::new(title).strong()
    } else {
        RichText::new(title).weak()
    };

    egui::CollapsingHeader::new(header)
        .id_salt((&plugin.id, &plugin.scope))
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
            ui.horizontal(|ui| {
                ui.weak("作用域");
                match &plugin.scope {
                    plugins::Scope::Global => ui.label("全局"),
                    plugins::Scope::Project(project) => {
                        ui.monospace(format!("项目 {}", project.display()))
                    }
                };
            });

            draw_plugin_contents(ui, plugin);

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(if enabled { "停用" } else { "启用" }).clicked() {
                    intents.push(UiIntent::SetPluginEnabled {
                        id: plugin.id.clone(),
                        scope: plugin.scope.clone(),
                        enabled: !enabled,
                    });
                }
                if ui.button("卸载").clicked() {
                    *pending = Some((plugin.id.clone(), plugin.scope.clone()));
                }
            });

            let is_pending = pending
                .as_ref()
                .is_some_and(|(id, scope)| id == &plugin.id && scope == &plugin.scope);
            if is_pending {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("确认卸载？").color(theme::BAD_RED));
                    if ui.button("确认").clicked() {
                        intents.push(UiIntent::UninstallPlugin {
                            id: plugin.id.clone(),
                            scope: plugin.scope.clone(),
                        });
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

/// What one plugin declared, listed by capability.
///
/// A plugin that declares nothing still says so, so an empty body is never
/// mistaken for a failed load.
fn draw_plugin_contents(ui: &mut egui::Ui, plugin: &plugins::LoadedPlugin) {
    let Some(runtime) = plugin.manifest.wasm_runtime() else {
        ui.weak("插件没有有效的 Wasmtime runtime");
        return;
    };
    let mut sections: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut any = false;
    ui.horizontal_wrapped(|ui| {
        ui.weak("运行时：");
        ui.label(RichText::new("Wasmtime").strong());
    });

    let mut capabilities = Vec::new();
    if !runtime.permissions.invoke_tools.is_empty() {
        capabilities.push("工具");
    }
    if !runtime.ui.surfaces.is_empty() {
        capabilities.push("插件界面");
    }
    if !capabilities.is_empty() {
        sections.push(("能力", capabilities));
    }

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
        ui.weak("Wasmtime 插件没有声明式内容或 runtime capability");
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
        JobState::Running => theme::OK_GREEN,
        JobState::Stopping => theme::WARN_AMBER,
        JobState::Completed => p.text_muted,
        JobState::Killed => theme::WARN_AMBER,
        JobState::Failed => theme::BAD_RED,
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
                theme::BAD_RED
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

fn circle_button<'a>(icon: &'a str, p: &Palette) -> egui::Button<'a> {
    egui::Button::new(RichText::new(icon).size(theme::font(15.0)).color(p.main_bg))
        .fill(p.text)
        .corner_radius(CornerRadius::same(COMPOSER_BUTTON as u8 / 2))
        .min_size(Vec2::splat(COMPOSER_BUTTON))
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

/// The renderer IR available for the visible steps, keyed the same way the draw
/// loop keys its salts.
#[derive(Default)]
struct Rendered {
    message: HashMap<(Uuid, usize), Arc<Vec<Node>>>,
    tool: HashMap<(Uuid, String), Arc<Vec<Node>>>,
}

/// One render request waiting to be sent.
enum PendingRender {
    Message {
        key: RenderKey,
        fp: u64,
        text: String,
        metrics: RenderMetrics,
    },
    Tool {
        key: RenderKey,
        fp: u64,
        name: String,
        arguments: Value,
        result: Option<ToolResult>,
        metrics: RenderMetrics,
    },
}

/// The measurements the renderer needs but cannot take itself.
///
/// `width` is the message column before clamping; `char_width` is measured from
/// the body font so the guest's width arithmetic tracks the host's type scale.
fn render_metrics(ui: &egui::Ui, width: f32) -> RenderMetrics {
    let font = FontId::proportional(theme::font(14.0));
    let char_width = ui
        .painter()
        .layout_no_wrap("0".to_string(), font, Color32::PLACEHOLDER)
        .size()
        .x;
    // A non-finite width would serialize as `null` and the guest would reject
    // the request; fall back to the column's own maximum instead.
    let width = if width.is_finite() {
        width
    } else {
        COMPOSER_MAX_WIDTH
    };
    RenderMetrics {
        available_width: width.clamp(0.0, COMPOSER_MAX_WIDTH),
        char_width: char_width.max(1.0),
        column_gap: 12.0,
    }
}

/// Hashes a tool call's panel-relevant input.
fn tool_fingerprint(name: &str, arguments: &Value, result: Option<&ToolResult>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    arguments.to_string().hash(&mut hasher);
    if let Some(result) = result {
        serde_json::to_string(result)
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// Projects a host tool result onto the wire, dropping images.
fn tool_result(result: Option<ToolResult>) -> Option<ToolRenderResult> {
    result.map(|result| ToolRenderResult {
        outcome: outcome_code(result.outcome).to_string(),
        output: result.output,
        hunks: result
            .hunks
            .into_iter()
            .map(|section| HunkLines {
                path: section.path,
                lines: section.lines,
            })
            .collect(),
        duration_ms: result.duration_ms,
    })
}

fn outcome_code(outcome: AuditOutcome) -> &'static str {
    match outcome {
        AuditOutcome::Executed => "executed",
        AuditOutcome::Denied => "denied",
        AuditOutcome::Failed => "failed",
    }
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
    rendered: &Rendered,
) {
    let nodes = rendered.message.get(&salt).map(|nodes| nodes.as_slice());
    match step {
        Step::User { text, images } => {
            draw_bubble(
                ui,
                p,
                Message {
                    text,
                    nodes: None,
                    salt,
                },
                p.bubble_user,
                Align::Max,
                max_width,
            );
            draw_user_images(ui, salt, images, thumbs);
        }
        Step::Assistant { text } => {
            draw_agent_message(ui, p, Message { text, nodes, salt }, max_width)
        }
        Step::Notice { text } => {
            draw_bubble(
                ui,
                p,
                Message { text, nodes, salt },
                p.bubble_notice,
                Align::Center,
                max_width,
            );
        }
        // A host message reached the model as a user turn, but it is not the
        // user's own words — it renders like a notice, not like a user bubble.
        Step::HostMessage { text } => {
            draw_bubble(
                ui,
                p,
                Message { text, nodes, salt },
                p.bubble_notice,
                Align::Center,
                max_width,
            );
        }
        // Reasoning needs the expanded set, which this function has no access to;
        // `draw_transcript` intercepts it before calling here. This arm only
        // exists to keep the match exhaustive, and renders collapsed.
        Step::Reasoning { text, .. } => {
            draw_reasoning_block(ui, p, text, false, max_width);
        }
        Step::Tool {
            call_id,
            name,
            result,
            ..
        } => {
            let nodes = rendered.tool.get(&(salt.0, call_id.clone()));
            draw_tool_card(
                ui,
                p,
                ToolCard {
                    call_id,
                    name,
                    result: result.as_ref(),
                    nodes: nodes.map(|nodes| nodes.as_slice()),
                },
                max_width,
            );
        }
        Step::Compaction { summary } => {
            let text = if summary.trim().is_empty() {
                "上下文压缩失败，对话按原样继续".to_string()
            } else {
                format!("上下文已压缩。此前对话的摘要：\n\n{summary}")
            };
            draw_bubble(
                ui,
                p,
                Message {
                    text: &text,
                    nodes: None,
                    salt,
                },
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

/// One transcript message as the drawing code needs it.
///
/// `nodes` is the renderer's display list once it is ready and takes precedence;
/// `text` is the raw body the plain-text fallback draws, and `salt` names the
/// message's scroll-area state. The three travel together through every
/// message-drawing function, so they are passed as one value.
pub(super) struct Message<'a> {
    pub(super) text: &'a str,
    pub(super) nodes: Option<&'a [Node]>,
    pub(super) salt: (Uuid, usize),
}

/// The assistant's reply, in a centred column.
///
/// Deliberately not a bubble. A fill around every reply only adds a box to read
/// past, and the column is already narrower than the panel, so the two sides
/// stay tellable apart: the user's turns are filled and hug the right, the
/// agent's sit open in the column.
///
/// The column is a fixed width rather than content-sized so consecutive replies
/// start at the same x — a column that shrank to each message would drift
/// around the middle of the window as the answer streamed in.
fn draw_agent_message(ui: &mut egui::Ui, p: &Palette, message: Message<'_>, max_width: f32) {
    let Message { text, nodes, salt } = message;
    ui.vertical_centered(|ui| {
        // No fill and no corner radius: `Frame` is here only to give the column
        // a width for `vertical_centered` to centre, the way the composer's
        // frame is used. `Frame::show` inherits `vertical_centered`'s layout,
        // which centres across the column — fine for a single block, wrong for a
        // stack of them, hence the explicit `vertical` that left-aligns.
        Frame::NONE.show(ui, |ui| {
            ui.set_width(max_width);
            ui.vertical(|ui| {
                draw_message_body(ui, p, text, nodes, salt);
            });
        });
    });
    ui.add_space(8.0);
}

/// Draws a message body from the renderer's display list when it is ready, else
/// as plain text.
fn draw_message_body<S: Hash>(
    ui: &mut egui::Ui,
    p: &Palette,
    text: &str,
    nodes: Option<&[Node]>,
    salt: S,
) {
    match nodes {
        Some(nodes) => present::draw(ui, p, nodes, salt),
        None => draw_plain_text(ui, p, text),
    }
}

/// The fallback for a message the renderer could not shape: the raw body,
/// selectable and wrapped, with no Markdown applied.
fn draw_plain_text(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add(
        egui::Label::new(RichText::new(text).size(theme::font(14.0)).color(p.text))
            .wrap()
            .selectable(true),
    );
}

/// One message, as a bubble of rendered content.
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
    message: Message<'_>,
    fill: Color32,
    side: Align,
    max_width: f32,
) -> egui::Rect {
    let Message { text, nodes, salt } = message;
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
                    draw_message_body(ui, p, text, nodes, (salt, "measure"));
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
                draw_message_body(ui, p, text, nodes, salt);
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

/// One tool call as the card draws it.
///
/// The call's identity and result, plus the renderer's display list once it has
/// returned. They are the same call's data, so they travel as one value.
struct ToolCard<'a> {
    call_id: &'a str,
    name: &'a str,
    result: Option<&'a ToolResult>,
    nodes: Option<&'a [Node]>,
}

/// One tool call.
///
/// When the renderer has shaped it, the whole card — the collapsed row and the
/// expanded body — is its display list, drawn by [`present::draw`]. When it has
/// not, a minimal host fold stands in: the tool's name over its raw output.
fn draw_tool_card(ui: &mut egui::Ui, p: &Palette, card: ToolCard<'_>, max_width: f32) {
    let ToolCard {
        call_id,
        name,
        result,
        nodes,
    } = card;

    // The same centred column the replies use: left to itself the row hugs the
    // transcript's edge, and a run of calls reads as a second flow beside the
    // prose it belongs to.
    ui.vertical_centered(|ui| {
        Frame::NONE.show(ui, |ui| {
            ui.set_width(max_width);
            match nodes {
                Some(nodes) => present::draw(ui, p, nodes, call_id),
                None => draw_tool_fallback(ui, p, call_id, name, result),
            }
        });
    });
    ui.add_space(8.0);
}

/// The stand-in for a tool card the renderer could not shape: a fold named after
/// the tool, holding its raw output.
fn draw_tool_fallback(
    ui: &mut egui::Ui,
    p: &Palette,
    call_id: &str,
    name: &str,
    result: Option<&ToolResult>,
) {
    let accent = match result {
        None => p.text_muted,
        Some(result) => outcome_colour(result.outcome),
    };
    let glyph = match result {
        None => icons::SPINNER_GAP,
        Some(result) => outcome_icon(result.outcome),
    };
    let font = FontId::proportional(theme::font(13.0));
    let mut job = LayoutJob::default();
    append_run(&mut job, &format!("{glyph}  "), &font, accent);
    append_run(&mut job, name, &font, p.text_muted);

    egui::CollapsingHeader::new(job)
        .id_salt(call_id)
        .default_open(false)
        .show_background(false)
        .show(ui, |ui| match result {
            Some(result) if !result.output.is_empty() => selectable_code(ui, &result.output),
            Some(_) => {
                ui.label(
                    RichText::new("（无输出）")
                        .size(theme::font(12.0))
                        .color(p.text_muted),
                );
            }
            None => {
                ui.label(
                    RichText::new("运行中…")
                        .size(theme::font(12.0))
                        .color(p.text_muted),
                );
            }
        });
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

fn outcome_colour(outcome: AuditOutcome) -> Color32 {
    match outcome {
        AuditOutcome::Executed => theme::OK_GREEN,
        AuditOutcome::Denied => theme::WARN_AMBER,
        AuditOutcome::Failed => theme::BAD_RED,
    }
}

fn outcome_icon(outcome: AuditOutcome) -> &'static str {
    match outcome {
        AuditOutcome::Executed => crate::icons::CHECK_CIRCLE,
        AuditOutcome::Denied => crate::icons::WARNING_CIRCLE,
        AuditOutcome::Failed => crate::icons::X_CIRCLE,
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
