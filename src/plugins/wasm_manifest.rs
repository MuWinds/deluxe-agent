//! Executable components are opt-in and never inherit ambient WASI access.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{code, AgentError, Result};

use super::ui_protocol::valid_identifier;

pub const API_VERSION: &str = "deluxe.harness/plugin@0.1";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    General,
    Hooks,
    Mcp,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WasmManifest {
    #[serde(rename = "type")]
    pub runtime_type: String,
    pub module: String,
    pub api_version: String,
    #[serde(default)]
    pub provider: ProviderKind,
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
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Permissions {
    #[serde(default)]
    pub invoke_tools: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub network_hosts: Vec<String>,
}

impl WasmManifest {
    /// Checks ABI and resolves an existing component inside `root`.
    ///
    /// Returns `Err` for an unsupported ABI, invalid declarations, or escaped paths,
    /// including symlinks. Called only on a worker or blocking loader task.
    pub fn resolve_entry(&self, root: &Path) -> Result<PathBuf> {
        if self.runtime_type != "wasm" || self.api_version != API_VERSION {
            return Err(AgentError::new(
                code::PLUGIN_API_MISMATCH,
                "Unsupported plugin runtime or API version",
            ));
        }
        for ids in [
            &self.ui.surfaces,
            &self.ui.actions,
            &self.permissions.invoke_tools,
            &self.permissions.mcp_servers,
            &self.permissions.network_hosts,
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
        if self.permissions.network_hosts.len() > 64
            || self
                .permissions
                .network_hosts
                .iter()
                .any(|host| host.is_empty() || host.len() > 255)
        {
            return Err(AgentError::new(
                code::PLUGIN_LOAD_FAILED,
                "Invalid network host declarations",
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
            r#"{"type":"wasm","module":"./plugin.wasm","apiVersion":"deluxe.harness/plugin@0.1"}"#,
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
}
