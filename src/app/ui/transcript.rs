//! 转录区和渲染相关

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui;
use egui::{Align, TextureHandle};
use serde_json::Value;
use uuid::Uuid;

use crate::renderer::protocol::{Node, RenderKey, RenderMetrics, ToolRenderRequest};
use crate::session::{Step, ToolResult};
use crate::theme::Palette;

use super::super::{App, Cmd, GuiResources};
use super::common::{render_metrics, tool_fingerprint, tool_result};
use super::composer::COMPOSER_MAX_WIDTH;
use super::messages::{
    draw_agent_message, draw_bubble, draw_reasoning_block, draw_user_images, Message, CHAT_MARGIN_X,
};
use super::primitives::draw_empty_state;
use super::tools::{draw_tool_card, ToolCard};

use super::super::project_name;
use super::super::render_cache;

/// How close to the bottom counts as "the user is following along".
const STICK_THRESHOLD: f32 = 24.0;

/// The renderer IR available for the visible steps.
#[derive(Default)]
pub(super) struct Rendered {
    pub(super) message: HashMap<(Uuid, usize), Arc<Vec<Node>>>,
    pub(super) tool: HashMap<(Uuid, String), Arc<Vec<Node>>>,
}

/// One render request waiting to be sent.
pub(super) enum PendingRender {
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

impl App {
    /// The scrolling transcript for the open session, or the empty state.
    pub(in crate::app) fn draw_transcript(
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
            let name = self.active_project.as_deref().map(project_name);
            draw_empty_state(ui, p, name.as_deref());
            return;
        };

        if self.sessions[index].steps.is_empty() {
            let name = self.sessions[index].project_name();
            draw_empty_state(ui, p, Some(&name));
            return;
        }

        let session_id = self.sessions[index].id;
        let metrics = render_metrics(ui, ui.available_width() - 2.0 * CHAT_MARGIN_X);
        let rendered = self.collect_rendered(session_id, index, metrics);
        let steps = &self.sessions[index].steps;
        let expanded_reasoning = &mut self.expanded_reasoning;
        let thumbs = &mut resources.thumbs;

        let output = egui::ScrollArea::vertical()
            .id_salt("transcript")
            .auto_shrink([false, false])
            .stick_to_bottom(stick)
            .show(ui, |ui| {
                ui.add_space(14.0);
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

        self.stick_to_bottom = output.state.offset.y + output.inner_rect.height()
            >= output.content_size.y - STICK_THRESHOLD;
    }

    /// Runs before the transcript borrows the session, so it can send commands
    /// and touch the cache.
    pub(super) fn collect_rendered(
        &mut self,
        session_id: Uuid,
        index: usize,
        metrics: RenderMetrics,
    ) -> Rendered {
        let mut rendered = Rendered::default();
        if !self.renderer_available {
            return rendered;
        }
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
    pub(super) fn dispatch_render(&mut self, request: PendingRender) {
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
}

/// Renders one transcript step.
pub(super) fn draw_step(
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
