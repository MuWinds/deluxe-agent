//! 面板：菜单栏、导航栏、侧边栏

use std::mem;

use eframe::egui;
use egui::{Align, Frame, Layout, Margin, RichText};
use uuid::Uuid;

use crate::icons;
use crate::ipc::RunState;
use crate::session::{self, Session};
use crate::theme::{self, Palette, ThemeChoice};

use super::super::{project_name, App, UiIntent};
use super::common::shorten;
use super::primitives::{rail_button, section_label, sidebar_row, RAIL_WIDTH, SIDEBAR_WIDTH};

impl App {
    /// The top menu bar: 文件 / 编辑 / 视图 / 帮助.
    pub(in crate::app) fn draw_menu_bar(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        intents: &mut Vec<UiIntent>,
    ) {
        let mut theme_choice = self.config.theme;
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
    pub(in crate::app) fn draw_rail(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        intents: &mut Vec<UiIntent>,
    ) {
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
                    if rail_button(ui, icons::HOUSE, !selected, "主界面").clicked() {
                        intents.push(UiIntent::NewSession);
                    }
                    if rail_button(ui, icons::PUZZLE_PIECE, plugins_open, "插件").clicked() {
                        intents.push(UiIntent::OpenPlugins);
                    }
                });

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
    pub(in crate::app) fn draw_sidebar(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        intents: &mut Vec<UiIntent>,
    ) {
        let mut search = mem::take(&mut self.search);
        let now = session::now_unix();
        let query = search.trim().to_lowercase();

        let projects = self.config.projects.clone();
        let sessions = &self.sessions;
        let selected = self.selected;
        let expanded = self.active_project.clone();
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
                        if let Some(error) = &sidebar_error {
                            ui.label(
                                RichText::new(error)
                                    .size(theme::font(11.0))
                                    .color(crate::theme::BAD_RED),
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
}

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
        egui::Button::selectable(selected == Some(session.id), (text, egui::Atom::grow()))
            .truncate(),
    );

    if !marker.is_empty() {
        let rect = response.rect;
        ui.painter().circle_filled(
            egui::pos2(rect.right() - 12.0, rect.center().y),
            3.5,
            if session.state == RunState::Running {
                p.accent
            } else {
                crate::theme::BAD_RED
            },
        );
    }

    response.on_hover_text(format!(
        "{}\n{}",
        session.title(),
        session::age_label(session.created_at, now)
    ))
}
