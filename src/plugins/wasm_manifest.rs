//! Executable components are opt-in and never inherit ambient WASI access.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{code, AgentError, Result};

use super::ui_protocol::valid_identifier;

pub const API_VERSION: &str = "deluxe.harness/plugin@0.1";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WasmManifest {
    pub module: String,
    pub api_version: String,
    #[serde(default)]
    pub ui: UiManifest,
    #[serde(default)]
    pub permissions: Permissions,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiManifest {
    #[serde(default)]
    pub surfaces: Vec<String>,
    #[serde(default)]
    pub actions: Vec<String>,
    /// Whether this plugin contributes an inline control to the input row.
    ///
    /// The host opens a [`super::ui_protocol::COMPOSER_SURFACE_ID`] surface for
    /// it and renders that snapshot inline. Kept separate from `surfaces`,
    /// which lists the windows the user can open from the plugin panel.
    #[serde(default)]
    pub composer: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Permissions {
    #[serde(default)]
    pub invoke_tools: Vec<String>,
    #[serde(default)]
    pub process_commands: Vec<String>,
    /// Whether the Component may replace files inside its configuration root.
    ///
    /// Off unless declared: a Component that only *reads* `.mcp.json` or
    /// `.hooks.json` must not be able to rewrite the user's scope config. A
    /// plain switch rather than a path allowlist because the root is the unit
    /// of trust — a Component already reads every file under it.
    #[serde(default)]
    pub write_plugin_files: bool,
}

impl WasmManifest {
    /// Checks ABI and resolves an existing component inside `root`.
    ///
    /// Returns `Err` for an unsupported ABI, invalid declarations, or escaped paths,
    /// including symlinks. Called only on a worker or blocking loader task.
    pub fn resolve_entry(&self, root: &Path) -> Result<PathBuf> {
        if self.api_version != API_VERSION {
            return Err(AgentError::new(
                code::PLUGIN_API_MISMATCH,
                "Unsupported plugin API version",
            ));
        }
        for ids in [
            &self.ui.surfaces,
            &self.ui.actions,
            &self.permissions.invoke_tools,
        ] {
            let mut seen = HashSet::new();
            if ids.len() > 128
                || ids
                    .iter()
                    .any(|id| !valid_identifier(id) || !seen.insert(id))
            {
                return Err(AgentError::new(
                    code::PLUGIN_LOAD_FAILED,
                    "Invalid or duplicate UI declarations",
                ));
            }
        }
        let mut seen = HashSet::new();
        if self.permissions.process_commands.len() > 128
            || self.permissions.process_commands.iter().any(|id| {
                (id != "*" && (id.is_empty() || id.len() > 256 || id.bytes().any(|byte| byte == 0)))
                    || !seen.insert(id)
            })
        {
            return Err(AgentError::new(
                code::PLUGIN_LOAD_FAILED,
                "Invalid or duplicate process declarations",
            ));
        }
        let relative = Path::new(&self.module);
        if relative.as_os_str().is_empty()
            || relative.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Component entry must stay inside its plugin root",
            ));
        }
        let root = root
            .canonicalize()
            .map_err(|error| AgentError::from_io("Resolve plugin root", error))?;
        let entry = root
            .join(relative)
            .canonicalize()
            .map_err(|error| AgentError::from_io("Resolve component entry", error))?;
        if !entry.starts_with(&root) || !entry.is_file() {
            return Err(AgentError::new(
                code::PLUGIN_PERMISSION_DENIED,
                "Component entry escapes its plugin root",
            ));
        }
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_cannot_escape_its_root_and_versions_are_explicit() {
        let root = tempfile::tempdir().expect("temp root");
        std::fs::write(root.path().join("plugin.wasm"), b"fixture").expect("fixture file");
        let mut manifest: WasmManifest = serde_json::from_str(
            r#"{"module":"./plugin.wasm","apiVersion":"deluxe.harness/plugin@0.1"}"#,
        )
        .expect("valid manifest");
        assert!(manifest.resolve_entry(root.path()).is_ok());
        for module in ["../outside.wasm", "/outside.wasm", r"C:\outside.wasm"] {
            manifest.module = module.into();
            assert!(
                manifest.resolve_entry(root.path()).is_err(),
                "expected `{module}` to be refused"
            );
        }
        manifest.module = "plugin.wasm".into();
        manifest.api_version = "future".into();
        assert_eq!(
            manifest
                .resolve_entry(root.path())
                .expect_err("incompatible ABI")
                .code,
            code::PLUGIN_API_MISMATCH
        );
    }

    /// The composer is opt-in: a manifest that does not mention it contributes
    /// no inline control, and one that does is recognized.
    #[test]
    fn the_inline_composer_is_opt_in() {
        let plain: WasmManifest = serde_json::from_str(
            r#"{"module":"plugin.wasm","apiVersion":"deluxe.harness/plugin@0.1"}"#,
        )
        .expect("valid manifest");
        assert!(!plain.ui.composer);

        let with_composer: WasmManifest = serde_json::from_str(
            r#"{"module":"plugin.wasm","apiVersion":"deluxe.harness/plugin@0.1",
                "ui":{"composer":true}}"#,
        )
        .expect("valid manifest");
        assert!(with_composer.ui.composer);
    }
}
