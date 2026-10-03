//! Wasmtime manifest declarations for one installed plugin.
//!
//! Plugin manifests describe Wasmtime Components. Provider-owned configuration
//! is intentionally not represented here: Components request their own files
//! through the generic host file capability.

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, Result};

/// `plugin.json` — the one file a Wasmtime plugin cannot omit.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    /// kebab-case identifier, and the component namespace.
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    /// The top-level summary. Used only when the `interface` block, which is
    /// written for people and reads better, carries no short description.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub interface: Option<PluginInterface>,
    #[serde(default)]
    pub runtime: Option<serde_json::Value>,
}

impl PluginManifest {
    /// Projects the Wasmtime component declaration, if one is present.
    ///
    /// Malformed declarations are logged and skipped.
    pub fn wasm_runtime(&self) -> Option<super::wasm_manifest::WasmManifest> {
        let runtime = self.runtime.as_ref()?;
        match serde_json::from_value(runtime.clone()) {
            Ok(manifest) => Some(manifest),
            Err(error) => {
                tracing::warn!(plugin = %self.name, %error, "skipping an invalid component declaration");
                None
            }
        }
    }

    /// The name a person should see, falling back to the identifier.
    pub fn display_name(&self) -> &str {
        self.interface
            .as_ref()
            .and_then(|interface| interface.display_name.as_deref())
            .unwrap_or(&self.name)
    }

    /// The one-line description, preferring the short interface field.
    ///
    /// The interface block is the presentation layer and is written for people,
    /// so it reads better in a log line than the top-level `description`, which
    /// tends to be terser.
    pub fn summary(&self) -> Option<&str> {
        self.interface
            .as_ref()
            .and_then(|interface| interface.short_description.as_deref())
            .or(self.description.as_deref())
    }
}

/// The presentation block, of which this agent reads only the two names.
///
/// The rest of the block — categories, capabilities, brand colours, icons,
/// screenshots — exists so a plugin browser can draw a card, and this agent has
/// no plugin browser. Declaring those fields would add nothing but unused code.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInterface {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub short_description: Option<String>,
}

/// The manifest file at the root of a Wasmtime plugin.
pub const MANIFEST_FILE: &str = "plugin.json";
/// Reads and parses a plugin manifest from a plugin root.
pub fn read_plugin(root: &Path) -> Result<PluginManifest> {
    let path = root.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).map_err(|error| {
        AgentError::from_io(&format!("Failed to read {}", path.display()), error)
    })?;
    parse_plugin(&text, &path)
}

fn parse_plugin(text: &str, path: &Path) -> Result<PluginManifest> {
    serde_json::from_str(text).map_err(|error| {
        AgentError::internal(format!(
            "{} is not a valid plugin manifest: {error}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest carrying far more than this agent reads — a presentation
    /// block, brand colours, icons, screenshots — because tolerating all those
    /// unknown fields is the property under test.
    ///
    /// `r##"…"##` rather than `r#"…"#`, because the brand colour is a hex string
    /// and its `"#` would close the shorter delimiter.
    const SAMPLE: &str = r##"{
      "name": "sample",
      "version": "2.0.20",
      "description": "Sample workflows for this agent.",
      "author": { "name": "Example", "url": "https://example.com" },
      "homepage": "https://example.com",
      "repository": "https://github.com/example/plugins",
      "license": "LicenseRef-Example",
      "keywords": ["sample", "example"],
      "apps": "./.app.json",
      "interface": {
        "displayName": "Sample",
        "shortDescription": "Sample workflows for this agent",
        "longDescription": "Longer prose for a details page.",
        "developerName": "Example",
        "category": "Productivity",
        "capabilities": ["Interactive", "Read", "Write"],
        "websiteURL": "https://example.com",
        "privacyPolicyURL": "https://example.com/legal/privacy/",
        "termsOfServiceURL": "https://example.com/legal/terms/",
        "defaultPrompt": ["Inspect a sample and act on it"],
        "brandColor": "#1ABCFE",
        "composerIcon": "./assets/logo-padded.png",
        "screenshots": []
      }
    }"##;

    #[test]
    fn a_full_manifest_parses() {
        let manifest = parse_plugin(SAMPLE, std::path::Path::new("plugin.json")).unwrap();

        assert_eq!(manifest.name, "sample");
        assert_eq!(manifest.version.as_deref(), Some("2.0.20"));
    }

    #[test]
    fn the_display_name_and_summary_prefer_the_interface_block() {
        let manifest = parse_plugin(SAMPLE, std::path::Path::new("plugin.json")).unwrap();

        assert_eq!(manifest.display_name(), "Sample");
        assert_eq!(
            manifest.summary(),
            Some("Sample workflows for this agent"),
            "the short interface description reads better than the top-level one"
        );
    }

    #[test]
    fn a_manifest_with_only_a_name_parses() {
        // Everything but `name` is optional: a plugin must not fail to load
        // just because it omits the presentation block.
        let manifest = parse_plugin(r#"{"name":"bare"}"#, std::path::Path::new("p")).unwrap();

        assert_eq!(manifest.name, "bare");
        assert!(manifest.interface.is_none());
        // Falls back to the identifier when there is no interface block.
        assert_eq!(manifest.display_name(), "bare");
        assert_eq!(manifest.summary(), None);
    }

    #[test]
    fn unknown_fields_are_ignored_rather_than_fatal() {
        // The format carries far more than this agent reads — icons, colours,
        // screenshots, connector ids — and it keeps growing. Every one of those
        // keys must be dropped, not treated as a malformed manifest.
        let manifest = parse_plugin(
            r##"{"name":"future","somethingNew":true,"nested":{"a":1},
                "interface":{"brandColor":"#fff","logo":"./l.png"}}"##,
            std::path::Path::new("p"),
        )
        .unwrap();

        assert_eq!(manifest.name, "future");
        // The block parses; its unread keys are simply absent.
        assert_eq!(manifest.interface.unwrap().display_name, None);
    }

    #[test]
    fn a_manifest_without_a_name_is_an_error() {
        // `name` is the one required field: it is the namespace every other
        // component is keyed by, so there is nothing sensible to fall back to.
        let error = parse_plugin(r#"{"version":"1.0.0"}"#, std::path::Path::new("p")).unwrap_err();
        assert!(
            error.message.contains("plugin manifest"),
            "{}",
            error.message
        );
    }
}
