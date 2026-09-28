//! Configuration and secret storage.
//!
//! The API key never goes in the config file. It is read from the environment
//! first, then from the OS credential store, and a key typed into the settings
//! panel lives only in memory for that session.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::ContextSettings;
use crate::error::{AgentError, Result};
use crate::plugins::PluginSettings;
use crate::theme::ThemeChoice;
use crate::tools::ToolSettings;

/// Read before the credential store, so a shell export always wins.
pub const API_KEY_ENV: &str = "DESKTOP_AGENT_API_KEY";

/// Overrides the config directory, so a shell export always wins — the same
/// escape hatch [`API_KEY_ENV`] gives the key. A portable install, a second
/// profile, or a test that must not touch the user's real state can point it
/// somewhere else.
pub const CONFIG_DIR_ENV: &str = "DESKTOP_AGENT_CONFIG_DIR";

const KEYRING_SERVICE: &str = "desktop-agent";
const KEYRING_ACCOUNT: &str = "llm-api-key";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub llm: LlmConfig,
    /// Where tools run and how far they are allowed to go.
    pub tools: ToolSettings,
    /// Persisted so the 视图 menu's choice survives a restart.
    pub theme: ThemeChoice,
    /// The model's context window and the share of it that triggers
    /// compaction. Zero tokens disables compaction, which is what a config
    /// written before the feature existed must mean.
    #[serde(default)]
    pub context: ContextSettings,
    /// The projects the sidebar lists, most recently added first.
    ///
    /// Each entry is the directory a new chat in that project is rooted at —
    /// the per-project replacement for the old single working directory. A
    /// `String` rather than a `PathBuf` for the same reason `Session::project`
    /// is one: a folder chosen through the native dialog can be non-UTF-8, and
    /// one such path would fail the whole config write.
    ///
    /// The field-level `serde(default)` is load-bearing: it makes a config
    /// written before projects existed deserialise to an *empty* list, which
    /// [`Config::normalize`] then fills from the working directory that config
    /// already carried. Relying on the container default would seed the
    /// *default* working directory instead, silently moving the user.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Which Codex plugins this agent loads, and where they apply.
    ///
    /// A table of per-plugin switches plus a per-project list, because plugins
    /// come in two scopes; see [`crate::plugins`]. This is also the trust
    /// boundary for a plugin's hooks, which are arbitrary shell commands, so
    /// nothing but this file decides what loads.
    ///
    /// No field-level `serde(default)` is needed the way `projects` needs one:
    /// the container default for this type is already the empty section, so a
    /// config written before plugins existed loads with none enabled.
    pub plugins: PluginSettings,
}

/// The default config carries one project, seeded from the default working
/// directory, so a fresh install opens with a project rather than an empty
/// sidebar. A config read from disk goes through [`Config::normalize`] instead,
/// which is what lets an *older* config migrate its own working directory.
impl Default for Config {
    fn default() -> Self {
        let tools = ToolSettings::default();
        let projects = vec![tools.working_directory.display().to_string()];
        Self {
            llm: LlmConfig::default(),
            tools,
            theme: ThemeChoice::default(),
            context: ContextSettings::default(),
            projects,
            plugins: PluginSettings::default(),
        }
    }
}

impl Config {
    /// Repairs a config that came off disk.
    ///
    /// Duplicate and blank projects are dropped, and an empty list falls back
    /// to the working directory — so a hand-edited file, or one written before
    /// projects existed, still leaves the sidebar with somewhere to point.
    fn normalize(&mut self) {
        self.llm.normalize();
        self.plugins.normalize();

        let mut seen = HashSet::new();
        self.projects
            .retain(|project| !project.trim().is_empty() && seen.insert(project.clone()));
        if self.projects.is_empty() {
            self.projects
                .push(self.tools.working_directory.display().to_string());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    /// Which input modalities the configured model accepts.
    ///
    /// Defaults to text only, so a config written before the field existed
    /// keeps `read_image` unregistered — the safe reading of "the user has not
    /// said this model can see images".
    #[serde(default = "default_input_modalities")]
    pub input: Vec<InputModality>,
    /// The `max_tokens` sent with every request, or `None` to leave the
    /// provider's own ceiling in force.
    ///
    /// Omitted from the file when unset, so a config written before the field
    /// existed — and one where the user never set a budget — carry no key at
    /// all rather than a number a provider would read as a real one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// One input modality a model can accept.
///
/// The vocabulary is closed, mirroring the harness: a model accepts text, or
/// text and images, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    Text,
    Image,
}

fn default_input_modalities() -> Vec<InputModality> {
    vec![InputModality::Text]
}

impl LlmConfig {
    /// Whether the model declares image input.
    ///
    /// An empty list reads as "not declared", which is text-only: a model that
    /// cannot see images must never be offered `read_image`.
    pub fn supports_images(&self) -> bool {
        self.input.contains(&InputModality::Image)
    }

    /// Drops an empty modality list back to text-only.
    ///
    /// A hand-edited config can leave `input = []`, which means the same thing
    /// as text-only at runtime but would render as both boxes unticked.
    fn normalize(&mut self) {
        if self.input.is_empty() {
            self.input = default_input_modalities();
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            input: default_input_modalities(),
            max_output_tokens: None,
        }
    }
}

/// The per-user config directory, created on demand.
pub fn config_dir() -> Option<PathBuf> {
    // An override beats the OS lookup, so a test or a portable install can
    // redirect everything under it — config, sessions, attachments — at once.
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    directories::ProjectDirs::from("", "", "desktop-agent")
        .map(|dirs| dirs.config_dir().to_path_buf())
}

/// The `config.toml` path, or `None` when the system offers no config
/// directory.
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.toml"))
}

/// Reads the config, falling back to defaults.
///
/// A malformed file is a warning rather than a startup failure: the panel is
/// where the user fixes it, so refusing to open the panel would be a trap.
pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };

    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };

    match toml::from_str::<Config>(&text) {
        Ok(mut config) => {
            config.normalize();
            config
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring malformed config");
            Config::default()
        }
    }
}

/// Writes the config to `path`, creating the parent directory if needed.
///
/// The only writer, and deliberately path-in: the GUI holds the path the app
/// was started with and writes to exactly that file rather than re-deriving it
/// from the environment. That is also what lets a test aim a save at a temp
/// file, which [`config_path`] alone would not — it reads the process
/// environment, and a test cannot set that without mutating the process.
pub fn save_to(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AgentError::from_io("Failed to create the config directory", error))?;
    }

    let text = toml::to_string_pretty(config).map_err(|error| {
        AgentError::internal(format!("Failed to serialise the config: {error}"))
    })?;

    std::fs::write(path, text)
        .map_err(|error| AgentError::from_io("Failed to write the config", error))?;

    tracing::info!(path = %path.display(), "saved config");
    Ok(())
}

/// The API key, from the environment or the OS credential store.
pub fn resolve_api_key() -> Option<String> {
    if let Ok(key) = std::env::var(API_KEY_ENV) {
        if !key.trim().is_empty() {
            return Some(key);
        }
    }
    load_api_key()
}

fn load_api_key() -> Option<String> {
    match keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .and_then(|entry| entry.get_password())
    {
        Ok(key) if !key.trim().is_empty() => Some(key),
        Ok(_) => None,
        Err(error) => {
            tracing::debug!(%error, "no API key in the credential store");
            None
        }
    }
}

/// Stores the API key in the OS credential store.
pub fn store_api_key(key: &str) -> Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .and_then(|entry| entry.set_password(key))
        .map_err(|error| AgentError::internal(format!("Failed to store the API key: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_config_grants_full_access_to_the_home_directory() {
        let config = Config::default();
        assert!(config.tools.working_directory.is_absolute());
        assert!(config.tools.block_destructive_commands);
    }

    #[test]
    fn the_default_config_seeds_one_project_from_the_working_directory() {
        // A fresh install must open with a project rather than an empty
        // sidebar, so the default list is never empty.
        let config = Config::default();
        assert_eq!(
            config.projects,
            vec![config.tools.working_directory.display().to_string()]
        );
    }

    #[test]
    fn a_config_written_before_projects_existed_migrates_the_working_directory() {
        // No `projects` key: the field-level `serde(default)` leaves it empty,
        // and `normalize` fills it from the working directory this config
        // already carried — so an existing user keeps working where they were,
        // rather than being moved to the default directory.
        let mut config: Config =
            toml::from_str("[tools]\nworkingDirectory = \"/tmp/workspace\"\n").unwrap();
        assert!(config.projects.is_empty());

        config.normalize();
        assert_eq!(config.projects, vec!["/tmp/workspace".to_string()]);
    }

    #[test]
    fn normalize_drops_blank_and_duplicate_projects() {
        let mut config = Config {
            projects: vec!["/a".into(), "  ".into(), "/a".into(), "/b".into()],
            ..Default::default()
        };

        config.normalize();

        assert_eq!(config.projects, vec!["/a".to_string(), "/b".to_string()]);
    }

    #[test]
    fn normalize_reseeds_an_empty_project_list() {
        let mut config = Config::default();
        config.tools.working_directory = PathBuf::from("/tmp/only");
        config.projects.clear();

        config.normalize();

        assert_eq!(config.projects, vec!["/tmp/only".to_string()]);
    }

    #[test]
    fn projects_round_trip_through_toml() {
        let config = Config {
            projects: vec!["/a".into(), "/b".into()],
            ..Default::default()
        };

        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert_eq!(parsed.projects, vec!["/a".to_string(), "/b".to_string()]);
    }

    #[test]
    fn a_config_round_trips_through_toml() {
        let mut config = Config::default();
        config.tools.working_directory = PathBuf::from("/tmp/workspace");
        config.tools.block_destructive_commands = false;
        config.llm.model = "some-model".into();
        config.context.context_limit = 131_072;
        config.context.threshold_percent = 45;

        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert_eq!(
            parsed.tools.working_directory,
            PathBuf::from("/tmp/workspace")
        );
        assert!(!parsed.tools.block_destructive_commands);
        assert_eq!(parsed.llm.model, "some-model");
        assert_eq!(parsed.context.context_limit, 131_072);
        assert_eq!(parsed.context.threshold_percent, 45);
    }

    #[test]
    fn an_empty_toml_falls_back_to_defaults() {
        let parsed: Config = toml::from_str("").unwrap();
        assert_eq!(parsed.llm.base_url, LlmConfig::default().base_url);
        assert_eq!(
            parsed.tools.working_directory,
            ToolSettings::default().working_directory
        );
        // No context section: compaction stays off until the user turns it on.
        assert_eq!(parsed.context.context_limit, 0);
    }

    #[test]
    fn a_config_written_before_the_context_section_existed_defaults_to_no_compaction() {
        let parsed: Config = toml::from_str("[llm]\nmodel = \"m\"\n").unwrap();
        assert_eq!(parsed.context, ContextSettings::default());
        assert_eq!(parsed.context.context_limit, 0);
    }

    #[test]
    fn a_config_still_carrying_the_removed_reasoning_toggle_loads() {
        // The toggle is gone — reasoning is always rendered — so an old file
        // that still names it must load rather than fail on the stray key.
        let parsed: Config = toml::from_str("[llm]\nmodel = \"m\"\nshowReasoning = false\n").unwrap();
        assert_eq!(parsed.llm.model, "m");
    }

    #[test]
    fn a_config_written_before_the_tools_section_existed_still_loads() {
        // Only `[llm]` is present, so `tools` must come entirely from defaults.
        let parsed: Config = toml::from_str("[llm]\nmodel = \"m\"\n").unwrap();
        assert_eq!(parsed.llm.model, "m");
        assert!(parsed.tools.block_destructive_commands);
    }

    #[test]
    fn the_default_model_accepts_text_only() {
        let config = Config::default();
        assert_eq!(config.llm.input, vec![InputModality::Text]);
        assert!(!config.llm.supports_images());
    }

    #[test]
    fn image_input_round_trips_through_toml() {
        let mut config = Config::default();
        config.llm.input = vec![InputModality::Text, InputModality::Image];

        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert_eq!(
            parsed.llm.input,
            vec![InputModality::Text, InputModality::Image]
        );
        assert!(parsed.llm.supports_images());
    }

    #[test]
    fn a_config_written_before_the_modalities_existed_is_text_only() {
        // No `input` key: the model must not be handed `read_image`.
        let parsed: Config = toml::from_str("[llm]\nmodel = \"m\"\n").unwrap();
        assert_eq!(parsed.llm.input, vec![InputModality::Text]);
        assert!(!parsed.llm.supports_images());
    }

    #[test]
    fn an_empty_modality_list_normalises_back_to_text_only() {
        let mut config: Config = toml::from_str("[llm]\nmodel = \"m\"\ninput = []\n").unwrap();
        assert!(config.llm.input.is_empty());
        config.llm.normalize();
        assert_eq!(config.llm.input, vec![InputModality::Text]);
    }

    #[test]
    fn a_config_written_before_plugins_existed_enables_none() {
        // The whole `[plugins]` section is absent, so nothing loads — which is
        // the only safe reading: a plugin's hooks are arbitrary shell commands.
        let parsed: Config = toml::from_str("[llm]\nmodel = \"m\"\n").unwrap();

        assert!(parsed.plugins.plugins.is_empty());
        assert!(parsed.plugins.projects.is_empty());
    }

    #[test]
    fn the_plugins_section_round_trips_through_toml() {
        let mut config = Config::default();
        config.plugins.set_enabled("figma@openai-curated", true);
        config.plugins.set_enabled("chrome@openai-bundled", false);
        config.plugins.projects.insert(
            "/work/repo".into(),
            vec!["repo-triage@my-team".into()],
        );

        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert!(parsed.plugins.plugins["figma@openai-curated"].enabled);
        assert!(
            !parsed.plugins.plugins["chrome@openai-bundled"].enabled,
            "an off switch is a row, which is what makes it reversible"
        );
        assert_eq!(
            parsed.plugins.projects.get("/work/repo"),
            Some(&vec!["repo-triage@my-team".to_string()])
        );
    }

    #[test]
    fn normalize_repairs_the_plugins_section() {
        // A hand-edited config is the normal way this section gets written, so
        // a stray space or a blank key must not survive into an unresolvable id.
        let mut config: Config = toml::from_str(
            "[plugins.\" figma@openai-curated \"]\nenabled = true\n\n[plugins.\"   \"]\nenabled = true\n",
        )
        .unwrap();

        config.normalize();

        assert_eq!(
            config.plugins.plugins.keys().collect::<Vec<_>>(),
            vec!["figma@openai-curated"]
        );
    }

    #[test]
    fn the_documented_plugins_shape_loads() {
        let text = r#"
            [plugins."computer-use@openai-bundled"]
            enabled = true

            [plugins."figma@openai-curated"]
            enabled = true

            [plugins.projects]
            "/work/repo" = ["repo-triage@my-team"]
        "#;
        let mut config: Config = toml::from_str(text).unwrap();
        config.normalize();

        // The table and the project list stay apart here; what each scope
        // *reaches* is decided by `plugins::discover` and tested there.
        assert_eq!(config.plugins.plugins.len(), 2);
        assert!(config.plugins.plugins["computer-use@openai-bundled"].enabled);
        assert!(config.plugins.plugins["figma@openai-curated"].enabled);
        assert_eq!(
            config.plugins.projects.get("/work/repo"),
            Some(&vec!["repo-triage@my-team".to_string()])
        );
    }

    #[test]
    fn save_to_writes_the_config_where_it_is_told() {
        // What the GUI relies on: a save aimed at a path writes that path, and
        // creates the directory on the way, without consulting the environment.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let mut config = Config::default();
        config.llm.model = "some-model".into();

        save_to(&path, &config).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.llm.model, "some-model");
        assert!(path.parent().unwrap().is_dir(), "the parent directory is created");
    }
}
