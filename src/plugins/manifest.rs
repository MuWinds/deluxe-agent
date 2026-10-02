//! Wasmtime manifest declarations for one installed plugin.
//!
//! Plugin manifests describe Wasmtime Components. Provider-owned configuration
//! is intentionally not represented here: Components request their own files
//! through the generic host file capability.

use std::path::{Path, PathBuf};

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
    /// Relative path to the skill directories, e.g. `./skills/`.
    ///
    /// A *supplement* to the standard location, not a replacement: Codex scans
    /// `skills/` whether or not this is set, and so does this agent. See
    /// [`crate::plugins::skills`].
    #[serde(default)]
    pub skills: Option<String>,
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

/// `marketplace.json` — a catalogue of plugins and where to find them.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketplaceManifest {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<MarketplaceEntry>,
}

/// One plugin offered by a marketplace.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceEntry {
    pub name: String,
    pub source: PluginSource,
    #[serde(default)]
    pub policy: Option<MarketplacePolicy>,
}

impl MarketplaceEntry {
    /// Whether this marketplace actually offers the plugin.
    ///
    /// `NOT_AVAILABLE` means the marketplace lists the entry but withholds it.
    /// Honouring that matters because it is the marketplace's only way to
    /// withdraw a plugin without deleting the entry.
    pub fn is_offered(&self) -> bool {
        !matches!(
            self.policy
                .as_ref()
                .and_then(|policy| policy.installation.as_deref()),
            Some("NOT_AVAILABLE")
        )
    }
}

/// Where a marketplace entry's plugin lives.
///
/// `source` stays a [`String`] rather than an enum on purpose: this agent only
/// resolves `local` sources (a remote source is Codex's job to fetch into the
/// cache — see [`crate::plugins`]), and a strict enum would make one entry with
/// an unknown future source type fail the *whole* marketplace instead of being
/// skipped on its own.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSource {
    pub source: String,
    #[serde(default)]
    pub path: Option<String>,
}

impl PluginSource {
    pub const LOCAL: &'static str = "local";

    /// Whether the source points at a local directory (a working copy) rather
    /// than a fetched/cached copy. Decides whether an uninstall may delete it.
    pub fn is_local(&self) -> bool {
        self.source == Self::LOCAL
    }
}

/// The marketplace's installation policy for one entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplacePolicy {
    /// `NOT_AVAILABLE` | `AVAILABLE` | `INSTALLED_BY_DEFAULT`.
    #[serde(default)]
    pub installation: Option<String>,
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

/// Reads and parses a `marketplace.json`.
pub fn read_marketplace(path: &Path) -> Result<MarketplaceManifest> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        AgentError::from_io(&format!("Failed to read {}", path.display()), error)
    })?;
    serde_json::from_str(&text).map_err(|error| {
        AgentError::internal(format!(
            "{} is not a valid marketplace manifest: {error}",
            path.display()
        ))
    })
}

/// The relative path a marketplace's `source.path` is resolved against.
///
/// The format puts `marketplace.json` at
/// `<root>/.deluxe-agents/plugins/marketplace.json` and resolves `./plugins/foo`
/// against `<root>` — the directory that *contains* `.deluxe-agents/`, not the
/// directory the file sits in and not `.deluxe-agents/plugins/`. For example,
/// `~/.deluxe-agents/plugins/marketplace.json` naming `./plugins/computer-use-local`
/// resolves to `~/plugins/computer-use-local`.
///
/// Returns `None` when `path` is not shaped like a marketplace file, which the
/// caller reports rather than guessing at.
pub fn marketplace_root(path: &Path) -> Option<PathBuf> {
    // <root>/.deluxe-agents/plugins/marketplace.json
    let plugins = path.parent()?; // …/plugins
    let home_dir = plugins.parent()?; // …/.deluxe-agents
    let root = home_dir.parent()?; // <root>
    if home_dir.file_name()? != super::HOME_DIR || plugins.file_name()? != "plugins" {
        return None;
    }
    Some(root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest shaped exactly like the real `figma` plugin's, presentation
    /// block and all — including the fields this agent does not read, because
    /// tolerating them is the property under test.
    ///
    /// `r##"…"##` rather than `r#"…"#`, because the brand colour is a hex string
    /// and its `"#` would close the shorter delimiter.
    const FIGMA: &str = r##"{
      "name": "figma",
      "version": "2.0.20",
      "description": "Figma workflows for design implementation.",
      "author": { "name": "Figma", "url": "https://www.figma.com" },
      "homepage": "https://www.figma.com",
      "repository": "https://github.com/openai/plugins",
      "license": "LicenseRef-Figma-Developer-Terms",
      "keywords": ["figma", "design"],
      "skills": "./skills/",
      "apps": "./.app.json",
      "interface": {
        "displayName": "Figma",
        "shortDescription": "Figma design-to-code workflows",
        "longDescription": "Longer prose for a details page.",
        "developerName": "Figma",
        "category": "Creativity",
        "capabilities": ["Interactive", "Read", "Write"],
        "websiteURL": "https://www.figma.com",
        "privacyPolicyURL": "https://www.figma.com/legal/privacy/",
        "termsOfServiceURL": "https://www.figma.com/legal/developer-terms/",
        "defaultPrompt": ["Inspect a Figma design and implement it in code"],
        "brandColor": "#1ABCFE",
        "composerIcon": "./assets/logo-padded.png",
        "screenshots": []
      }
    }"##;

    #[test]
    fn a_real_manifest_parses() {
        let manifest = parse_plugin(FIGMA, std::path::Path::new("plugin.json")).unwrap();

        assert_eq!(manifest.name, "figma");
        assert_eq!(manifest.version.as_deref(), Some("2.0.20"));
        assert_eq!(manifest.skills.as_deref(), Some("./skills/"));
    }

    #[test]
    fn the_display_name_and_summary_prefer_the_interface_block() {
        let manifest = parse_plugin(FIGMA, std::path::Path::new("plugin.json")).unwrap();

        assert_eq!(manifest.display_name(), "Figma");
        assert_eq!(
            manifest.summary(),
            Some("Figma design-to-code workflows"),
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
        assert!(manifest.skills.is_none());
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

    #[test]
    fn a_marketplace_parses_with_all_three_source_kinds() {
        let text = r#"{
          "name": "openai-curated",
          "interface": { "displayName": "Codex official" },
          "plugins": [
            { "name": "linear", "source": { "source": "local", "path": "./plugins/linear" },
              "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
              "category": "Productivity" },
            { "name": "remote", "source": { "source": "git-subdir",
              "url": "https://github.com/example/p.git", "path": "./plugins/remote", "ref": "main" } },
            { "name": "packaged", "source": { "source": "npm" } }
          ]
        }"#;
        let marketplace = serde_json::from_str::<MarketplaceManifest>(text).unwrap();

        assert_eq!(marketplace.name, "openai-curated");
        assert_eq!(marketplace.plugins.len(), 3);
        assert!(marketplace.plugins[0].source.is_local());
        assert_eq!(
            marketplace.plugins[0].source.path.as_deref(),
            Some("./plugins/linear")
        );
        assert!(!marketplace.plugins[1].source.is_local());
        assert!(marketplace.plugins[1].policy.is_none());
    }

    #[test]
    fn an_entry_the_marketplace_withholds_is_not_offered() {
        let offered = |installation: &str| {
            let text = format!(
                r#"{{"name":"m","plugins":[{{"name":"p","source":{{"source":"local","path":"./p"}},
                   "policy":{{"installation":"{installation}"}}}}]}}"#
            );
            let marketplace = serde_json::from_str::<MarketplaceManifest>(&text).unwrap();
            marketplace.plugins[0].is_offered()
        };

        assert!(offered("AVAILABLE"));
        assert!(offered("INSTALLED_BY_DEFAULT"));
        assert!(
            !offered("NOT_AVAILABLE"),
            "withdrawn entries must be skipped"
        );
    }

    #[test]
    fn the_marketplace_root_is_the_directory_containing_deluxe_agents() {
        // The layout, and the trap the format sets: `./plugins/foo` is
        // resolved against the directory that *contains* `.deluxe-agents/`.
        let root = marketplace_root(std::path::Path::new(
            "/home/u/.deluxe-agents/plugins/marketplace.json",
        ))
        .unwrap();
        assert_eq!(root, std::path::PathBuf::from("/home/u"));

        // The documented resolution, spelled out.
        assert_eq!(
            root.join("plugins/computer-use-local"),
            std::path::PathBuf::from("/home/u/plugins/computer-use-local")
        );
    }

    #[test]
    fn a_repo_marketplace_resolves_against_the_repo_root() {
        let root = marketplace_root(std::path::Path::new(
            "/work/repo/.deluxe-agents/plugins/marketplace.json",
        ))
        .unwrap();
        assert_eq!(root, std::path::PathBuf::from("/work/repo"));
    }

    #[test]
    fn a_file_that_is_not_a_marketplace_has_no_root() {
        // Guessing here would silently point at the wrong directory, so the
        // caller is told instead. Codex's old `.agents` name is not accepted.
        assert!(marketplace_root(std::path::Path::new("/tmp/marketplace.json")).is_none());
        assert!(marketplace_root(std::path::Path::new("/tmp/.agents/marketplace.json")).is_none());
        assert!(marketplace_root(std::path::Path::new(
            "/tmp/.agents/plugins/marketplace.json"
        ))
        .is_none());
    }
}
