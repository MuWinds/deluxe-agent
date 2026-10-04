//! 输入编辑器、上下文度量、思考选择器

use eframe::egui;
use egui::{Color32, CornerRadius, Frame, Margin, Pos2, RichText, Stroke, Vec2};

use crate::attachments::ImageRef;
use crate::icons;
use crate::llm::ThinkingLevel;
use crate::session::Session;
use crate::theme::{self, Palette};

use super::super::{chip_label, format_tokens, App, UiIntent};
use super::jobs::draw_job_row;
use super::primitives::{circle_button, COMPOSER_BUTTON};

/// The composer stops growing past this, and is centred in the main area.
pub const COMPOSER_MAX_WIDTH: f32 = 820.0;

/// Space between the gauge ring and its percentage.
const GAUGE_RING_GAP: f32 = 7.0;

/// Upper bound of the gauge box.
const CONTEXT_GAUGE_RESERVE: f32 = 64.0;

/// Upper bound of the thinking picker on the composer's input row.
const THINKING_PICKER_RESERVE: f32 = 84.0;

/// Space reserved on the input row for a plugin-contributed composer control.
const COMPOSER_CONTROL_RESERVE: f32 = 150.0;

/// Straight-line segments used to draw the gauge ring.
const SEGMENTS_PER_RING: usize = 24;

/// The most job rows the composer draws.
const MAX_JOB_ROWS: usize = 5;

impl App {
    /// The composer: the input row and the queued images.
    pub(in crate::app) fn draw_composer(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        intents: &mut Vec<UiIntent>,
    ) {
        let running = self.selected.and_then(|id| self.run_for(id)).is_some();
        let can_send = self.can_send();

        egui::Panel::bottom("composer")
            .min_size(76.0)
            .show_separator_line(false)
            .frame(Frame::NONE.fill(p.main_bg).inner_margin(Margin {
                left: super::messages::CHAT_MARGIN_X as i8,
                right: super::messages::CHAT_MARGIN_X as i8,
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
                                let composer_reserve = if self.composer.is_some() {
                                    COMPOSER_CONTROL_RESERVE
                                } else {
                                    0.0
                                };
                                let editor_width = (ui.available_width()
                                    - CONTEXT_GAUGE_RESERVE
                                    - THINKING_PICKER_RESERVE
                                    - COMPOSER_BUTTON
                                    - composer_reserve
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
                                let enter = ui.input(|input| {
                                    input.key_pressed(egui::Key::Enter) && !input.modifiers.shift
                                });
                                if editor.has_focus() && enter {
                                    intents.push(UiIntent::SendPrompt);
                                }

                                self.draw_composer_control(ui, intents);
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

                            if !self.pending_images.is_empty() {
                                self.draw_pending_image_strip(ui, p, intents);
                            }
                        });

                    self.draw_jobs(ui, p, intents);
                });
            });
    }

    /// The plugin-contributed control on the input row, if one is loaded.
    ///
    /// It is drawn from the same validated snapshot the settings window would
    /// show; the host knows nothing about what the control means.
    fn draw_composer_control(&self, ui: &mut egui::Ui, intents: &mut Vec<UiIntent>) {
        let Some(surface) = &self.composer else {
            return;
        };
        let Some(document) = &surface.document else {
            return;
        };
        ui.push_id(&surface.request, |ui| {
            super::super::plugin_ui::render_node(
                ui,
                &document.root,
                &surface.request,
                document.revision,
                intents,
            );
        });
    }

    /// The per-conversation reasoning-effort picker, drawn in the composer.
    fn draw_thinking_picker(&mut self, ui: &mut egui::Ui) {
        let selected = match self.thinking {
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
    fn draw_context_gauge(&self, ui: &mut egui::Ui, p: &Palette) {
        let session = self.selected_session();
        let measured = session
            .and_then(|session| session.context_measurement)
            .map(|(tokens, _)| tokens);
        // The window is the provider Component's to declare; the host only
        // knows the compaction share it applies to it.
        let limit = self.llm_descriptor.context_tokens;

        let used = measured.unwrap_or(0);
        let ratio = if limit > 0 {
            (used as f32 / limit as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let compaction = self.config.context.threshold_ratio();
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
                "上下文：已用 {} tokens；供应商未报告窗口大小",
                format_tokens(tokens)
            ),
            (None, true) => format!("上下文：窗口 {}，还没有用量数据", format_tokens(limit)),
            (None, false) => "上下文：还没有用量数据；供应商未报告窗口大小".to_string(),
        };
        if limit > 0 && ratio >= compaction && measured.is_some() {
            summary.push_str(" —— 达到压缩阈值，下一条消息会先压缩历史");
        }

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

        let painter = ui.painter_at(rect);
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

    /// The composer's background-task and sub-agent list.
    fn draw_jobs(&mut self, ui: &mut egui::Ui, p: &Palette, intents: &mut Vec<UiIntent>) {
        if self.jobs.is_empty() {
            return;
        }

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
}

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
