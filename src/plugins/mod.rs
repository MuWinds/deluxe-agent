//! Codex plugins, as this agent consumes them.
//!
//! A *plugin* is a directory Codex installs: a manifest at
//! `.codex-plugin/plugin.json` plus optional companions (`skills/`, `commands/`,
//! `agents/`, `hooks.json`, `.mcp.json`). This module finds the ones the user
//! enabled and reads their manifests and skills; the other companions are read
//! by their own modules.
//!
//! It also reads the ones the user *disabled*, and keeps them apart — see
//! [`PluginCatalogue::disabled`]. A switch is only worth having if what it
//! turned off stays visible and can be turned back on, and that needs the
//! plugin resolved and read just like an enabled one. Nothing that runs a
//! plugin ever sees them: the two lists are separate, so a consumer cannot
//! reach a disabled plugin by forgetting a check.
//!
//! # Two scopes, and why they are not flattened
//!
//! Codex plugins come from marketplaces at two levels. The personal marketplace
//! (`~/.agents/plugins/marketplace.json`) and the ones shipped with Codex
//! (`~/.codex/bundled-marketplaces/`) are **global**: they apply everywhere. A
//! repository's own marketplace (`<repo>/.agents/plugins/marketplace.json`) is
//! **project-scoped**: it applies to that repository and nowhere else.
//!
//! Flattening the two into one list would be simpler and wrong. A plugin brings
//! skills, MCP servers and hooks, and a project-scoped hook firing in an
//! unrelated project is a real bug — so the split is preserved all the way to
//! [`PluginCatalogue::for_project`], and every consumer goes through it.
//!
//! # What is deliberately not done here
//!
//! * **No remote fetching.** Only `local` sources and the already-installed
//!   cache are resolved. `git-subdir` and `npm` sources are Codex's job to fetch;
//!   by the time this agent looks, the result is in the cache.
//! * **No enable decision from a repository.** See [`settings`].
//! * **Discovery never fails.** A malformed manifest, a missing directory, an id
//!   that names nothing — each is a warning and a skip, so one bad plugin cannot
//!   stop the others from loading. This mirrors `config::load`, which treats a
//!   malformed file as a warning rather than a startup failure.

pub mod agents;
pub mod commands;
pub mod frontmatter;
pub mod hooks;
pub mod manifest;
pub mod settings;
pub mod skills;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub use agents::AgentRole;
pub use commands::Command;
pub use hooks::Hook;
pub use manifest::PluginManifest;
pub use settings::PluginSettings;
pub use skills::Skill;

use settings::project_key;

/// A one-line summary is capped at this many characters.
///
/// `interface.shortDescription`, a skill's frontmatter and a command's
/// frontmatter are each meant to be a subtitle, but nothing enforces that, and
/// any of them is free to hold a paragraph. Shared by [`skills`] and
/// [`commands`] so the two summaries the UI shows cannot drift apart.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 200;

/// Caps text on a character boundary, so a multi-byte character is never split.
pub(crate) fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut capped: String = text.chars().take(max).collect();
    capped.push('…');
    capped
}

/// The summary to show when a file's frontmatter carries none.
///
/// The first line of prose: headings are skipped because an opening heading
/// names the thing rather than describing it, which is exactly what the name
/// already does. Shared by [`commands`] and [`agents`], whose real files both
/// omit the frontmatter entirely.
pub(crate) fn first_prose_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| cap_chars(line, MAX_DESCRIPTION_CHARS))
}

/// Where a plugin came from, which decides where it applies.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Applies in every project: the personal marketplace and the bundled ones.
    Global,
    /// Applies only in the project at this root: a repository's own marketplace.
    Project(PathBuf),
}

impl Scope {
    /// A short label for logs.
    pub fn label(&self) -> String {
        match self {
            Self::Global => "global".to_string(),
            Self::Project(project) => format!("project {}", project.display()),
        }
    }
}

/// One plugin that loaded successfully.
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    /// `name@marketplace` — the identity the config names it by.
    pub id: String,
    pub scope: Scope,
    /// The plugin's directory, absolute.
    pub root: PathBuf,
    pub manifest: PluginManifest,
    /// The skills this plugin contributes, sorted by name.
    pub skills: Vec<Skill>,
    /// The slash commands this plugin contributes, sorted by name.
    ///
    /// Unlike a skill, a command is invoked by the user rather than chosen by
    /// the model, so this reaches the composer and never the system prompt.
    pub commands: Vec<Command>,
    /// The `PostToolUse` hooks this plugin contributes.
    ///
    /// Carried resolved rather than as a path, because a hook is a plain command
    /// with no work left to do at run time — see [`hooks`]. They fire in the
    /// agent loop, so like the skills they must belong to the project that
    /// loaded them and no other.
    pub hooks: Vec<Hook>,
    /// The sub-agent roles this plugin contributes, sorted by name.
    ///
    /// Offered to the model through the `task` tool; see [`agents`].
    pub agents: Vec<AgentRole>,
    /// The MCP servers this plugin declares, keyed by the name they are exposed
    /// under. Empty for most plugins.
    ///
    /// Carried here rather than resolved at discovery time because starting a
    /// server is async work with a handshake and a timeout, and discovery is a
    /// pure function that runs before the window exists. The worker connects
    /// them for the project that needs them.
    pub mcp_servers: BTreeMap<String, manifest::McpServerConfig>,
}

impl LoadedPlugin {
    /// The plugin's own display name, falling back to its manifest name.
    pub fn display_name(&self) -> &str {
        self.manifest.display_name()
    }

    /// The manifest's one-line summary, when it carries one.
    pub fn summary(&self) -> Option<&str> {
        self.manifest.summary()
    }
}

/// Every plugin that loaded, split by scope.
#[derive(Debug, Default)]
pub struct PluginCatalogue {
    global: Vec<LoadedPlugin>,
    /// Keyed by [`settings::project_key`], the same key `PluginSettings` uses,
    /// so a project is one project whether it is spelled with a trailing
    /// separator or not.
    by_project: BTreeMap<String, Vec<LoadedPlugin>>,
    /// Global plugins the user turned off but that are still installed.
    ///
    /// Resolved and read like the others, because the plugins window must show
    /// what a disabled plugin *is* — its skills, its commands — and offer to
    /// turn it back on. They are kept apart from `global` rather than flagged
    /// on [`LoadedPlugin`], so nothing that consumes the catalogue for its
    /// actual work can reach a plugin the user disabled by forgetting a check:
    /// the disabled ones simply are not in the lists those consumers walk.
    disabled: Vec<LoadedPlugin>,
}

impl PluginCatalogue {
    /// The plugins that apply to `project`: the global ones, then that project's
    /// own.
    ///
    /// A project-scoped plugin shadows a global one with the same id, so a
    /// repository can pin its own build of a plugin the user also has globally.
    /// Sorted by id, which keeps the system prompt byte-stable across runs — the
    /// property that keeps a provider's prompt cache warm.
    pub fn for_project(&self, project: &Path) -> Vec<&LoadedPlugin> {
        let mut merged: BTreeMap<&str, &LoadedPlugin> = BTreeMap::new();

        for plugin in &self.global {
            merged.insert(plugin.id.as_str(), plugin);
        }
        if let Some(project_plugins) = self.by_project.get(&project_key(project)) {
            for plugin in project_plugins {
                merged.insert(plugin.id.as_str(), plugin);
            }
        }

        merged.into_values().collect()
    }

    /// Every loaded plugin, global first, for logging.
    pub fn all(&self) -> impl Iterator<Item = &LoadedPlugin> {
        self.global
            .iter()
            .chain(self.by_project.values().flatten())
    }

    /// The global plugins alone: the personal marketplace's, and the ones
    /// bundled with Codex.
    ///
    /// Distinct from [`all`](Self::all) because the two answer different
    /// questions. "What is installed for every project" is what the plugins
    /// window shows, and it must not quietly pick up whatever one repository
    /// happened to enable — a list that mixed the scopes would claim a
    /// project's plugin applies everywhere, which is the exact confusion the
    /// two scopes exist to prevent.
    ///
    /// Sorted by id, which is the order `[plugins]` is stored in.
    pub fn global(&self) -> &[LoadedPlugin] {
        &self.global
    }

    /// The global plugins the user disabled, sorted by id.
    ///
    /// The plugins window shows these below the enabled ones, greyed, so a
    /// plugin turned off stays visible and can be turned back on. They are
    /// resolved, not merely named, so the window can show what turning one on
    /// would bring.
    pub fn disabled(&self) -> &[LoadedPlugin] {
        &self.disabled
    }

    /// Files a plugin under its scope: global, or the project that owns it.
    fn insert(&mut self, plugin: LoadedPlugin) {
        match &plugin.scope {
            Scope::Global => self.global.push(plugin),
            Scope::Project(project) => self
                .by_project
                .entry(project_key(project))
                .or_default()
                .push(plugin),
        }
    }

    /// Records a global plugin the user turned off. Always [`Scope::Global`]:
    /// the disabled list is only ever fed from the global pass.
    fn disable(&mut self, plugin: LoadedPlugin) {
        debug_assert!(
            matches!(plugin.scope, Scope::Global),
            "only global plugins are ever disabled"
        );
        self.disabled.push(plugin);
    }
}

/// A marketplace file that was read, with the root its `source.path` resolves
/// against.
#[derive(Debug)]
struct Marketplace {
    name: String,
    root: PathBuf,
    scope: Scope,
    manifest: manifest::MarketplaceManifest,
}

/// Finds and loads the enabled plugins.
///
/// `home` is passed in rather than looked up so tests can point it at a temp
/// directory. It cannot come from `DELUXE_AGENT_CONFIG_DIR`, which redirects
/// this agent's own config but not `~/.agents` or `~/.codex` — using it here
/// would let a test read, and a stray write destroy, the real installation.
///
/// Never fails: see the module docs.
pub fn discover(home: &Path, projects: &[PathBuf], settings: &PluginSettings) -> PluginCatalogue {
    let marketplaces = read_marketplaces(home, projects);
    let offered = offered_ids(&marketplaces);

    let mut catalogue = PluginCatalogue::default();

    for (id, entry) in &settings.plugins {
        match resolve(id, &marketplaces, None, home) {
            Some(plugin) if entry.enabled => catalogue.insert(plugin),
            // Disabled, but installed: still read, so the window can show what
            // it is and offer to turn it back on. Not an error, so no warning.
            Some(plugin) => catalogue.disable(plugin),
            // Only an id the user *asked* to enable and that resolved to
            // nothing is worth a warning. A disabled id that names nothing is
            // the user having cleaned up, or a plugin uninstalled while off.
            None if entry.enabled => warn_unresolved(id, &offered),
            None => {}
        }
    }

    // A project entry can name a plugin from any marketplace, including a
    // global one — "enable figma in this repository only" is a legitimate
    // request, and it is why the project's own marketplaces are searched before
    // the global ones rather than instead of them.
    //
    // A plugin switched off globally is skipped even here. [`PluginSettings`]
    // already strikes such ids from every project list, but this is the load
    // path and it must not depend on that repair having run: off has to mean
    // off, and a project quietly overriding the switch is the one failure this
    // whole shape exists to prevent.
    let off: HashSet<&str> = settings
        .plugins
        .iter()
        .filter(|(_, entry)| !entry.enabled)
        .map(|(id, _)| id.as_str())
        .collect();
    let mut projects: Vec<&String> = settings.projects.keys().collect();
    projects.sort();
    for project in projects {
        let root = PathBuf::from(project);
        let ids = &settings.projects[project];
        for id in ids {
            if off.contains(id.as_str()) {
                continue;
            }
            match resolve(id, &marketplaces, Some(&root), home) {
                Some(plugin) => catalogue.insert(plugin),
                None => warn_unresolved(id, &offered),
            }
        }
    }

    for plugin in catalogue.all() {
        tracing::info!(
            plugin = %plugin.id,
            name = plugin.display_name(),
            version = plugin.manifest.version.as_deref().unwrap_or("unknown"),
            about = plugin.summary().unwrap_or(""),
            scope = %plugin.scope.label(),
            root = %plugin.root.display(),
            skills = plugin.skills.len(),
            commands = plugin.commands.len(),
            hooks = plugin.hooks.len(),
            agents = plugin.agents.len(),
            "loaded plugin"
        );
    }

    catalogue
}

/// Resolves one `name@marketplace` id to a loaded plugin.
///
/// `project` narrows the search: when it is set, that project's marketplaces are
/// tried before the global ones. When it is `None`, only global marketplaces are
/// considered — a global enable entry must not silently pick up a plugin from
/// some repository's marketplace.
fn resolve(
    id: &str,
    marketplaces: &[Marketplace],
    project: Option<&Path>,
    home: &Path,
) -> Option<LoadedPlugin> {
    let (plugin_name, marketplace_name) = match id.split_once('@') {
        Some((plugin, marketplace)) if !plugin.is_empty() && !marketplace.is_empty() => {
            (plugin, marketplace)
        }
        _ => {
            tracing::warn!(id, "plugin id is not `name@marketplace`; skipping");
            return None;
        }
    };

    // The project's own marketplaces first, then the global ones.
    let mut candidates: Vec<&Marketplace> = Vec::new();
    if let Some(project) = project {
        let key = project_key(project);
        candidates.extend(
            marketplaces
                .iter()
                .filter(|marketplace| marketplace.name == marketplace_name)
                .filter(|marketplace| match &marketplace.scope {
                    Scope::Project(root) => project_key(root) == key,
                    Scope::Global => false,
                }),
        );
    }
    candidates.extend(
        marketplaces
            .iter()
            .filter(|marketplace| marketplace.name == marketplace_name)
            .filter(|marketplace| marketplace.scope == Scope::Global),
    );

    for marketplace in candidates {
        let Some(entry) = marketplace
            .manifest
            .plugins
            .iter()
            .find(|entry| entry.name == plugin_name)
        else {
            continue;
        };

        if !entry.is_offered() {
            tracing::warn!(
                id,
                marketplace = %marketplace.name,
                "the marketplace lists this plugin as NOT_AVAILABLE; skipping"
            );
            return None;
        }

        // A local source is the working copy, which is what a developer editing
        // a plugin wants picked up.
        if entry.source.is_local() {
            if let Some(relative) = &entry.source.path {
                let root = marketplace.root.join(relative.trim_start_matches("./"));
                if let Some(plugin) = load_from(&root, id, scope_for(project)) {
                    return Some(plugin);
                }
                tracing::debug!(
                    id,
                    path = %root.display(),
                    "the marketplace's local source is not a plugin; trying the cache"
                );
            }
        }

        // Otherwise fall back to what Codex installed. This is the only arm that
        // can satisfy `openai-curated`, whose marketplace file is built into
        // Codex and exists nowhere on disk.
        if let Some(root) = cached_root(home, marketplace_name, plugin_name) {
            return load_from(&root, id, scope_for(project));
        }

        tracing::warn!(
            id,
            marketplace = %marketplace.name,
            "the marketplace offers this plugin but no local copy or cache entry was found"
        );
        return None;
    }

    // No marketplace claimed it — the cache may still have it, which is how a
    // plugin installed from a marketplace this agent cannot see still loads.
    if let Some(root) = cached_root(home, marketplace_name, plugin_name) {
        return load_from(&root, id, scope_for(project));
    }

    None
}

fn scope_for(project: Option<&Path>) -> Scope {
    match project {
        Some(project) => Scope::Project(project.to_path_buf()),
        None => Scope::Global,
    }
}

/// Reads a plugin from `root`, or `None` if it is not one.
fn load_from(root: &Path, id: &str, scope: Scope) -> Option<LoadedPlugin> {
    if !root
        .join(manifest::MANIFEST_DIR)
        .join(manifest::MANIFEST_FILE)
        .is_file()
    {
        return None;
    }

    let manifest = match manifest::read_plugin(root) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::warn!(id, %error, "skipping a plugin with an unreadable manifest");
            return None;
        }
    };

    let skills = skills::load(id, root, manifest.skills.as_deref());

    // Commands are namespaced by the plugin's *short* name — `/figma:…` — so
    // this takes `manifest.name` rather than the `name@marketplace` id the
    // skills above are attributed by. A user types the short one.
    let commands = commands::load(&manifest.name, root);

    // Hooks and roles are attributed by the full `name@marketplace` id, like the
    // skills: both are surfaced by the host rather than typed by the user, and
    // with two marketplaces installed the short name alone is ambiguous.
    let hooks = hooks::load(id, root);
    let agents = agents::load(id, root);

    // A `.mcp.json` that will not parse costs the plugin its servers and
    // nothing else — the skills above are already loaded, and the manifest
    // parsed. Same rule as everywhere else in discovery: warn, keep going.
    let mcp_servers = match manifest::read_mcp_servers(root) {
        Ok(servers) => servers,
        Err(error) => {
            tracing::warn!(id, %error, "skipping a plugin's unreadable .mcp.json");
            BTreeMap::new()
        }
    };

    Some(LoadedPlugin {
        id: id.to_string(),
        scope,
        root: root.to_path_buf(),
        manifest,
        skills,
        commands,
        hooks,
        agents,
        mcp_servers,
    })
}

/// The installed copy Codex keeps, if there is one.
///
/// A plugin can have several versions cached side by side. The last one in
/// sorted order wins: version strings like `26.616.51431` sort correctly, and
/// the hash-shaped ones a vendored plugin uses have only one entry, so the rule
/// is at worst arbitrary rather than wrong.
fn cached_root(home: &Path, marketplace: &str, plugin: &str) -> Option<PathBuf> {
    let dir = home
        .join(".codex")
        .join("plugins")
        .join("cache")
        .join(marketplace)
        .join(plugin);

    let mut versions: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    versions.sort();

    versions.into_iter().rev().find(|version| {
        version
            .join(manifest::MANIFEST_DIR)
            .join(manifest::MANIFEST_FILE)
            .is_file()
    })
}

/// Every marketplace file this agent knows how to find.
fn read_marketplaces(home: &Path, projects: &[PathBuf]) -> Vec<Marketplace> {
    let mut found = Vec::new();

    // The personal marketplace, and the ones shipped with Codex: both global.
    push_marketplace(
        &mut found,
        &home.join(".agents").join("plugins").join("marketplace.json"),
        Scope::Global,
    );

    let bundled = home.join(".codex").join("bundled-marketplaces");
    let mut bundles: Vec<PathBuf> = std::fs::read_dir(&bundled)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    // `read_dir` order is unspecified; sorting keeps discovery deterministic so
    // the system prompt does not churn between runs.
    bundles.sort();
    for bundle in bundles {
        push_marketplace(
            &mut found,
            &bundle.join(".agents").join("plugins").join("marketplace.json"),
            Scope::Global,
        );
    }

    // Each project's own marketplace, scoped to that project.
    let mut seen = std::collections::HashSet::new();
    for project in projects {
        if !seen.insert(project_key(project)) {
            continue;
        }
        push_marketplace(
            &mut found,
            &project.join(".agents").join("plugins").join("marketplace.json"),
            Scope::Project(project.clone()),
        );
    }

    found
}

/// Reads one marketplace file, warning and skipping if it is absent or broken.
///
/// An absent file is the normal case — most projects have no marketplace — so it
/// is not logged at all. A file that exists but does not parse is, because
/// somebody meant it to work.
fn push_marketplace(found: &mut Vec<Marketplace>, path: &Path, scope: Scope) {
    if !path.is_file() {
        return;
    }

    let manifest = match manifest::read_marketplace(path) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipping an unreadable marketplace");
            return;
        }
    };

    let Some(root) = manifest::marketplace_root(path) else {
        tracing::warn!(
            path = %path.display(),
            "a marketplace must live at <root>/.agents/plugins/marketplace.json; skipping"
        );
        return;
    };

    found.push(Marketplace {
        name: manifest.name.clone(),
        root,
        scope,
        manifest,
    });
}

/// Every `name@marketplace` the marketplaces offer, for a "did you mean" hint.
fn offered_ids(marketplaces: &[Marketplace]) -> Vec<String> {
    let mut ids: Vec<String> = marketplaces
        .iter()
        .flat_map(|marketplace| {
            marketplace
                .manifest
                .plugins
                .iter()
                .filter(|&entry| entry.is_offered())
                .map(|entry| format!("{}@{}", entry.name, marketplace.name))
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Says an enabled id resolved to nothing, and lists what was on offer.
///
/// The hint is the whole affordance for finding a plugin id: there is no plugin
/// browser in this agent, so the log is where a user learns the spelling.
fn warn_unresolved(id: &str, offered: &[String]) {
    if offered.is_empty() {
        tracing::warn!(id, "enabled plugin not found, and no marketplace was readable");
    } else {
        tracing::warn!(
            id,
            offered = %offered.join(", "),
            "enabled plugin not found; the ids above are what the marketplaces offer"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Scaffolds a plugin directory: a manifest plus one skill.
    fn write_plugin(root: &Path, name: &str, skill: Option<&str>) {
        let manifest_dir = root.join(manifest::MANIFEST_DIR);
        fs::create_dir_all(&manifest_dir).unwrap();
        fs::write(
            manifest_dir.join(manifest::MANIFEST_FILE),
            format!(
                r#"{{"name":"{name}","version":"1.0.0","description":"The {name} plugin.",
                     "interface":{{"displayName":"{name}","shortDescription":"Does {name} things"}}}}"#
            ),
        )
        .unwrap();

        if let Some(skill) = skill {
            let skill_dir = root.join("skills").join(skill);
            fs::create_dir_all(&skill_dir).unwrap();
            fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {skill}\ndescription: The {skill} skill\n---\n\nBody.\n"),
            )
            .unwrap();
        }
    }

    /// Writes a plugin's `.mcp.json`.
    fn write_mcp(root: &Path, body: &str) {
        fs::write(root.join(manifest::MCP_FILE), body).unwrap();
    }

    /// Writes a marketplace file at the standard location under `root`.
    fn write_marketplace(root: &Path, name: &str, entries: &[(&str, &str)]) {
        let dir = root.join(".agents").join("plugins");
        fs::create_dir_all(&dir).unwrap();
        let plugins: Vec<String> = entries
            .iter()
            .map(|(plugin, path)| {
                format!(
                    r#"{{"name":"{plugin}","source":{{"source":"local","path":"{path}"}},
                       "policy":{{"installation":"AVAILABLE","authentication":"ON_INSTALL"}}}}"#
                )
            })
            .collect();
        fs::write(
            dir.join("marketplace.json"),
            format!(
                r#"{{"name":"{name}","interface":{{"displayName":"{name}"}},"plugins":[{}]}}"#,
                plugins.join(",")
            ),
        )
        .unwrap();
    }

    /// A home directory with the personal marketplace and one bundled one, each
    /// offering one plugin, plus a project with its own marketplace.
    struct Fixture {
        home: tempfile::TempDir,
        project: tempfile::TempDir,
        other: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();

        // Personal (global): `figma` at `home/plugins/figma`.
        write_marketplace(home.path(), "personal", &[("figma", "./plugins/figma")]);
        write_plugin(&home.path().join("plugins/figma"), "figma", Some("figma-use"));

        // Bundled (global): `computer-use`.
        let bundle = home.path().join(".codex/bundled-marketplaces/openai-bundled");
        write_marketplace(&bundle, "openai-bundled", &[("computer-use", "./plugins/computer-use")]);
        write_plugin(&bundle.join("plugins/computer-use"), "computer-use", Some("computer-use"));

        // The project's own marketplace, which resolves against the project root.
        write_marketplace(
            project.path(),
            "my-team",
            &[("repo-triage", "./plugins/repo-triage")],
        );
        write_plugin(&project.path().join("plugins/repo-triage"), "repo-triage", Some("repo-triage"));

        Fixture { home, project, other }
    }

    fn ids<'a>(plugins: &[&'a LoadedPlugin]) -> Vec<&'a str> {
        plugins.iter().map(|plugin| plugin.id.as_str()).collect()
    }

    #[test]
    fn a_global_plugin_applies_in_every_project() {
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&["figma@personal"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &[], &settings);

        assert_eq!(ids(&catalogue.for_project(fixture.project.path())), vec!["figma@personal"]);
        assert_eq!(ids(&catalogue.for_project(fixture.other.path())), vec!["figma@personal"]);
    }

    #[test]
    fn a_project_plugin_applies_only_in_its_project() {
        // The load-bearing test for the two scopes. A repository's plugin
        // leaking into an unrelated project would fire its hooks there.
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["repo-triage@my-team".into()],
            )]),
        };
        let catalogue = discover(
            fixture.home.path(),
            &[fixture.project.path().to_path_buf(), fixture.other.path().to_path_buf()],
            &settings,
        );

        assert_eq!(ids(&catalogue.for_project(fixture.project.path())), vec!["repo-triage@my-team"]);
        assert!(
            catalogue.for_project(fixture.other.path()).is_empty(),
            "a project-scoped plugin must not leak into another project"
        );
    }

    #[test]
    fn a_project_sees_its_own_plugins_alongside_the_global_ones() {
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&["figma@personal", "computer-use@openai-bundled"]),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["repo-triage@my-team".into()],
            )]),
        };
        let catalogue = discover(fixture.home.path(), &[fixture.project.path().to_path_buf()], &settings);

        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())),
            vec!["computer-use@openai-bundled", "figma@personal", "repo-triage@my-team"],
            "global plus project, sorted by id"
        );
        assert_eq!(
            ids(&catalogue.for_project(fixture.other.path())),
            vec!["computer-use@openai-bundled", "figma@personal"],
            "the other project gets the global ones only"
        );
    }

    #[test]
    fn the_global_list_holds_the_global_plugins_and_none_of_a_projects() {
        // What the plugins window shows. A project's plugin appearing here would
        // claim it applies everywhere, which is the confusion the two scopes
        // exist to prevent.
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&["figma@personal"]),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["repo-triage@my-team".into()],
            )]),
        };
        let catalogue = discover(
            fixture.home.path(),
            &[fixture.project.path().to_path_buf()],
            &settings,
        );

        let global: Vec<&str> = catalogue
            .global()
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect();
        assert_eq!(global, vec!["figma@personal"]);
        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())).len(),
            2,
            "the project itself still sees both"
        );
    }

    #[test]
    fn a_project_may_enable_a_global_plugin_for_itself_alone() {
        // `[plugins.projects]` is not restricted to the project's own
        // marketplace: pinning a global plugin to one repository is the point.
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["figma@personal".into()],
            )]),
        };
        let catalogue = discover(fixture.home.path(), &[fixture.project.path().to_path_buf()], &settings);

        assert_eq!(ids(&catalogue.for_project(fixture.project.path())), vec!["figma@personal"]);
        assert!(catalogue.for_project(fixture.other.path()).is_empty());
    }

    #[test]
    fn a_global_entry_cannot_reach_into_a_repository_marketplace() {
        // A global enable entry naming a repo-scoped marketplace must not
        // resolve, or a cloned repository could inject a plugin into every
        // project.
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&["repo-triage@my-team"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &[fixture.project.path().to_path_buf()], &settings);

        let resolved = catalogue.for_project(fixture.project.path());
        assert!(resolved.is_empty(), "got {:?}", ids(&resolved));
    }

    #[test]
    fn a_project_scoped_plugin_shadows_a_global_one_with_the_same_id() {
        let fixture = fixture();
        // The same id in both scopes, but each marketplace points somewhere else.
        let project_plugin = fixture.project.path().join("plugins/pinned");
        write_plugin(&project_plugin, "pinned", None);
        write_marketplace(
            fixture.project.path(),
            "personal",
            &[("pinned", "./plugins/pinned")],
        );

        let settings = PluginSettings {
            plugins: settings::entries(&["pinned@personal"]),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["pinned@personal".into()],
            )]),
        };
        let catalogue = discover(fixture.home.path(), &[fixture.project.path().to_path_buf()], &settings);

        let resolved = catalogue.for_project(fixture.project.path());
        assert_eq!(resolved.len(), 1, "one id is one plugin, not two");
        assert_eq!(
            resolved[0].root,
            project_plugin,
            "the project's own copy wins, so a repository can pin its build"
        );
        // The project's marketplace is named `personal` too, and it is scoped to
        // the project — so elsewhere the id resolves to nothing rather than to
        // the project's copy.
        assert!(catalogue.for_project(fixture.other.path()).is_empty());
    }

    #[test]
    fn a_plugin_is_read_from_the_cache_when_no_marketplace_offers_it() {
        // `openai-curated` is built into Codex: its marketplace file exists
        // nowhere on disk, so the cache is the only source.
        let home = tempfile::tempdir().unwrap();
        write_plugin(
            &home.path().join(".codex/plugins/cache/openai-curated/figma/1dc19589"),
            "figma",
            Some("figma-use"),
        );

        let settings = PluginSettings {
            plugins: settings::entries(&["figma@openai-curated"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(home.path(), &[], &settings);

        assert_eq!(ids(&catalogue.for_project(Path::new("/any"))), vec!["figma@openai-curated"]);
    }

    #[test]
    fn the_newest_cached_version_wins() {
        let home = tempfile::tempdir().unwrap();
        let cache = home.path().join(".codex/plugins/cache/openai-bundled/computer-use");
        write_plugin(&cache.join("26.616.51431"), "computer-use", None);
        write_plugin(&cache.join("27.1.1"), "computer-use", None);

        let settings = PluginSettings {
            plugins: settings::entries(&["computer-use@openai-bundled"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(home.path(), &[], &settings);

        assert_eq!(
            catalogue.for_project(Path::new("/any"))[0].root,
            cache.join("27.1.1")
        );
    }

    #[test]
    fn the_marketplaces_local_copy_wins_over_the_cache() {
        let fixture = fixture();
        // A cached copy of the same plugin, which the working copy must beat.
        write_plugin(
            &fixture.home.path().join(".codex/plugins/cache/personal/figma/9.9.9"),
            "figma",
            None,
        );

        let settings = PluginSettings {
            plugins: settings::entries(&["figma@personal"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &[], &settings);

        assert_eq!(
            catalogue.for_project(Path::new("/any"))[0].root,
            fixture.home.path().join("plugins/figma"),
            "a developer editing a plugin expects their working copy to load"
        );
    }

    #[test]
    fn an_entry_the_marketplace_withholds_does_not_load_even_if_cached() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".agents/plugins");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("marketplace.json"),
            r#"{"name":"personal","plugins":[{"name":"withdrawn",
               "source":{"source":"local","path":"./plugins/withdrawn"},
               "policy":{"installation":"NOT_AVAILABLE"}}]}"#,
        )
        .unwrap();
        write_plugin(
            &home.path().join(".codex/plugins/cache/personal/withdrawn/1.0.0"),
            "withdrawn",
            None,
        );

        let settings = PluginSettings {
            plugins: settings::entries(&["withdrawn@personal"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(home.path(), &[], &settings);

        assert!(catalogue.for_project(Path::new("/any")).is_empty());
    }

    #[test]
    fn skills_are_read_alongside_the_manifest() {
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&["computer-use@openai-bundled"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &[], &settings);

        let plugin = catalogue.for_project(Path::new("/any"))[0];
        assert_eq!(plugin.display_name(), "computer-use");
        assert_eq!(plugin.summary(), Some("Does computer-use things"));
        assert_eq!(plugin.skills.len(), 1);
        assert_eq!(plugin.skills[0].name, "computer-use");
        assert_eq!(plugin.skills[0].plugin, "computer-use@openai-bundled");
    }

    #[test]
    fn an_id_that_names_nothing_is_skipped_without_taking_the_others_down() {
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&[
                "nope@personal",
                "figma@personal",
                "also-nope@nowhere",
                "malformed-id",
            ]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &[], &settings);

        assert_eq!(
            ids(&catalogue.for_project(Path::new("/any"))),
            vec!["figma@personal"],
            "one bad id must not stop the good ones"
        );
    }

    #[test]
    fn a_broken_manifest_is_skipped_rather_than_fatal() {
        let fixture = fixture();
        let broken = fixture.home.path().join("plugins/broken");
        fs::create_dir_all(broken.join(manifest::MANIFEST_DIR)).unwrap();
        fs::write(broken.join(manifest::MANIFEST_DIR).join(manifest::MANIFEST_FILE), "{ not json").unwrap();
        write_marketplace(
            fixture.home.path(),
            "personal",
            &[("figma", "./plugins/figma"), ("broken", "./plugins/broken")],
        );

        let settings = PluginSettings {
            plugins: settings::entries(&["broken@personal", "figma@personal"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &[], &settings);

        assert_eq!(ids(&catalogue.for_project(Path::new("/any"))), vec!["figma@personal"]);
    }

    #[test]
    fn a_marketplace_that_is_not_at_the_standard_path_is_skipped() {
        // The root cannot be inferred, and guessing would resolve `source.path`
        // against the wrong directory.
        let home = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join("marketplace.json"),
            r#"{"name":"stray","plugins":[{"name":"x","source":{"source":"local","path":"./plugins/x"}}]}"#,
        )
        .unwrap();

        let settings = PluginSettings {
            plugins: settings::entries(&["x@stray"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(home.path(), &[], &settings);

        assert!(catalogue.for_project(Path::new("/any")).is_empty());
    }

    #[test]
    fn a_plugins_mcp_servers_load_alongside_its_skills() {
        let fixture = fixture();
        write_mcp(
            &fixture.project.path().join("plugins/repo-triage"),
            r#"{"mcpServers":{"triage":{"command":"python","args":["./s.py"],"cwd":"."}}}"#,
        );

        let settings = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["repo-triage@my-team".into()],
            )]),
        };
        let catalogue = discover(
            fixture.home.path(),
            &[fixture.project.path().to_path_buf()],
            &settings,
        );

        let plugins = catalogue.for_project(fixture.project.path());
        let triage = plugins
            .iter()
            .find(|plugin| plugin.id == "repo-triage@my-team")
            .expect("the project's own plugin loads");

        assert_eq!(triage.mcp_servers.len(), 1);
        assert_eq!(
            triage.mcp_servers["triage"].command.as_deref(),
            Some("python"),
            "the server is carried on the plugin, not resolved at discovery time"
        );
    }

    #[test]
    fn a_plugin_with_no_mcp_file_has_no_servers() {
        // The common case, and it must not be an error: most plugins bring no
        // server, and one that does not must still load its skills.
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&["figma@personal"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &[], &settings);

        let plugins = catalogue.for_project(Path::new("/any"));
        assert_eq!(ids(&plugins), vec!["figma@personal"]);
        assert!(plugins[0].mcp_servers.is_empty());
        assert_eq!(plugins[0].skills.len(), 1, "the skills still load");
    }

    #[test]
    fn no_configuration_means_no_plugins_and_no_failure() {
        let home = tempfile::tempdir().unwrap();
        let catalogue = discover(home.path(), &[], &PluginSettings::default());

        assert!(catalogue.for_project(Path::new("/any")).is_empty());
    }

    #[test]
    fn a_disabled_plugin_is_read_but_not_loaded() {
        // The plugins window must be able to show a plugin that is switched off
        // — that is what makes the switch reversible rather than a delete — so
        // discovery still resolves and reads it. What it must not do is hand it
        // to anything that would run it.
        let fixture = fixture();
        let mut config = PluginSettings::default();
        config
            .plugins
            .insert("figma@personal".into(), settings::PluginEntry { enabled: false });

        let catalogue = discover(fixture.home.path(), &[], &config);

        assert!(
            catalogue.for_project(Path::new("/any")).is_empty(),
            "off means off: nothing that runs plugins may see it"
        );
        assert!(catalogue.global().is_empty());
        assert_eq!(
            catalogue
                .disabled()
                .iter()
                .map(|plugin| plugin.id.as_str())
                .collect::<Vec<_>>(),
            vec!["figma@personal"]
        );
        assert_eq!(
            catalogue.disabled()[0].skills.len(),
            1,
            "a disabled plugin is still read, so the window can describe what turning it on brings"
        );
    }

    #[test]
    fn a_disabled_id_that_names_nothing_is_not_an_error() {
        // Turning a plugin off and later uninstalling it is a normal sequence.
        // The leftover row is silent: the user asked for nothing to happen.
        let fixture = fixture();
        let mut config = PluginSettings::default();
        config
            .plugins
            .insert("gone@personal".into(), settings::PluginEntry { enabled: false });

        let catalogue = discover(fixture.home.path(), &[], &config);

        assert!(catalogue.global().is_empty());
        assert!(catalogue.disabled().is_empty());
    }

    #[test]
    fn a_project_cannot_override_a_global_switch() {
        // Belt and braces over `PluginSettings::normalize`, which already
        // strikes such ids from every project list: discovery itself must
        // refuse, so a list that still names a globally disabled plugin cannot
        // make an agent run it.
        let fixture = fixture();
        let mut config = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["figma@personal".into()],
            )]),
        };
        config
            .plugins
            .insert("figma@personal".into(), settings::PluginEntry { enabled: false });

        let catalogue = discover(
            fixture.home.path(),
            &[fixture.project.path().to_path_buf()],
            &config,
        );

        assert!(catalogue.for_project(fixture.project.path()).is_empty());
    }

    #[test]
    fn a_project_listed_twice_is_scanned_once() {
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["repo-triage@my-team".into()],
            )]),
        };
        let catalogue = discover(
            fixture.home.path(),
            &[
                fixture.project.path().to_path_buf(),
                fixture.project.path().to_path_buf(),
            ],
            &settings,
        );

        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())),
            vec!["repo-triage@my-team"],
            "a duplicate project must not load its plugins twice"
        );
    }

    #[test]
    fn a_trailing_separator_in_the_config_still_matches_the_project() {
        let fixture = fixture();
        let with_separator = format!("{}/", project_key(fixture.project.path()));
        let settings = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([(with_separator, vec!["repo-triage@my-team".into()])]),
        };
        let catalogue = discover(fixture.home.path(), &[fixture.project.path().to_path_buf()], &settings);

        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())),
            vec!["repo-triage@my-team"]
        );
    }

    #[test]
    fn the_real_figma_plugin_loads_its_hooks_and_agents_from_this_machines_install() {
        // Not a fixture: the plugin Codex actually cached, so the shapes the
        // unit tests encode are the shapes that ship, and the wiring from a
        // plugin root to `LoadedPlugin` is exercised end to end. Skipped where
        // figma is absent, so the suite still passes on a machine that has
        // never run Codex.
        let Some(home) = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
        else {
            return;
        };
        if !home.join(".codex/plugins/cache/openai-curated/figma").is_dir() {
            return;
        }

        let settings = PluginSettings {
            plugins: settings::entries(&["figma@openai-curated"]),
            projects: BTreeMap::new(),
        };
        let catalogue = discover(&home, &[], &settings);
        let plugins = catalogue.for_project(Path::new("/any"));
        let Some(figma) = plugins.iter().find(|plugin| plugin.id == "figma@openai-curated") else {
            return;
        };

        // The four roles figma ships, named by their files and sorted.
        assert_eq!(
            figma.agents.iter().map(|role| role.name.as_str()).collect::<Vec<_>>(),
            vec![
                "design-parity-review-agent",
                "design-system-rules-agent",
                "figma-code-connect-agent",
                "figma-implementation-agent",
            ]
        );
        assert!(
            figma.agents.iter().all(|role| !role.instructions.is_empty()),
            "each role's body becomes a sub-agent's system prompt"
        );

        // One hook, matching the Codex names for an edit — which this agent
        // spells `apply_patch`.
        assert_eq!(figma.hooks.len(), 1);
        assert_eq!(figma.hooks[0].pattern, "Write|Edit");
        assert!(figma.hooks[0].matches("apply_patch"));
    }
}
