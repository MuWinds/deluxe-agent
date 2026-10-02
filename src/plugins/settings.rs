//! Which plugins to load, and where.
//!
//! Two things, because plugins come in two scopes. The table of per-plugin
//! entries applies in every project; `projects` narrows an id to one project.
//! Both name a plugin the same way — `name@marketplace` — which is the identity
//! Codex itself uses.
//!
//! # Why a switch and not a list
//!
//! The entries are shaped after Codex's own config:
//!
//! ```toml
//! [plugins."figma@openai-curated"]
//! enabled = true
//! ```
//!
//! `enabled` is a switch rather than a membership test, and that is the whole
//! point of the shape. With a bare list of enabled ids, "switched off" and
//! "never installed" are the same absence — so switching a plugin off would be
//! indistinguishable from deleting it, and nothing could offer it back.
//!
//! This file is the *only* place the load decision is made. In particular the
//! decision is never read from a repository: a cloned repo can ship plugin
//! *definitions* in `<repo>/.deluxe-agents/plugins/`. Executable behavior is still
//! isolated behind the Wasm manifest and explicit capability permissions, so
//! listing an id here is the act of trust and can only be performed by the
//! person who owns this config.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The `[plugins]` section of this agent's config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginSettings {
    /// Every plugin this agent knows about, keyed by `name@marketplace`.
    ///
    /// `projects` is the one reserved key in this table, and it cannot collide:
    /// a plugin id always carries an `@`, which no bare TOML key can. serde
    /// matches the named field first and hands it the rest, so `projects` lands
    /// here as a field and never as a plugin nobody can resolve.
    #[serde(flatten)]
    pub plugins: BTreeMap<String, PluginEntry>,
    /// Plugin ids loaded only in the named project, keyed by the project root.
    ///
    /// The key must match the entry in `Config::projects` — the same directory
    /// spelled the same way — because that string is what identifies a project
    /// everywhere else in this program. [`project_key`] is the normal form both
    /// sides are put through.
    pub projects: BTreeMap<String, Vec<String>>,
    /// Project-scoped plugin ids that are installed but explicitly disabled.
    #[serde(default)]
    pub disabled_projects: BTreeMap<String, Vec<String>>,
}

/// One plugin's row in the `[plugins]` table.
///
/// Unknown keys are dropped rather than fatal, the same rule the plugin manifest
/// follows and for the same reason: this section mirrors Codex's, and Codex's is
/// still growing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginEntry {
    /// Whether this agent loads the plugin.
    ///
    /// Absent means off. An id only reaches this table because something wrote
    /// it there, and the switch is what says what that was meant to do — so a
    /// table with nothing in it is not an instruction to load anything.
    pub enabled: bool,
}

impl PluginSettings {
    /// Turns one plugin on or off, creating its row if it has none.
    ///
    /// The row is kept when switched off rather than removed: keeping "off" and
    /// "never installed" distinguishable is the whole reason this section is a
    /// table of switches, and it is what lets the plugins window offer a
    /// disabled plugin back. A blank id is ignored — there is nothing to name.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        self.plugins.insert(id.to_string(), PluginEntry { enabled });
    }

    /// Turns one plugin on or off only for `project`.
    pub fn set_project_enabled(&mut self, project: &Path, id: &str, enabled: bool) {
        let id = id.trim();
        let project = project_key(project);
        if id.is_empty() || project.is_empty() {
            return;
        }
        remove_id(self.projects.entry(project.clone()).or_default(), id);
        remove_id(
            self.disabled_projects.entry(project.clone()).or_default(),
            id,
        );
        let target = if enabled {
            &mut self.projects
        } else {
            &mut self.disabled_projects
        };
        target.entry(project).or_default().push(id.to_string());
    }

    /// Repairs a hand-edited section.
    ///
    /// Blank ids are dropped, so a stray key does not become a plugin nobody can
    /// resolve. Project keys are normalised the same way lookups are, so a
    /// trailing separator in the config still matches, and duplicates are
    /// collapsed.
    ///
    /// A plugin switched off globally is also struck from every project list.
    /// Off has to mean off: a switch a project can quietly override is worse
    /// than no switch at all, because the plugins window would report a plugin
    /// as disabled while an agent went on loading it.
    pub fn normalize(&mut self) {
        let plugins = std::mem::take(&mut self.plugins);
        self.plugins = plugins
            .into_iter()
            .filter(|(id, _)| !id.trim().is_empty())
            // Store the trimmed form, so a stray space does not make an id
            // unresolvable.
            .map(|(id, entry)| (id.trim().to_string(), entry))
            .collect();

        let off: HashSet<&str> = self
            .plugins
            .iter()
            .filter(|(_, entry)| !entry.enabled)
            .map(|(id, _)| id.as_str())
            .collect();

        let projects = std::mem::take(&mut self.projects);
        self.projects = projects
            .into_iter()
            .filter_map(|(project, mut ids)| {
                let key = project_key(Path::new(&project));
                if key.is_empty() {
                    return None;
                }
                dedup(&mut ids);
                ids.retain(|id| !off.contains(id.as_str()));
                Some((key, ids))
            })
            .collect();

        let disabled_projects = std::mem::take(&mut self.disabled_projects);
        self.disabled_projects = disabled_projects
            .into_iter()
            .filter_map(|(project, mut ids)| {
                let key = project_key(Path::new(&project));
                if key.is_empty() {
                    return None;
                }
                dedup(&mut ids);
                ids.retain(|id| {
                    !off.contains(id.as_str())
                        && !self
                            .projects
                            .get(&key)
                            .is_some_and(|enabled| enabled.iter().any(|item| item == id))
                });
                Some((key, ids))
            })
            .collect();
    }
}

/// The `[plugins]` map for these ids, every one of them switched on.
///
/// Test-only: a test almost always wants "these plugins, all on", and spelling
/// the map out at thirty call sites buries what each test is actually about.
#[cfg(test)]
pub fn entries(ids: &[&str]) -> BTreeMap<String, PluginEntry> {
    ids.iter()
        .map(|id| ((*id).to_string(), PluginEntry { enabled: true }))
        .collect()
}

/// The key a project is looked up under.
///
/// Trims surrounding whitespace and trailing separators, so `C:\work\repo\` and
/// `C:\work\repo` are one project. Case is deliberately *not* folded: the
/// config's `projects` list is literal, and quietly case-folding here would make
/// this key disagree with the string the rest of the program matches on.
pub fn project_key(project: &Path) -> String {
    project
        .to_string_lossy()
        .trim()
        .trim_end_matches(['/', '\\'])
        .to_string()
}

fn dedup(ids: &mut Vec<String>) {
    let mut seen = HashSet::new();
    ids.retain(|id| {
        let id = id.trim();
        !id.is_empty() && seen.insert(id.to_string())
    });
    // Store the trimmed form, so a stray space does not make an id unresolvable.
    for id in ids.iter_mut() {
        *id = id.trim().to_string();
    }
}

fn remove_id(ids: &mut Vec<String>, id: &str) {
    ids.retain(|item| item != id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn off(id: &str) -> (String, PluginEntry) {
        (id.to_string(), PluginEntry { enabled: false })
    }

    #[test]
    fn normalize_trims_and_drops_blank_ids() {
        let mut settings = PluginSettings {
            plugins: BTreeMap::from([
                (" a@m ".to_string(), PluginEntry { enabled: true }),
                ("".to_string(), PluginEntry { enabled: true }),
                ("   ".to_string(), PluginEntry { enabled: true }),
            ]),
            projects: BTreeMap::from([(
                "/work/repo/".to_string(),
                vec!["c@m".into(), "c@m".into(), " ".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };

        settings.normalize();

        assert_eq!(settings.plugins.keys().collect::<Vec<_>>(), vec!["a@m"]);
        assert_eq!(
            settings.projects,
            BTreeMap::from([("/work/repo".to_string(), vec!["c@m".to_string()])]),
            "a blank project key is dropped and the separator is trimmed"
        );
    }

    #[test]
    fn normalize_keeps_a_project_whose_list_emptied_out() {
        // An empty list is a legitimate way to say "this project adds nothing",
        // and dropping the key would make the config churn on every save.
        let mut settings = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([("/work/repo".to_string(), vec!["  ".into()])]),
            disabled_projects: BTreeMap::new(),
        };

        settings.normalize();

        assert_eq!(settings.projects.get("/work/repo"), Some(&Vec::new()));
    }

    #[test]
    fn a_plugin_switched_off_globally_is_struck_from_every_project() {
        // Off has to mean off. A project that kept loading it would make the
        // plugins window lie about the switch it just showed the user.
        let mut settings = PluginSettings {
            plugins: BTreeMap::from([
                off("figma@personal"),
                (
                    "repo-triage@my-team".to_string(),
                    PluginEntry { enabled: true },
                ),
            ]),
            projects: BTreeMap::from([(
                "/work/repo".to_string(),
                vec!["figma@personal".into(), "repo-triage@my-team".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };

        settings.normalize();

        assert_eq!(
            settings.projects["/work/repo"],
            vec!["repo-triage@my-team".to_string()]
        );
    }

    #[test]
    fn a_project_key_is_the_same_whether_or_not_it_ends_in_a_separator() {
        assert_eq!(
            project_key(Path::new("/work/repo/")),
            project_key(Path::new("/work/repo"))
        );
        assert_eq!(
            project_key(Path::new(r"C:\work\repo\")),
            project_key(Path::new(r"C:\work\repo"))
        );
        assert_eq!(project_key(Path::new("  /work/repo  ")), "/work/repo");
    }

    #[test]
    fn project_switch_moves_an_id_between_enabled_and_disabled_lists() {
        let project = Path::new("/work/repo");
        let key = project_key(project);
        let mut settings = PluginSettings::default();

        settings.set_project_enabled(project, "repo-triage@team", false);
        assert_eq!(settings.disabled_projects[&key], vec!["repo-triage@team"]);
        assert!(
            settings.projects.get(&key).is_none_or(Vec::is_empty),
            "disabling a project plugin must not create a global switch"
        );

        settings.set_project_enabled(project, "repo-triage@team", true);
        assert_eq!(settings.projects[&key], vec!["repo-triage@team"]);
        assert!(
            settings
                .disabled_projects
                .get(&key)
                .is_none_or(Vec::is_empty),
            "enabling a project plugin removes its disabled marker"
        );
    }

    #[test]
    fn an_absent_section_is_an_empty_one() {
        let parsed: PluginSettings = toml::from_str("").unwrap();
        assert!(parsed.plugins.is_empty());
        assert!(parsed.projects.is_empty());
        assert!(parsed.disabled_projects.is_empty());
    }

    #[test]
    fn a_plugin_table_is_codexs_shape() {
        let text = r#"
            [ "figma@openai-curated" ]
            enabled = true

            [ "chrome@openai-bundled" ]
            enabled = false

            [projects]
            "C:\\work\\repo" = ["repo-triage@my-team"]
        "#;
        let parsed: PluginSettings = toml::from_str(text).unwrap();

        assert!(parsed.plugins["figma@openai-curated"].enabled);
        assert!(
            !parsed.plugins["chrome@openai-bundled"].enabled,
            "a switch that is off is still a row, which is what makes it reversible"
        );
        assert_eq!(
            parsed.projects.len(),
            1,
            "`projects` is a field, not a plugin"
        );
    }

    #[test]
    fn a_bare_plugin_table_is_off() {
        // An id only reaches the table because something wrote it there, and the
        // switch is what says what that was meant to do.
        let parsed: PluginSettings = toml::from_str(r#"["figma@openai-curated"]"#).unwrap();
        assert!(!parsed.plugins["figma@openai-curated"].enabled);
    }

    #[test]
    fn the_section_round_trips_through_toml() {
        let settings = PluginSettings {
            plugins: BTreeMap::from([
                (
                    "figma@openai-curated".to_string(),
                    PluginEntry { enabled: true },
                ),
                off("chrome@openai-bundled"),
            ]),
            projects: BTreeMap::from([(
                "/work/repo".to_string(),
                vec!["repo-triage@my-team".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };

        let text = toml::to_string(&settings).unwrap();
        let parsed: PluginSettings = toml::from_str(&text).unwrap();

        assert_eq!(parsed.plugins, settings.plugins);
        assert_eq!(parsed.projects, settings.projects);
        assert_eq!(parsed.disabled_projects, settings.disabled_projects);
    }
}
