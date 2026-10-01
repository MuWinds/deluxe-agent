//! GUI-only resources owned by the renderer.
//!
//! Keeping texture handles here means application state can be folded and
//! tested without linking a live egui context.

use std::collections::HashMap;

use eframe::egui::TextureHandle;

/// Resources whose lifetimes are tied to the egui context.
#[derive(Default)]
pub struct GuiResources {
    pub(super) thumbs: HashMap<String, TextureHandle>,
}
