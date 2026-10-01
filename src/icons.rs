//! The icon set.
//!
//! One thin layer over `egui_phosphor`, so that turning on that crate's
//! `subset!` feature later — which trims the 488 KB font down to just the
//! glyphs actually used — touches this file and nothing else.
//!
//! The constants are listed explicitly rather than glob re-exported on purpose:
//! this is the manifest of which icons the app draws, and a glob would hide it.
//!
//! `egui_phosphor::add_to_fonts` registers the face as `"phosphor"` and appends
//! it to the proportional family, so an icon can be mixed into ordinary text
//! with `format!("{GEAR} 设置")` — which is what the sidebar rows do.

pub use egui_phosphor::regular::{
    ARROW_CLOCKWISE, ARROW_UP, BRAIN, CARET_DOWN, CARET_RIGHT, CHECK_CIRCLE, COPY, DOTS_THREE, EYE,
    FOLDER_SIMPLE, GEAR, HOUSE, IMAGE, MAGNIFYING_GLASS, NOTE_PENCIL, PLUS, PUZZLE_PIECE, QUESTION,
    ROBOT, SPINNER_GAP, STOP_CIRCLE, TERMINAL_WINDOW, WARNING_CIRCLE, X, X_CIRCLE,
};
