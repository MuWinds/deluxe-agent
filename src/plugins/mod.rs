//! Wasmtime plugin discovery, scope resolution, and lifecycle metadata.
//!
//! Every loaded plugin must declare a Wasmtime component. Tools, plugin events,
//! and UI surfaces are implemented by Components using the same generic ABI; no
//! non-Wasm plugin path is retained. Discovery scans the managed plugin cache
//! directly — there is no separate catalogue file to keep in sync.
pub mod capabilities;
pub mod defaults;
pub mod manifest;
pub mod providers;
pub mod runtime;
pub mod settings;
pub mod ui_protocol;
pub mod wasm;
pub mod wasm_manifest;
pub mod wasm_runtime;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

pub use manifest::PluginManifest;
use settings::project_key;
pub use settings::PluginSettings;

/// Caps text on a character boundary, so a multi-byte character is never split.
pub(crate) fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut capped: String = text.chars().take(max).collect();
    capped.push('…');
    capped
}

/// Where a plugin came from, which decides where it applies.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Applies in every project: a plugin enabled in the global table.
    Global,
    /// Applies only in the project at this root.
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

/// The directory under the user's home that holds this agent's plugin
/// configuration and plugin cache.
pub const HOME_DIR: &str = ".deluxe-agents";

/// `home`'s [`HOME_DIR`], the root the plugin layout is anchored at.
pub fn home_root(home: &Path) -> PathBuf {
    home.join(HOME_DIR)
}

/// The directory a [`Scope::Global`] Component's generic file capability is
/// bound to, resolved from the user's home directory.
///
/// Deliberately a plugin configuration directory rather than the home itself:
/// a `read-plugin-file` capability rooted at `~` would hand any global plugin
/// `~/.ssh/id_rsa`, `~/.aws/credentials`, or a stray key file — none of which
/// is plugin configuration. The plugin cache lives under it too, so a global
/// `.mcp.json` or `.hooks.json` sits beside it.
pub fn global_configuration_root(home: &Path) -> PathBuf {
    home_root(home)
}

/// The managed plugin cache: `<home>/.deluxe-agents/plugins/cache`.
///
/// This is the one place an installed plugin lives. Discovery scans it directly;
/// there is no separate catalogue file to keep in sync.
pub fn plugin_cache_root(home: &Path) -> PathBuf {
    home_root(home).join("plugins").join("cache")
}

/// One plugin that loaded successfully.
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    /// `name@namespace` — the identity the config names it by.
    pub id: String,
    pub scope: Scope,
    /// The plugin's directory, absolute.
    pub root: PathBuf,
    pub manifest: PluginManifest,
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
    /// Project-scoped plugins that are installed but disabled.
    disabled_by_project: BTreeMap<String, Vec<LoadedPlugin>>,
    /// Global plugins the user turned off but that are still installed.
    ///
    /// Resolved and read like the others, because the plugins window must show
    /// what a disabled plugin *is* — its roles and its runtime — and offer to
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
        self.global.iter().chain(self.by_project.values().flatten())
    }

    /// Enabled plugins explicitly assigned to `project`, excluding globals.
    pub fn project(&self, project: &Path) -> &[LoadedPlugin] {
        self.by_project
            .get(&project_key(project))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Disabled plugins explicitly assigned to `project`, excluding globals.
    pub fn disabled_for_project(&self, project: &Path) -> &[LoadedPlugin] {
        self.disabled_by_project
            .get(&project_key(project))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Finds an installed plugin by id and exact scope.
    pub fn find_in_scope(&self, id: &str, scope: &Scope) -> Option<&LoadedPlugin> {
        match scope {
            Scope::Global => self
                .global
                .iter()
                .chain(self.disabled.iter())
                .find(|plugin| plugin.id == id),
            Scope::Project(project) => self
                .project(project)
                .iter()
                .chain(self.disabled_for_project(project))
                .find(|plugin| plugin.id == id),
        }
    }

    /// The global plugins alone: those enabled for every project.
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

    fn disable_project(&mut self, plugin: LoadedPlugin) {
        let Scope::Project(project) = &plugin.scope else {
            debug_assert!(false, "only project plugins are stored as project-disabled");
            return;
        };
        self.disabled_by_project
            .entry(project_key(project))
            .or_default()
            .push(plugin);
    }
}

/// One installed plugin found in the managed cache, before a scope is applied.
///
/// Scope is a property of the load decision, not of where the bytes live: the
/// same cached copy is global when the global table switches it on and
/// project-scoped when a project list does.
struct Cached {
    root: PathBuf,
    manifest: PluginManifest,
}

impl Cached {
    /// Attaches the id and scope the load decision gave this plugin.
    fn loaded(&self, id: &str, scope: Scope) -> LoadedPlugin {
        LoadedPlugin {
            id: id.to_string(),
            scope,
            root: self.root.clone(),
            manifest: self.manifest.clone(),
        }
    }
}

/// Finds and loads the enabled plugins.
///
/// `home` is passed in rather than looked up so tests can point it at a temp
/// directory. It cannot come from `DELUXE_AGENT_CONFIG_DIR`, which redirects
/// this agent's own config but not the plugin home directory — using it here
/// would let a test read, and a stray write destroy, the real installation.
///
/// Never fails: see the module docs.
pub fn discover(home: &Path, settings: &PluginSettings) -> PluginCatalogue {
    let cache = read_cache(home);
    let installed: Vec<String> = cache.keys().cloned().collect();

    let mut catalogue = PluginCatalogue::default();

    for (id, enabled) in &settings.plugins {
        match cache.get(id) {
            Some(cached) if *enabled => catalogue.insert(cached.loaded(id, Scope::Global)),
            // Disabled, but installed: still read, so the window can show what
            // it is and offer to turn it back on. Not an error, so no warning.
            Some(cached) => catalogue.disable(cached.loaded(id, Scope::Global)),
            // Only an id the user *asked* to enable and that resolved to
            // nothing is worth a warning. A disabled id that names nothing is
            // the user having cleaned up, or a plugin uninstalled while off.
            None if *enabled => warn_unresolved(id, &installed),
            None => {}
        }
    }

    // A plugin switched off globally is skipped even here. [`PluginSettings`]
    // already strikes such ids from every project list, but this is the load
    // path and it must not depend on that repair having run: off has to mean
    // off, and a project quietly overriding the switch is the one failure this
    // whole shape exists to prevent.
    let off: HashSet<&str> = settings
        .plugins
        .iter()
        .filter(|(_, enabled)| !**enabled)
        .map(|(id, _)| id.as_str())
        .collect();
    let mut projects: Vec<&String> = settings
        .projects
        .keys()
        .chain(settings.disabled_projects.keys())
        .collect();
    projects.sort();
    projects.dedup();
    for project in projects {
        let root = PathBuf::from(project);
        let enabled_ids = settings
            .projects
            .get(project)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for id in enabled_ids {
            if off.contains(id.as_str()) {
                continue;
            }
            match cache.get(id) {
                Some(cached) => {
                    catalogue.insert(cached.loaded(id, Scope::Project(root.clone())));
                }
                None => warn_unresolved(id, &installed),
            }
        }
        let disabled_ids = settings
            .disabled_projects
            .get(project)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for id in disabled_ids {
            if off.contains(id.as_str()) || enabled_ids.contains(id) {
                continue;
            }
            if let Some(cached) = cache.get(id) {
                catalogue.disable_project(cached.loaded(id, Scope::Project(root.clone())));
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
            "loaded plugin"
        );
    }

    catalogue
}

/// Every plugin installed in the managed cache, keyed by `name@namespace`.
///
/// The namespace is the cache directory the plugin sits under — `deluxe-defaults`
/// for a bundled component, `deluxe-local` for one imported from disk — and it
/// is what keeps two builds of the same plugin apart. The newest cached version
/// of each plugin wins; see [`latest_version`].
fn read_cache(home: &Path) -> BTreeMap<String, Cached> {
    let mut found = BTreeMap::new();
    let root = plugin_cache_root(home);
    let Ok(namespaces) = std::fs::read_dir(&root) else {
        return found;
    };
    let mut namespaces: Vec<PathBuf> = namespaces
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    // `read_dir` order is unspecified; sorting keeps discovery deterministic so
    // the system prompt does not churn between runs.
    namespaces.sort();

    for namespace_dir in namespaces {
        let Some(namespace) = namespace_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(names) = std::fs::read_dir(&namespace_dir) else {
            continue;
        };
        let mut names: Vec<PathBuf> = names
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        names.sort();
        for name_dir in names {
            let Some(name) = name_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(root) = latest_version(&name_dir) else {
                continue;
            };
            let Some(manifest) = read_cached(&root) else {
                continue;
            };
            found.insert(format!("{name}@{namespace}"), Cached { root, manifest });
        }
    }

    found
}

/// The newest cached version of one plugin that carries a manifest.
///
/// A plugin can have several versions cached side by side. The last one in
/// sorted order wins: numeric version strings like `2.1.0` sort correctly, and
/// the hash-shaped ones a vendored plugin uses have only one entry, so the rule
/// is at worst arbitrary rather than wrong.
fn latest_version(plugin_dir: &Path) -> Option<PathBuf> {
    let mut versions: Vec<PathBuf> = std::fs::read_dir(plugin_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    versions.sort();
    versions
        .into_iter()
        .rev()
        .find(|version| version.join(manifest::MANIFEST_FILE).is_file())
}

/// Reads one cached plugin, or `None` if it is not a usable Wasmtime component.
///
/// A broken manifest, or one with no Wasmtime runtime, is logged and skipped:
/// one invalid plugin must not make the rest of the catalogue unusable.
fn read_cached(root: &Path) -> Option<PluginManifest> {
    let manifest = match manifest::read_plugin(root) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::warn!(root = %root.display(), %error, "skipping a plugin with an unreadable manifest");
            return None;
        }
    };
    if manifest.wasm_runtime().is_none() {
        tracing::warn!(
            root = %root.display(),
            "skipping a plugin without a Wasmtime runtime"
        );
        return None;
    }
    Some(manifest)
}

/// Says an enabled id resolved to nothing, and lists what is installed.
///
/// The hint is the whole affordance for finding a plugin id: there is no plugin
/// browser in this agent, so the log is where a user learns the spelling.
fn warn_unresolved(id: &str, installed: &[String]) {
    if installed.is_empty() {
        tracing::warn!(id, "enabled plugin not found, and no plugin is installed");
    } else {
        tracing::warn!(
            id,
            installed = %installed.join(", "),
            "enabled plugin not found; the ids above are what is installed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Scaffolds a plugin directory: a manifest only.
    fn write_plugin(root: &Path, name: &str) {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join(manifest::MANIFEST_FILE),
            format!(
                r#"{{"name":"{name}","version":"1.0.0","description":"The {name} plugin.",
                     "runtime":{{"module":"plugin.wasm",
                     "apiVersion":"deluxe.harness/plugin@0.1"}},
                     "interface":{{"displayName":"{name}","shortDescription":"Does {name} things"}}}}"#
            ),
        )
        .unwrap();
    }

    /// Installs `name` into the managed cache under `namespace`.
    fn cache_plugin(home: &Path, namespace: &str, name: &str, version: &str) -> PathBuf {
        let root = plugin_cache_root(home)
            .join(namespace)
            .join(name)
            .join(version);
        write_plugin(&root, name);
        root
    }

    /// A home directory with three installed plugins, plus two projects to tell
    /// global scope from project scope.
    struct Fixture {
        home: tempfile::TempDir,
        project: tempfile::TempDir,
        other: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();

        cache_plugin(home.path(), "personal", "notes", "1.0.0");
        cache_plugin(home.path(), "bundled", "linter", "1.0.0");
        cache_plugin(home.path(), "my-team", "deploy", "1.0.0");

        Fixture {
            home,
            project,
            other,
        }
    }

    fn ids<'a>(plugins: &[&'a LoadedPlugin]) -> Vec<&'a str> {
        plugins.iter().map(|plugin| plugin.id.as_str()).collect()
    }

    #[test]
    fn a_global_plugin_applies_in_every_project() {
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&["notes@personal"]),
            projects: BTreeMap::new(),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())),
            vec!["notes@personal"]
        );
        assert_eq!(
            ids(&catalogue.for_project(fixture.other.path())),
            vec!["notes@personal"]
        );
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
                vec!["deploy@my-team".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())),
            vec!["deploy@my-team"]
        );
        assert!(
            catalogue.for_project(fixture.other.path()).is_empty(),
            "a project-scoped plugin must not leak into another project"
        );
    }

    #[test]
    fn a_project_plugin_can_be_disabled_without_disabling_global_plugins() {
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&["notes@personal"]),
            projects: BTreeMap::new(),
            disabled_projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["deploy@my-team".into()],
            )]),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())),
            vec!["notes@personal"],
            "a disabled project plugin must not enter the project runtime"
        );
        assert_eq!(
            catalogue
                .disabled_for_project(fixture.project.path())
                .iter()
                .map(|plugin| plugin.id.as_str())
                .collect::<Vec<_>>(),
            vec!["deploy@my-team"],
            "the disabled project plugin remains visible in its project scope"
        );
        assert!(
            catalogue
                .disabled_for_project(fixture.other.path())
                .is_empty(),
            "a project-disabled plugin must not leak into another project"
        );
    }

    #[test]
    fn discovery_reads_only_the_manifest_not_provider_configuration() {
        // A plugin's own `.mcp.json` / `.hooks.json` sit beside its manifest in
        // the cache and are read by the Component, never by discovery.
        let fixture = fixture();
        let global_root = plugin_cache_root(fixture.home.path()).join("personal/notes/1.0.0");
        let project_root = plugin_cache_root(fixture.home.path()).join("my-team/deploy/1.0.0");

        fs::write(
            global_root.join(".mcp.json"),
            r#"{"mcpServers":{"global":{"command":"global-server"}}}"#,
        )
        .unwrap();
        fs::write(
            project_root.join(".hooks.json"),
            r#"{"hooks":{"PostToolUse":[{"hooks":[]}]}}"#,
        )
        .unwrap();

        let settings = PluginSettings {
            plugins: settings::entries(&["notes@personal"]),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["deploy@my-team".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        let plugins = catalogue.for_project(fixture.project.path());
        assert_eq!(
            ids(&plugins),
            vec!["deploy@my-team", "notes@personal"],
            "scope configuration files must not affect Wasmtime discovery"
        );
        assert_eq!(
            plugins[0].root, project_root,
            "a project-scoped plugin keeps its cached root"
        );
        assert_eq!(
            plugins[1].root, global_root,
            "discovery keeps the plugin root only for component lookup"
        );
    }

    #[test]
    fn a_project_scoped_plugin_is_visible_only_to_its_project() {
        let fixture = fixture();
        let root = plugin_cache_root(fixture.home.path()).join("my-team/wasm-echo/1.0.0");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(manifest::MANIFEST_FILE),
            r#"{
              "name": "wasm-echo",
              "runtime": {
                "module": "plugin.wasm",
                "apiVersion": "deluxe.harness/plugin@0.1"
              }
            }"#,
        )
        .expect("the project Wasm manifest is writable");

        let settings = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["wasm-echo@my-team".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        let project_plugins = catalogue.for_project(fixture.project.path());
        let wasm = project_plugins
            .iter()
            .find(|plugin| plugin.id == "wasm-echo@my-team")
            .expect("the project-scoped Wasm plugin is discovered");
        assert!(
            wasm.manifest.wasm_runtime().is_some(),
            "the runtime declaration is preserved for the worker"
        );
        assert!(
            catalogue.for_project(fixture.other.path()).is_empty(),
            "a project Wasm plugin must not leak into another project"
        );
    }

    #[test]
    fn a_project_sees_its_own_plugins_alongside_the_global_ones() {
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&["notes@personal", "linter@bundled"]),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["deploy@my-team".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())),
            vec!["deploy@my-team", "linter@bundled", "notes@personal"],
            "global plus project, sorted by id"
        );
        assert_eq!(
            ids(&catalogue.for_project(fixture.other.path())),
            vec!["linter@bundled", "notes@personal"],
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
            plugins: settings::entries(&["notes@personal"]),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["deploy@my-team".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        let global: Vec<&str> = catalogue
            .global()
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect();
        assert_eq!(global, vec!["notes@personal"]);
        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())).len(),
            2,
            "the project itself still sees both"
        );
    }

    #[test]
    fn a_project_may_enable_an_installed_plugin_for_itself_alone() {
        // `[plugins.projects]` is not restricted to a plugin's own scope:
        // pinning an installed plugin to one repository is the point.
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([(
                project_key(fixture.project.path()),
                vec!["notes@personal".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())),
            vec!["notes@personal"]
        );
        assert!(catalogue.for_project(fixture.other.path()).is_empty());
    }

    #[test]
    fn an_installed_plugin_is_read_from_the_cache() {
        // The cache is the one source of plugins: an id is installed or it is
        // not.
        let home = tempfile::tempdir().unwrap();
        cache_plugin(home.path(), "curated", "notes", "1dc19589");

        let settings = PluginSettings {
            plugins: settings::entries(&["notes@curated"]),
            projects: BTreeMap::new(),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(home.path(), &settings);

        assert_eq!(
            ids(&catalogue.for_project(Path::new("/any"))),
            vec!["notes@curated"]
        );
    }

    #[test]
    fn the_newest_cached_version_wins() {
        let home = tempfile::tempdir().unwrap();
        let cache = plugin_cache_root(home.path()).join("bundled/linter");
        write_plugin(&cache.join("1.0.0"), "linter");
        write_plugin(&cache.join("2.1.0"), "linter");

        let settings = PluginSettings {
            plugins: settings::entries(&["linter@bundled"]),
            projects: BTreeMap::new(),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(home.path(), &settings);

        assert_eq!(
            catalogue.for_project(Path::new("/any"))[0].root,
            cache.join("2.1.0")
        );
    }

    #[test]
    fn an_id_that_names_nothing_is_skipped_without_taking_the_others_down() {
        let fixture = fixture();
        let settings = PluginSettings {
            plugins: settings::entries(&[
                "nope@personal",
                "notes@personal",
                "also-nope@nowhere",
                "malformed-id",
            ]),
            projects: BTreeMap::new(),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        assert_eq!(
            ids(&catalogue.for_project(Path::new("/any"))),
            vec!["notes@personal"],
            "one bad id must not stop the good ones"
        );
    }

    #[test]
    fn a_broken_manifest_is_skipped_rather_than_fatal() {
        let fixture = fixture();
        let broken = plugin_cache_root(fixture.home.path()).join("personal/broken/1.0.0");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(manifest::MANIFEST_FILE), "{ not json").unwrap();

        let settings = PluginSettings {
            plugins: settings::entries(&["broken@personal", "notes@personal"]),
            projects: BTreeMap::new(),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        assert_eq!(
            ids(&catalogue.for_project(Path::new("/any"))),
            vec!["notes@personal"]
        );
    }

    #[test]
    fn no_configuration_means_no_plugins_and_no_failure() {
        let home = tempfile::tempdir().unwrap();
        let catalogue = discover(home.path(), &PluginSettings::default());

        assert!(catalogue.for_project(Path::new("/any")).is_empty());
    }

    #[test]
    fn a_disabled_plugin_is_read_but_not_loaded() {
        // The plugins window must be able to show a plugin that is switched off
        // — that is what makes the switch reversible rather than a delete — so
        // discovery still reads it. What it must not do is hand it to anything
        // that would run it.
        let fixture = fixture();
        let mut config = PluginSettings::default();
        config.plugins.insert("notes@personal".into(), false);

        let catalogue = discover(fixture.home.path(), &config);

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
            vec!["notes@personal"]
        );
        assert!(
            catalogue.disabled()[0].manifest.wasm_runtime().is_some(),
            "a disabled plugin is still read, so the window can describe what turning it on brings"
        );
    }

    #[test]
    fn a_disabled_id_that_names_nothing_is_not_an_error() {
        // Turning a plugin off and later uninstalling it is a normal sequence.
        // The leftover row is silent: the user asked for nothing to happen.
        let fixture = fixture();
        let mut config = PluginSettings::default();
        config.plugins.insert("gone@personal".into(), false);

        let catalogue = discover(fixture.home.path(), &config);

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
                vec!["notes@personal".into()],
            )]),
            disabled_projects: BTreeMap::new(),
        };
        config.plugins.insert("notes@personal".into(), false);

        let catalogue = discover(fixture.home.path(), &config);

        assert!(catalogue.for_project(fixture.project.path()).is_empty());
    }

    #[test]
    fn a_trailing_separator_in_the_config_still_matches_the_project() {
        let fixture = fixture();
        let with_separator = format!("{}/", project_key(fixture.project.path()));
        let settings = PluginSettings {
            plugins: BTreeMap::new(),
            projects: BTreeMap::from([(with_separator, vec!["deploy@my-team".into()])]),
            disabled_projects: BTreeMap::new(),
        };
        let catalogue = discover(fixture.home.path(), &settings);

        assert_eq!(
            ids(&catalogue.for_project(fixture.project.path())),
            vec!["deploy@my-team"]
        );
    }

    #[test]
    fn a_plugin_without_a_wasmtime_runtime_is_not_loaded() {
        // A cached root with no Wasmtime runtime exercises the wiring from a
        // root to `LoadedPlugin` end to end; only Component plugins belong in
        // the catalogue.
        let fixture = fixture();
        let root = plugin_cache_root(fixture.home.path()).join("personal/no-runtime/1.0.0");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(manifest::MANIFEST_FILE),
            r#"{"name":"no-runtime","version":"1.0.0"}"#,
        )
        .unwrap();
        let settings = PluginSettings {
            plugins: settings::entries(&["no-runtime@personal"]),
            projects: BTreeMap::new(),
            disabled_projects: BTreeMap::new(),
        };

        assert!(
            discover(fixture.home.path(), &settings)
                .for_project(Path::new("/any"))
                .is_empty(),
            "only Wasmtime component plugins belong in the catalogue"
        );
    }

    #[test]
    fn the_global_configuration_root_is_the_deluxe_agents_directory_not_the_home() {
        // The whole point of the indirection: a global Component's generic file
        // capability must not be bound to `~`, where it could read `~/.ssh` or a
        // key file. `~/.deluxe-agents` is plugin configuration.
        let home = Path::new("/home/someone");
        assert_eq!(
            global_configuration_root(home),
            PathBuf::from("/home/someone/.deluxe-agents")
        );
        assert_ne!(global_configuration_root(home), home.to_path_buf());
    }
}
