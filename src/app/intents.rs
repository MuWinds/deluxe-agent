//! Pure input intents emitted by the GUI renderer.
//!
//! An intent is a controller input, not an egui callback. Applying one may
//! update domain state or enqueue an IPC command, while GUI-only effects are
//! returned to the renderer as [`UiEffects`].

use std::path::PathBuf;

use uuid::Uuid;

use crate::theme::ThemeChoice;

/// A user action collected during one render pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UiIntent {
    Quit,
    NewSession,
    SelectSession(Uuid),
    SelectProject(String),
    DeleteSession(Uuid),
    SendPrompt,
    CancelRun,
    KillJob(String),
    PollJobs(PathBuf),
    ToggleSubagent(String),
    CloseSubagent,
    OpenSettings,
    OpenAbout,
    OpenPlugins,
    SetTheme(ThemeChoice),
    SetSidebarVisible(bool),
    CopyTranscript,
    SetPluginEnabled { id: String, enabled: bool },
    UninstallPlugin(String),
    AddProject,
    RemoveProject(String),
    PasteImage,
    PickImage,
    RemovePendingImage(String),
    SaveSettings,
}

/// GUI effects produced after the controller applies intents.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct UiEffects {
    pub(super) close: bool,
    pub(super) theme: Option<ThemeChoice>,
    pub(super) clipboard_text: Option<String>,
    pub(super) repaint_after: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_protocol_does_not_require_an_egui_context() {
        let intent = UiIntent::SetTheme(ThemeChoice::Light);

        assert_eq!(intent, UiIntent::SetTheme(ThemeChoice::Light));
        assert_eq!(UiEffects::default(), UiEffects::default());
    }
}
