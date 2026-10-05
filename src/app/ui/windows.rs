//! 独立窗口：设置、关于、插件、子代理

use std::path::{Path, PathBuf};

use eframe::egui;
use egui::RichText;

use crate::icons;
use crate::plugins;
use crate::session;
use crate::theme::{self, Palette};

use super::super::{project_name, App, UiIntent};
use super::messages::draw_reasoning_block;
use super::primitives::draw_logo;
use super::transcript::draw_step;

impl App {
    /// The 设置 window: endpoint, key, context and safety settings.
    pub(in crate::app) fn draw_settings(
        &mut self,
        ctx: &egui::Context,
        p: &Palette,
        intents: &mut Vec<UiIntent>,
    ) {
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
                ui.weak("供应商、协议与密钥由「LLM 供应商」插件管理，本页只保留压缩与重试策略。");
                ui.horizontal(|ui| {
                    // The model name lives on the composer's picker now; this
                    // page only reports readiness and links to the provider
                    // configuration.
                    let status = if !self.llm_available {
                        "供应商插件未启用"
                    } else if !self.llm_descriptor.ready {
                        "尚未选择供应商"
                    } else {
                        "已就绪"
                    };
                    ui.label(format!("状态：{status}"));
                    if ui.button("配置供应商").clicked() {
                        intents.push(UiIntent::OpenPluginSurface {
                            plugin_id: crate::plugins::llm::LLM_PROVIDER_PLUGIN_ID.into(),
                            surface_id: "providers".into(),
                        });
                    }
                });
                ui.add_space(6.0);

                egui::Grid::new("llm-settings")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
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
    pub(in crate::app) fn draw_about(&mut self, ctx: &egui::Context, p: &Palette) {
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
    pub(in crate::app) fn draw_plugins(
        &mut self,
        ctx: &egui::Context,
        intents: &mut Vec<UiIntent>,
    ) {
        if !self.show_plugins {
            return;
        }

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

                if let Some(error) = &self.plugins_error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(error).color(theme::BAD_RED));
                }
                ui.add_space(6.0);

                let surfaces =
                    super::super::view_model::plugin_surfaces(&self.catalogue, Path::new(&project));
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

    /// The window showing one sub-agent's own conversation.
    pub(in crate::app) fn draw_subagent_window(
        &mut self,
        ctx: &egui::Context,
        p: &Palette,
        intents: &mut Vec<UiIntent>,
    ) {
        let Some(job_id) = self.open_subagent.clone() else {
            return;
        };
        let Some(index) = self
            .subagent_runs
            .iter()
            .position(|run| run.job_id == job_id)
        else {
            return;
        };

        let job = self.jobs.iter().find(|job| job.id == job_id);
        let title = job
            .map(|job| job.label.clone())
            .unwrap_or_else(|| job_id.clone());
        let state = job.map(|job| job.state);
        let agent = self.subagent_runs[index].agent.clone();

        let mut open = true;
        let mut stop = false;
        let salt = self.subagent_runs[index].salt;
        let steps = &self.subagent_runs[index].steps;
        let expanded_reasoning = &mut self.expanded_reasoning;

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
                                crate::session::Step::Reasoning { id, text } => {
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
                                    &super::transcript::Rendered::default(),
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
}

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
                ui.weak("只删除托管缓存里的副本。");
            }
        });
}

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
