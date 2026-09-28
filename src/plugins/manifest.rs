//! The Codex plugin manifest, as this agent reads it.
//!
//! The shapes here mirror `codex-rs`'s plugin format, because the whole point is
//! to consume plugins that were written for Codex rather than for this agent. A
//! field this agent does not understand must therefore be *ignored*, never a
//! parse failure — Codex's schema is still growing, and a plugin that adds a key
//! tomorrow must not stop loading here today. So every field except `name` is
//! optional and unknown keys are dropped by serde's default behaviour.
//!
//! Only the fields this agent acts on are modelled. The format also carries
//! icons, brand colours, screenshots, connector ids and the like; none of that
//! is read here, so it is not declared, and a manifest full of it still parses.
//! A field is added to these types when something starts reading it.
//!
//! Three files are modelled: `.codex-plugin/plugin.json`, which describes one
//! plugin; `marketplace.json`, which only says where plugins are and whether
//! they are offered; and `.mcp.json`, which names the MCP servers the plugin
//! brings. They are separate types because they answer different questions and
//! are read at different times.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{AgentError, Result};

/// `.codex-plugin/plugin.json` — the one file a plugin cannot omit.
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
}

impl PluginManifest {
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
            self.policy.as_ref().and_then(|policy| policy.installation.as_deref()),
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

/// `.mcp.json` — the MCP servers a plugin brings.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpServersFile {
    /// The map is named `mcpServers` in the format. The snake_case spelling
    /// Codex's own `config.toml` uses is accepted too, because a hand-written
    /// file is as likely to reach for it.
    #[serde(default, rename = "mcpServers", alias = "mcp_servers")]
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// One MCP server, in either transport the format describes.
///
/// One struct for both rather than an enum, for the reason [`PluginSource`] is
/// a `String`: an entry whose `type` this agent has never heard of must be
/// skipped on its own, not take the whole file down with it.
///
/// The field names are spelled out one by one instead of using
/// `rename_all`, because the file mixes conventions — `mcpServers` is camelCase
/// while `oauth_resource` and `startup_timeout_sec` are snake_case. Each field
/// therefore names its wire spelling and accepts the other as an alias.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// `stdio` | `http`. Absent means stdio; see [`McpServerConfig::is_http`].
    #[serde(default, rename = "type")]
    pub transport: Option<String>,
    /// The endpoint, for an `http` server.
    #[serde(default)]
    pub url: Option<String>,
    /// Set when the server needs an OAuth flow. This agent does not do OAuth,
    /// so a server carrying this is reported and skipped rather than attempted
    /// — a silent failure here would look like a broken plugin.
    #[serde(default, rename = "oauth_resource", alias = "oauthResource")]
    pub oauth_resource: Option<String>,
    /// The executable, for a stdio server.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// The child's working directory. Relative means relative to the plugin
    /// root, which is what makes a plugin's `./scripts/server.py` resolve.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// How long the handshake may take, for a server that is slow to start.
    ///
    /// The format's `tool_timeout_sec` is deliberately not read: the host
    /// already bounds every tool call with its own timeout, and a second
    /// authority over the same thing would only make the effective limit
    /// depend on which one happened to be smaller.
    #[serde(default, rename = "startup_timeout_sec", alias = "startupTimeoutSec")]
    pub startup_timeout_sec: Option<u64>,
}

impl McpServerConfig {
    /// Which transport to speak.
    ///
    /// `type` is optional and its absence means stdio — the same default Codex
    /// uses. A server with a `url` and no `command` is read as `http` even
    /// without the field, because that is the only thing it can be.
    pub fn is_http(&self) -> bool {
        match self.transport.as_deref() {
            Some("http") => true,
            Some("stdio") => false,
            _ => self.command.is_none() && self.url.is_some(),
        }
    }
}

/// The `<plugin>/.codex-plugin/` directory name, relative to a plugin root.
pub const MANIFEST_DIR: &str = ".codex-plugin";
/// The manifest file inside [`MANIFEST_DIR`].
pub const MANIFEST_FILE: &str = "plugin.json";
/// The MCP servers a plugin brings, at the plugin root.
///
/// There is no manifest field pointing at this file — `plugin.json` carries
/// `skills` and `apps` paths but nothing for MCP, and no plugin in a real
/// install declares one — so the name is fixed.
pub const MCP_FILE: &str = ".mcp.json";

/// Reads and parses a plugin manifest from a plugin root.
pub fn read_plugin(root: &Path) -> Result<PluginManifest> {
    let path = root.join(MANIFEST_DIR).join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| AgentError::from_io(&format!("Failed to read {}", path.display()), error))?;
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
    let text = std::fs::read_to_string(path)
        .map_err(|error| AgentError::from_io(&format!("Failed to read {}", path.display()), error))?;
    serde_json::from_str(&text).map_err(|error| {
        AgentError::internal(format!(
            "{} is not a valid marketplace manifest: {error}",
            path.display()
        ))
    })
}

/// The MCP servers a plugin declares, keyed by the name they are exposed under.
///
/// A plugin with no `.mcp.json` — which is most of them — is not an error and
/// not a warning: it is `Ok` with nothing in it. A file that exists but does not
/// parse *is* an error, and the caller reports it; the plugin still loads, just
/// without its servers.
pub fn read_mcp_servers(root: &Path) -> Result<BTreeMap<String, McpServerConfig>> {
    let path = root.join(MCP_FILE);
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| AgentError::from_io(&format!("Failed to read {}", path.display()), error))?;
    Ok(parse_mcp_servers(&text, &path)?.servers)
}

fn parse_mcp_servers(text: &str, path: &Path) -> Result<McpServersFile> {
    serde_json::from_str(text).map_err(|error| {
        AgentError::internal(format!(
            "{} is not a valid MCP server file: {error}",
            path.display()
        ))
    })
}

/// The relative path a marketplace's `source.path` is resolved against.
///
/// The format puts `marketplace.json` at `<root>/.agents/plugins/marketplace.json`
/// and resolves `./plugins/foo` against `<root>` — the directory that *contains*
/// `.agents/`, not the directory the file sits in and not `.agents/plugins/`.
/// Verified against a real install: `~/.agents/plugins/marketplace.json` naming
/// `./plugins/computer-use-local` resolves to `~/plugins/computer-use-local`.
///
/// Returns `None` when `path` is not shaped like a marketplace file, which the
/// caller reports rather than guessing at.
pub fn marketplace_root(path: &Path) -> Option<PathBuf> {
    // <root>/.agents/plugins/marketplace.json
    let plugins = path.parent()?; // …/plugins
    let agents = plugins.parent()?; // …/.agents
    let root = agents.parent()?; // <root>
    if agents.file_name()? != ".agents" || plugins.file_name()? != "plugins" {
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
        assert!(error.message.contains("plugin manifest"), "{}", error.message);
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
        assert_eq!(marketplace.plugins[0].source.path.as_deref(), Some("./plugins/linear"));
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
        assert!(!offered("NOT_AVAILABLE"), "withdrawn entries must be skipped");
    }

    #[test]
    fn the_marketplace_root_is_the_directory_containing_agents() {
        // The real layout, and the trap the format sets: `./plugins/foo` is
        // resolved against the directory that *contains* `.agents/`.
        let root = marketplace_root(std::path::Path::new(
            "/home/u/.agents/plugins/marketplace.json",
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
            "/work/repo/.agents/plugins/marketplace.json",
        ))
        .unwrap();
        assert_eq!(root, std::path::PathBuf::from("/work/repo"));
    }

    #[test]
    fn a_file_that_is_not_a_marketplace_has_no_root() {
        // Guessing here would silently point at the wrong directory, so the
        // caller is told instead.
        assert!(marketplace_root(std::path::Path::new("/tmp/marketplace.json")).is_none());
        assert!(marketplace_root(std::path::Path::new("/tmp/.agents/marketplace.json")).is_none());
    }

    /// The real `figma` plugin's `.mcp.json`, verbatim.
    const FIGMA_MCP: &str = r#"{
      "mcpServers": {
        "figma": {
          "type": "http",
          "url": "https://mcp.figma.com/mcp",
          "oauth_resource": "https://mcp.figma.com/mcp"
        }
      }
    }"#;

    /// The real stdio shape, including the two timeout fields — one of which
    /// this agent reads and one it deliberately ignores, so both must at least
    /// parse.
    const STDIO_MCP: &str = r#"{
      "mcpServers": {
        "taskScheduler": {
          "command": "python",
          "args": ["./scripts/mcp_server.py"],
          "cwd": ".",
          "env": { "PYTHONUTF8": "1" },
          "startup_timeout_sec": 20,
          "tool_timeout_sec": 60
        }
      }
    }"#;

    fn servers(text: &str) -> BTreeMap<String, McpServerConfig> {
        parse_mcp_servers(text, std::path::Path::new(".mcp.json"))
            .unwrap()
            .servers
    }

    #[test]
    fn a_real_http_mcp_file_parses() {
        let servers = servers(FIGMA_MCP);
        let figma = &servers["figma"];

        assert_eq!(figma.url.as_deref(), Some("https://mcp.figma.com/mcp"));
        assert_eq!(
            figma.oauth_resource.as_deref(),
            Some("https://mcp.figma.com/mcp"),
            "the OAuth marker is what tells the host to skip this server"
        );
        assert!(figma.is_http());
    }

    #[test]
    fn a_real_stdio_mcp_file_parses() {
        let servers = servers(STDIO_MCP);
        let scheduler = &servers["taskScheduler"];

        assert_eq!(scheduler.command.as_deref(), Some("python"));
        assert_eq!(scheduler.args, vec!["./scripts/mcp_server.py"]);
        assert_eq!(scheduler.cwd.as_deref(), Some("."));
        assert_eq!(scheduler.env.get("PYTHONUTF8").map(String::as_str), Some("1"));
        assert_eq!(
            scheduler.startup_timeout_sec,
            Some(20),
            "a server that declares how slow it is gets to say so"
        );
        assert!(
            !scheduler.is_http(),
            "a server with a command and no type is stdio"
        );
    }

    #[test]
    fn the_camel_case_spelling_of_the_snake_case_fields_is_accepted() {
        // The format mixes conventions, so each field takes both spellings
        // rather than one of them being silently dropped.
        let servers = servers(
            r#"{"mcpServers":{"x":{"type":"http","url":"https://x/mcp",
                "oauthResource":"https://x","startupTimeoutSec":5}}}"#,
        );
        let x = &servers["x"];

        assert_eq!(x.oauth_resource.as_deref(), Some("https://x"));
        assert_eq!(x.startup_timeout_sec, Some(5));
    }

    #[test]
    fn the_transport_defaults_to_stdio_and_an_explicit_type_wins() {
        let kind = |text: &str| servers(text).into_values().next().unwrap().is_http();

        assert!(!kind(r#"{"mcpServers":{"x":{"command":"node"}}}"#), "no type means stdio");
        assert!(kind(r#"{"mcpServers":{"x":{"url":"https://x/mcp"}}}"#), "a url with no command can only be http");
        assert!(
            !kind(r#"{"mcpServers":{"x":{"type":"stdio","url":"https://x/mcp"}}}"#),
            "an explicit type is not overruled by a stray url"
        );
    }

    #[test]
    fn a_file_that_is_not_an_mcp_file_is_an_error() {
        let error = parse_mcp_servers("{", std::path::Path::new(".mcp.json")).unwrap_err();
        assert!(error.message.contains("MCP server file"), "{}", error.message);
    }

    #[test]
    fn a_plugin_without_an_mcp_file_declares_no_servers() {
        // The common case, and not a warning: most plugins bring no server.
        let root = tempfile::tempdir().unwrap();
        assert!(read_mcp_servers(root.path()).unwrap().is_empty());

        std::fs::write(
            root.path().join(MCP_FILE),
            r#"{"mcpServers":{"s":{"command":"node"}}}"#,
        )
        .unwrap();
        assert_eq!(read_mcp_servers(root.path()).unwrap().len(), 1);
    }
}
