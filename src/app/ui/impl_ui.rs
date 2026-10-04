//! The window's egui rendering.
//!
//! Every widget the app draws lives here; the view state it reads and the
//! [`Cmd`]s it sends are defined in the parent module.

use super::super::{App, GuiResources};

use eframe::egui;
use egui::Frame;

use crate::theme;

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
        super::super::plugin_ui::draw(&ctx, self, &mut intents);
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
            ctx.request_repaint_after(super::super::JOBS_POLL_INTERVAL);
        }
    }

    /// The central panel: the composer pinned below the transcript.
    ///
    /// The composer is drawn first so the bottom panel can claim its height
    /// before the transcript's scroll area sizes itself from what is left.
    fn draw_main(
        &mut self,
        ui: &mut egui::Ui,
        p: &crate::theme::Palette,
        intents: &mut Vec<super::super::UiIntent>,
        resources: &mut GuiResources,
    ) {
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(p.main_bg))
            .show(ui, |ui| {
                self.draw_composer(ui, p, intents);
                self.draw_transcript(ui, p, resources);
            });
    }
}
