//! Configuration and secret lookup.
//!
//! The host stores no model endpoint, no model name, and no API key: the whole
//! provider model is owned by the `llm-provider` Component, which keeps it in
//! its own configuration file and resolves credentials through the host's
//! `get-secret` capability. What is left here is the retry policy and the
//! generic credential lookup that capability is built on.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::ContextSettings;
use crate::error::{AgentError, Result};
use crate::plugins::PluginSettings;
use crate::theme::ThemeChoice;
use crate::tools::ToolSettings;

/// Overrides the config directory, so a shell export always wins. A portable
/// install, a second profile, or a test that must not touch the user's real
/// state can point it somewhere else.
pub const CONFIG_DIR_ENV: &str = "DELUXE_AGENT_CONFIG_DIR";

/// The most additional model-request attempts a config may enable.
pub const MAX_RETRY_COUNT: u32 = 10;

const KEYRING_SERVICE: &str = "deluxe-agent";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub llm: LlmConfig,
    /// Where tools run and how far they are allowed to go.
    pub tools: ToolSettings,
    /// Persisted so the 视图 menu's choice survives a restart.
    pub theme: ThemeChoice,
    /// When and how aggressively to compact. The window itself is not here:
    /// it belongs to the model provider Component, which reports it through
    /// `describe()`.
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
    /// Which Wasmtime plugins this agent loads, and where they apply.
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

/// The model retry policy the host still owns.
///
/// Everything else about reaching a model — the endpoint, the model name, the
/// key, the context window — is owned by the `llm-provider` Component and lives
/// in that plugin's own configuration file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LlmConfig {
    /// Additional attempts made after a failed model request. Defaults to
    /// three and is bounded by [`MAX_RETRY_COUNT`].
    pub retry_count: u32,
    /// Whether failed model requests should keep retrying until cancelled.
    pub retry_forever: bool,
}

impl LlmConfig {
    fn normalize(&mut self) {
        self.retry_count = self.retry_count.min(MAX_RETRY_COUNT);
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            retry_count: 3,
            retry_forever: false,
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
    directories::ProjectDirs::from("", "", "deluxe-agent")
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

/// Reads a credential by name, from the environment or the OS credential store.
///
/// The environment variable is `DELUXE_AGENT_SECRET_<NAME>`, where `<NAME>` is
/// the uppercased name with every non-alphanumeric byte replaced by `_`. `None`
/// means the name resolved to nothing — including an unavailable credential
/// store, which is logged rather than surfaced so a missing keyring cannot fail
/// a run. This backs the host's `get-secret` capability, which the model
/// provider Component calls for a key it does not hold inline.
pub fn resolve_secret(name: &str) -> Option<String> {
    if let Some(value) = secret_from_env(name) {
        return Some(value);
    }
    match keyring::Entry::new(KEYRING_SERVICE, name).and_then(|entry| entry.get_password()) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        Ok(_) => None,
        Err(error) => {
            tracing::debug!(%error, name, "no secret in the credential store");
            None
        }
    }
}

/// Maps a secret name onto its environment variable, or `None` when unset.
fn secret_from_env(name: &str) -> Option<String> {
    let mut variable = String::from("DELUXE_AGENT_SECRET_");
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() {
            variable.push(byte.to_ascii_uppercase() as char);
        } else {
            variable.push('_');
        }
    }
    std::env::var(variable)
        .ok()
        .filter(|value| !value.trim().is_empty())
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
        config.llm.retry_count = 4;
        config.llm.retry_forever = true;
        config.context.threshold_percent = 45;
        config.context.keep_recent_turns = 3;

        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert_eq!(
            parsed.tools.working_directory,
            PathBuf::from("/tmp/workspace")
        );
        assert!(!parsed.tools.block_destructive_commands);
        assert_eq!(parsed.llm.retry_count, 4);
        assert!(parsed.llm.retry_forever);
        assert_eq!(parsed.context.threshold_percent, 45);
        assert_eq!(parsed.context.keep_recent_turns, 3);
    }

    #[test]
    fn an_empty_toml_falls_back_to_defaults() {
        let parsed: Config = toml::from_str("").unwrap();
        assert_eq!(
            parsed.tools.working_directory,
            ToolSettings::default().working_directory
        );
        // No context section: the compaction policy comes from defaults, and a
        // file without the tail field keeps one rather than folding everything.
        assert_eq!(parsed.context, ContextSettings::default());
        assert_eq!(parsed.context.keep_recent_turns, 2);
        assert_eq!(parsed.llm.retry_count, 3);
        assert!(!parsed.llm.retry_forever);
    }

    #[test]
    fn a_config_written_before_the_context_section_existed_uses_the_default_policy() {
        let parsed: Config = toml::from_str("[llm]\nretryCount = 2\n").unwrap();
        assert_eq!(parsed.context, ContextSettings::default());
    }

    #[test]
    fn a_config_still_carrying_removed_llm_fields_loads() {
        // The endpoint, model, modality list, and output budget all moved into
        // the provider Component, so a file that still names them must load
        // rather than fail on the stray keys.
        let parsed: Config = toml::from_str(
            "[llm]\nbaseUrl = \"https://example.com\"\nmodel = \"m\"\nmaxOutputTokens = 8192\ninput = [\"text\"]\nretryCount = 2\n",
        )
        .unwrap();
        assert_eq!(parsed.llm.retry_count, 2);
    }

    #[test]
    fn a_config_written_before_the_tools_section_existed_still_loads() {
        // Only `[llm]` is present, so `tools` must come entirely from defaults.
        let parsed: Config = toml::from_str("[llm]\nretryCount = 2\n").unwrap();
        assert_eq!(parsed.llm.retry_count, 2);
        assert!(parsed.tools.block_destructive_commands);
    }

    #[test]
    fn retry_count_is_bounded_when_loading_a_hand_edited_config() {
        let mut config: Config = toml::from_str("[llm]\nretryCount = 99\n").unwrap();

        config.normalize();

        assert_eq!(config.llm.retry_count, MAX_RETRY_COUNT);
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
        config.plugins.set_enabled("notes@personal", true);
        config.plugins.set_enabled("formatter@bundled", false);
        config
            .plugins
            .projects
            .insert("/work/repo".into(), vec!["deploy@my-team".into()]);

        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert!(parsed.plugins.plugins["notes@personal"]);
        assert!(
            !parsed.plugins.plugins["formatter@bundled"],
            "an off switch is a row, which is what makes it reversible"
        );
        assert_eq!(
            parsed.plugins.projects.get("/work/repo"),
            Some(&vec!["deploy@my-team".to_string()])
        );
    }

    #[test]
    fn normalize_repairs_the_plugins_section() {
        // A hand-edited config is the normal way this section gets written, so
        // a stray space or a blank key must not survive into an unresolvable id.
        let mut config: Config =
            toml::from_str("[plugins]\n\" notes@personal \" = true\n\"   \" = true\n").unwrap();

        config.normalize();

        assert_eq!(
            config.plugins.plugins.keys().collect::<Vec<_>>(),
            vec!["notes@personal"]
        );
    }

    #[test]
    fn the_documented_plugins_shape_loads() {
        let text = r#"
            [plugins]
            "linter@bundled" = true
            "notes@personal" = true

            [plugins.projects]
            "/work/repo" = ["deploy@my-team"]
        "#;
        let mut config: Config = toml::from_str(text).unwrap();
        config.normalize();

        // The table and the project list stay apart here; what each scope
        // *reaches* is decided by `plugins::discover` and tested there.
        assert_eq!(config.plugins.plugins.len(), 2);
        assert!(config.plugins.plugins["linter@bundled"]);
        assert!(config.plugins.plugins["notes@personal"]);
        assert_eq!(
            config.plugins.projects.get("/work/repo"),
            Some(&vec!["deploy@my-team".to_string()])
        );
    }

    #[test]
    fn save_to_writes_the_config_where_it_is_told() {
        // What the GUI relies on: a save aimed at a path writes that path, and
        // creates the directory on the way, without consulting the environment.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let mut config = Config::default();
        config.llm.retry_count = 7;

        save_to(&path, &config).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.llm.retry_count, 7);
        assert!(
            path.parent().unwrap().is_dir(),
            "the parent directory is created"
        );
    }
}
