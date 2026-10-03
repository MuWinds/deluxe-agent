//! The transcript renderer: a built-in Wasm Component and its protocol.
//!
//! The guest shapes messages and tool results into a generic display list —
//! styled text runs, rows, frames, grids, folds — and the host draws that list
//! with egui. The renderer ships as an ordinary built-in plugin
//! (`builtin-plugins/transcript-renderer/`) and is loaded and called through the
//! plugin platform ([`crate::plugins::wasm_runtime::ComponentActor`]): it
//! exports the harness `plugin` interface inertly so the platform can load it,
//! plus the `renderer` interface the host calls.
//!
//! The host knows nothing about Markdown or tool-card semantics. When the
//! renderer plugin is disabled, missing, or fails, a message falls back to plain
//! text and a tool call to a minimal name-and-output card; see [`crate::app::ui`].

pub mod present;
pub mod protocol;

#[cfg(test)]
mod golden;
