//! Native implementations of the harness ports.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::{self, Config};
use crate::context::summarize;
use crate::error::{AgentError, Result};
use crate::llm::{AssistantTurn, LlmClient, Message, StreamFragment, ThinkingLevel, ToolCall};
use crate::plugins::{self, PluginCatalogue, PluginSettings, Scope};
use crate::tools::jobs::{JobRegistry, JobSnapshot};
use crate::tools::{validate_arguments, ToolDescriptor, ToolRegistry, ToolSettings};

use super::ports::{
    ConfigStore, ContextCompactor, InstalledPlugin, JobFactory, JobRuntime, LlmProvider,
    LlmStreamEvent, LlmStreamSink, PluginEvent, PluginEventRuntime, PluginManager, PromptProvider,
    SecretStore, ToolContext, ToolExecution, ToolRuntime,
};
use super::types::AuditOutcome;

#[derive(Clone)]
pub struct NativeLlmProvider {
    client: LlmClient,
}

impl NativeLlmProvider {
    /// Wraps the existing OpenAI-compatible client as a runtime provider.
    pub fn new(client: LlmClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl LlmProvider for NativeLlmProvider {
    async fn stream_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        thinking: Option<ThinkingLevel>,
        cancel: &CancellationToken,
        sink: &mut dyn LlmStreamSink,
    ) -> Result<AssistantTurn> {
        self.client
            .stream_turn(messages, tools, thinking, cancel, |fragment| {
                let event = match fragment {
                    StreamFragment::Reset => LlmStreamEvent::Reset,
                    StreamFragment::Reasoning(text) => LlmStreamEvent::Reasoning(text.to_string()),
                    StreamFragment::Content(text) => LlmStreamEvent::Content(text.to_string()),
                };
                sink.push(event);
            })
            .await
    }

    async fn complete_turn(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn> {
        self.client.complete_turn(messages, tools, cancel).await
    }
}

pub struct NativeContextCompactor {
    llm: Arc<dyn LlmProvider>,
}

impl NativeContextCompactor {
    /// Creates a compactor backed by the same provider as the agent loop.
    pub fn new(llm: Arc<dyn LlmProvider>) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl ContextCompactor for NativeContextCompactor {
    async fn summarize(
        &self,
        history: &[Message],
        tools: &Value,
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        summarize(history, tools, self.llm.as_ref(), cancel).await
    }
}

pub struct NativeJobRuntime {
    jobs: Arc<JobRegistry>,
}

impl NativeJobRuntime {
    /// Adapts the registry shared by the built-in job tools.
    pub fn new(jobs: Arc<JobRegistry>) -> Self {
        Self { jobs }
    }
}

#[async_trait]
impl JobRuntime for NativeJobRuntime {
    fn list(&self) -> Vec<JobSnapshot> {
        self.jobs.list()
    }

    async fn kill(&self, id: &str, reason: Option<&str>) -> Result<()> {
        self.jobs.kill(id, reason).map(|_| ())
    }

    fn start_result(&self, kind: &'static str, label: String, make: JobFactory) -> String {
        self.jobs.start_result(kind, label, make)
    }

    fn drain_notifications(&self) -> Vec<String> {
        self.jobs.drain_notifications()
    }
}

pub struct NativeConfigStore {
    path: Option<std::path::PathBuf>,
}

impl NativeConfigStore {
    /// Creates a TOML store targeting the config path resolved at startup.
    pub fn new(path: Option<std::path::PathBuf>) -> Self {
        Self { path }
    }
}

#[async_trait]
impl ConfigStore for NativeConfigStore {
    async fn save(&self, config: &Config) -> Result<()> {
        let Some(path) = self.path.clone() else {
            return Err(AgentError::internal(
                "No config directory is available on this system",
            ));
        };
        let config = config.clone();
        tokio::task::spawn_blocking(move || config::save_to(&path, &config))
            .await
            .map_err(|error| AgentError::internal(format!("Config store worker failed: {error}")))?
    }
}

pub struct NativeSecretStore;

impl NativeSecretStore {
    /// Creates a keyring-backed store for model credentials.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SecretStore for NativeSecretStore {
    async fn save_api_key(&self, api_key: &str) -> Result<()> {
        let api_key = api_key.to_string();
        tokio::task::spawn_blocking(move || config::store_api_key(&api_key))
            .await
            .map_err(|error| AgentError::internal(format!("Secret store worker failed: {error}")))?
    }
}

pub struct NativePluginManager {
    home: std::path::PathBuf,
}

impl NativePluginManager {
    /// Creates a manager rooted at the user's plugin installation directory.
    pub fn new(home: std::path::PathBuf) -> Self {
        Self { home }
    }
}

#[async_trait]
impl PluginManager for NativePluginManager {
    async fn discover(&self, settings: PluginSettings) -> Result<Arc<PluginCatalogue>> {
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || Arc::new(plugins::discover(&home, &settings)))
            .await
            .map_err(|error| {
                AgentError::internal(format!("Plugin discovery worker failed: {error}"))
            })
    }

    async fn ensure_bundled_defaults(&self, settings: PluginSettings) -> Result<PluginSettings> {
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || ensure_bundled_defaults(&home, settings))
            .await
            .map_err(|error| {
                AgentError::internal(format!("Bundled plugin install worker failed: {error}"))
            })?
    }

    async fn install_local(&self, component_path: PathBuf) -> Result<InstalledPlugin> {
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || install_local_plugin(&home, &component_path))
            .await
            .map_err(|error| {
                AgentError::internal(format!("Plugin install worker failed: {error}"))
            })?
    }

    async fn discard_install(&self, root: PathBuf) -> Result<()> {
        let cache = self.cache_root();
        tokio::task::spawn_blocking(move || {
            if !root.starts_with(&cache) || !root.exists() {
                return Ok(());
            }
            std::fs::remove_dir_all(&root)
                .map_err(|error| AgentError::from_io("Failed to roll back plugin install", error))
        })
        .await
        .map_err(|error| AgentError::internal(format!("Plugin rollback worker failed: {error}")))?
    }

    async fn uninstall(
        &self,
        id: &str,
        scope: &Scope,
        catalogue: Arc<PluginCatalogue>,
    ) -> Result<()> {
        let Some(root) = catalogue
            .find_in_scope(id, scope)
            .map(|plugin| plugin.root.clone())
        else {
            return Err(AgentError::invalid_params(format!(
                "Plugin `{id}` has no installed location"
            )));
        };

        let cache = self.cache_root();
        if !root.starts_with(&cache) {
            return Ok(());
        }

        tokio::task::spawn_blocking(move || {
            std::fs::remove_dir_all(&root)
                .map_err(|error| AgentError::from_io("Failed to uninstall the plugin", error))
        })
        .await
        .map_err(|error| AgentError::internal(format!("Plugin uninstall worker failed: {error}")))?
    }
}

impl NativePluginManager {
    fn cache_root(&self) -> PathBuf {
        plugins::plugin_cache_root(&self.home)
    }
}

const LOCAL_NAMESPACE: &str = "deluxe-local";

fn ensure_bundled_defaults(home: &Path, mut settings: PluginSettings) -> Result<PluginSettings> {
    for package in plugins::defaults::PLUGINS {
        let id = format!("{}@{}", package.name, plugins::defaults::NAMESPACE);
        if settings.plugins.get(&id) == Some(&false) {
            continue;
        }

        let target = plugins::plugin_cache_root(home)
            .join(plugins::defaults::NAMESPACE)
            .join(package.name)
            .join(package.version);
        // The managed cache is host-owned and the embedded bytes are the source
        // of truth, so a copy that no longer matches this build is refreshed
        // rather than reported as fatal. A rebuilt bundled component whose
        // version did not change would otherwise fail the whole install and
        // blank the plugin list, taking the default MCP and Hooks providers
        // down with it.
        if !target.exists() || !is_embedded_plugin(package, &target)? {
            install_embedded_plugin(package, &target)?;
        }
        settings.set_enabled(&id, true);
    }
    settings.normalize();
    Ok(settings)
}

fn install_embedded_plugin(
    package: &plugins::defaults::EmbeddedPlugin,
    target: &Path,
) -> Result<()> {
    plugins::wasm_runtime::validate_component_bytes(package.component)?;
    let manifest: plugins::PluginManifest =
        serde_json::from_str(package.manifest).map_err(|error| {
            AgentError::internal(format!("Bundled plugin manifest is invalid: {error}"))
        })?;
    let runtime = manifest.wasm_runtime().ok_or_else(|| {
        AgentError::new(
            crate::error::code::PLUGIN_LOAD_FAILED,
            "Bundled plugin does not declare a supported Wasmtime runtime",
        )
    })?;
    if manifest.name != package.name || manifest.version.as_deref() != Some(package.version) {
        return Err(AgentError::internal(
            "Bundled plugin metadata does not match its package declaration",
        ));
    }
    if runtime.module != "plugin.wasm" {
        return Err(AgentError::internal(
            "Bundled plugin must use the embedded plugin.wasm entry",
        ));
    }

    if let Err(error) = write_embedded_plugin(package, target) {
        let _ = std::fs::remove_dir_all(target);
        return Err(error);
    }
    if let Err(error) = is_embedded_plugin(package, target) {
        let _ = std::fs::remove_dir_all(target);
        return Err(error);
    }
    Ok(())
}

fn write_embedded_plugin(package: &plugins::defaults::EmbeddedPlugin, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)
        .map_err(|error| AgentError::from_io("Create bundled plugin directory", error))?;
    std::fs::write(
        target.join(plugins::manifest::MANIFEST_FILE),
        package.manifest,
    )
    .map_err(|error| AgentError::from_io("Write bundled plugin manifest", error))?;
    std::fs::write(target.join("plugin.wasm"), package.component)
        .map_err(|error| AgentError::from_io("Write bundled Wasmtime component", error))?;
    Ok(())
}

fn is_embedded_plugin(package: &plugins::defaults::EmbeddedPlugin, target: &Path) -> Result<bool> {
    let manifest = match plugins::manifest::read_plugin(target) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    let Some(runtime) = manifest.wasm_runtime() else {
        return Ok(false);
    };
    if manifest.name != package.name
        || manifest.version.as_deref() != Some(package.version)
        || runtime.module != "plugin.wasm"
    {
        return Ok(false);
    }
    let component = runtime.resolve_entry(target)?;
    let bytes = std::fs::read(component)
        .map_err(|error| AgentError::from_io("Read bundled Wasmtime component", error))?;
    if bytes != package.component {
        return Ok(false);
    }
    Ok(true)
}

fn install_local_plugin(home: &Path, selected_component: &Path) -> Result<InstalledPlugin> {
    let source = selected_plugin_root(selected_component)?;
    let manifest = plugins::manifest::read_plugin(&source)?;
    if !valid_cache_segment(&manifest.name) {
        return Err(AgentError::invalid_params(
            "The plugin manifest name must contain only letters, numbers, dots, underscores, or hyphens",
        ));
    }
    let runtime = manifest.wasm_runtime().ok_or_else(|| {
        AgentError::new(
            crate::error::code::PLUGIN_LOAD_FAILED,
            "The selected plugin does not declare a supported Wasmtime runtime",
        )
    })?;
    let component = runtime.resolve_entry(&source)?;
    plugins::wasm_runtime::validate_component_file(&component)?;

    let version = manifest.version.as_deref().unwrap_or("local");
    if !valid_cache_segment(version) {
        return Err(AgentError::invalid_params(
            "The plugin version cannot be used as a cache directory name",
        ));
    }

    let cache = plugins::plugin_cache_root(home);
    let target = cache
        .join(LOCAL_NAMESPACE)
        .join(&manifest.name)
        .join(version);
    if !target.starts_with(&cache) {
        return Err(AgentError::new(
            crate::error::code::PLUGIN_PERMISSION_DENIED,
            "The plugin cache path escapes the managed cache directory",
        ));
    }
    if target != source && (source.starts_with(&target) || target.starts_with(&source)) {
        return Err(AgentError::invalid_params(
            "The selected plugin directory cannot contain its destination cache",
        ));
    }

    let id = format!("{}@{LOCAL_NAMESPACE}", manifest.name);
    if target == source {
        return Ok(InstalledPlugin {
            id,
            root: target,
            copied: false,
        });
    }

    if target.exists() {
        return Err(AgentError::invalid_params(
            "This Wasmtime plugin version is already imported; uninstall it or change its manifest version before importing again",
        ));
    }
    if let Err(error) = copy_plugin_tree(&source, &target) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(error);
    }

    Ok(InstalledPlugin {
        id,
        root: target,
        copied: true,
    })
}

fn selected_plugin_root(selected: &Path) -> Result<PathBuf> {
    let selected = selected
        .canonicalize()
        .map_err(|error| AgentError::from_io("Resolve the selected Wasmtime component", error))?;
    if selected.is_dir() {
        return Ok(selected);
    }
    if !selected.is_file()
        || !selected
            .extension()
            .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("wasm"))
    {
        return Err(AgentError::invalid_params(
            "Select a plugin directory or a `.wasm` component file",
        ));
    }

    let mut candidate = selected.parent();
    while let Some(root) = candidate {
        let manifest_path = root.join(plugins::manifest::MANIFEST_FILE);
        if manifest_path.is_file() {
            let manifest = plugins::manifest::read_plugin(root)?;
            let runtime = manifest.wasm_runtime().ok_or_else(|| {
                AgentError::new(
                    crate::error::code::PLUGIN_LOAD_FAILED,
                    "The selected plugin does not declare a supported Wasmtime runtime",
                )
            })?;
            match runtime.resolve_entry(root) {
                Ok(entry) if entry == selected => return Ok(root.to_path_buf()),
                Ok(_) => {}
                Err(error) if error.code == crate::error::code::IO => {}
                Err(error) => return Err(error),
            }
        }
        candidate = root.parent();
    }

    Err(AgentError::invalid_params(
        "No root `plugin.json` declares the selected component; choose the `.wasm` file referenced by `runtime.module`",
    ))
}

fn copy_plugin_tree(source: &Path, target: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|error| {
            AgentError::internal(format!("Read the selected plugin directory: {error}"))
        })?;
        if entry.file_type().is_symlink() {
            return Err(AgentError::new(
                crate::error::code::PLUGIN_PERMISSION_DENIED,
                "Symlinks are not allowed in an imported Wasmtime plugin",
            ));
        }
        let relative = entry.path().strip_prefix(source).map_err(|_| {
            AgentError::internal("The selected plugin path could not be made relative")
        })?;
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&destination)
                .map_err(|error| AgentError::from_io("Create the plugin cache directory", error))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    AgentError::from_io("Create the plugin cache parent directory", error)
                })?;
            }
            std::fs::copy(entry.path(), &destination)
                .map_err(|error| AgentError::from_io("Copy the Wasmtime plugin file", error))?;
        }
    }
    Ok(())
}

fn valid_cache_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

pub struct RegistryToolRuntime {
    registry: Arc<ToolRegistry>,
    settings: Arc<RwLock<ToolSettings>>,
}

impl RegistryToolRuntime {
    /// Moves registry dispatch policy behind the tool runtime port.
    pub fn new(registry: Arc<ToolRegistry>, settings: Arc<RwLock<ToolSettings>>) -> Self {
        Self { registry, settings }
    }

    async fn current_settings(&self, project: &Path) -> ToolSettings {
        let mut settings = self.settings.read().await.clone();
        settings.working_directory = project.to_path_buf();
        settings
    }
}

#[async_trait]
impl ToolRuntime for RegistryToolRuntime {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.registry.descriptors()
    }

    async fn context(&self, project: &Path) -> ToolContext {
        let settings = self.current_settings(project).await;
        ToolContext {
            project: project.to_path_buf(),
            working_directory: settings.working_directory,
            timeout: Duration::from_millis(settings.default_timeout_ms),
            max_output_chars: settings.max_output_chars,
        }
    }

    async fn execute(
        &self,
        call: &ToolCall,
        context: &ToolContext,
        cancel: &CancellationToken,
    ) -> Result<ToolExecution> {
        let name = call.function.name.as_str();
        let tool = self.registry.require(name)?;
        let descriptor = tool.descriptor();
        let arguments: Value = serde_json::from_str(&call.function.arguments).map_err(|error| {
            AgentError::invalid_params(format!("Could not parse tool arguments as JSON: {error}"))
        })?;

        if descriptor.host_validates_arguments {
            validate_arguments(&descriptor.input_schema, &arguments)?;
        }

        let mut settings = self.current_settings(&context.project).await;
        settings.working_directory = context.working_directory.clone();
        let timeout = if tool.bounds_own_timeout() {
            const HOST_TIMEOUT_CEILING_MS: u64 = 605_000;
            Duration::from_millis(settings.default_timeout_ms.max(HOST_TIMEOUT_CEILING_MS))
        } else {
            context.timeout
        };

        let output = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AgentError::cancelled()),
            result = tokio::time::timeout(timeout, tool.execute(arguments, &settings)) => {
                match result {
                    Ok(Ok(output)) => output,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(AgentError::timeout(format!(
                            "`{name}` exceeded the {timeout:?} host limit"
                        )));
                    }
                }
            }
        };

        let outcome = if output.is_error {
            AuditOutcome::Failed
        } else {
            AuditOutcome::Executed
        };

        Ok(ToolExecution {
            output: output.truncate_to(context.max_output_chars),
            outcome,
        })
    }
}

struct EmptyPluginEventRuntime;

#[async_trait]
impl PluginEventRuntime for EmptyPluginEventRuntime {
    async fn dispatch(&self, _event: &PluginEvent, _cancel: &CancellationToken) -> Result<String> {
        Ok(String::new())
    }
}

/// Builds host services for a registry and its model provider.
pub fn native_services(
    client: LlmClient,
    registry: Arc<ToolRegistry>,
    settings: Arc<RwLock<ToolSettings>>,
    prompts: Arc<dyn PromptProvider>,
) -> super::ports::AgentServices {
    let llm: Arc<dyn LlmProvider> = Arc::new(NativeLlmProvider::new(client));
    let native_tools = Arc::new(RegistryToolRuntime::new(registry.clone(), settings));
    let tools: Arc<dyn ToolRuntime> = native_tools.clone();
    let jobs: Arc<dyn JobRuntime> = Arc::new(NativeJobRuntime::new(registry.jobs().clone()));
    let context: Arc<dyn ContextCompactor> = Arc::new(NativeContextCompactor::new(llm.clone()));
    let events: Arc<dyn PluginEventRuntime> = Arc::new(EmptyPluginEventRuntime);
    super::ports::AgentServices {
        llm,
        tools,
        context,
        prompts,
        events,
        jobs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    #[tokio::test]
    async fn native_config_store_writes_to_its_injected_path() {
        let dir = tempfile::tempdir().expect("a temp directory is available");
        let path = dir.path().join("nested").join("config.toml");
        let store = NativeConfigStore::new(Some(path.clone()));
        let mut config = Config::default();
        config.llm.retry_count = 7;

        store.save(&config).await.expect("the config is saved");

        let text = fs::read_to_string(path).expect("the config file exists");
        let saved: Config = toml::from_str(&text).expect("the config is valid TOML");
        assert_eq!(saved.llm.retry_count, 7);
    }

    #[tokio::test]
    async fn native_config_store_reports_an_unavailable_path() {
        let store = NativeConfigStore::new(None);

        assert!(
            store.save(&Config::default()).await.is_err(),
            "a missing config directory must not be reported as a successful save"
        );
    }

    #[tokio::test]
    async fn native_plugin_manager_only_deletes_cached_plugin_copies() {
        let home = tempfile::tempdir().expect("a temp directory is available");
        let cached = plugins::plugin_cache_root(home.path()).join("test/thing/1.0.0");
        fs::create_dir_all(&cached).expect("the cached plugin directory is writable");
        fs::write(
            cached.join("plugin.json"),
            r#"{"name":"thing","version":"1.0.0","runtime":{
                "module":"plugin.wasm",
                "apiVersion":"deluxe.harness/plugin@0.1"}}"#,
        )
        .expect("the manifest is written");
        let mut settings = PluginSettings::default();
        settings.set_enabled("thing@test", true);
        let catalogue = Arc::new(plugins::discover(home.path(), &settings));
        let manager = NativePluginManager::new(home.path().to_path_buf());

        manager
            .uninstall("thing@test", &Scope::Global, catalogue)
            .await
            .expect("the cached plugin is removed");

        assert!(!cached.exists(), "the cache copy is deleted");
    }

    #[tokio::test]
    async fn native_plugin_manager_installs_bundled_defaults_into_the_plugin_cache() {
        let home = tempfile::tempdir().expect("a temporary home is available");
        let manager = NativePluginManager::new(home.path().to_path_buf());

        let settings = manager
            .ensure_bundled_defaults(PluginSettings::default())
            .await
            .expect("bundled defaults are installed");
        for name in ["hooks", "mcp"] {
            let id = format!("{name}@deluxe-defaults");
            let root = plugins::plugin_cache_root(home.path())
                .join("deluxe-defaults")
                .join(name)
                .join("0.1.0");

            assert!(
                settings.is_enabled(&id),
                "the first run enables the bundled `{name}` Component"
            );
            assert!(
                root.join("plugin.json").is_file(),
                "the bundled `{name}` manifest is written beside the Component"
            );
            assert!(
                root.join("plugin.wasm").is_file(),
                "the bundled `{name}` Wasmtime Component is written to the managed cache"
            );
            assert!(
                !root.join(".hooks.json").is_file() && !root.join(".mcp.json").is_file(),
                "the bundled `{name}` package does not carry scope configuration"
            );
        }

        let catalogue = plugins::discover(home.path(), &settings);
        for id in ["hooks@deluxe-defaults", "mcp@deluxe-defaults"] {
            assert!(
                catalogue.global().iter().any(|plugin| plugin.id == id),
                "the bundled `{id}` Component is discoverable through the normal plugin path"
            );
        }
    }

    #[tokio::test]
    async fn native_plugin_manager_keeps_bundled_defaults_idempotent() {
        let home = tempfile::tempdir().expect("a temporary home is available");
        let manager = NativePluginManager::new(home.path().to_path_buf());
        let first = manager
            .ensure_bundled_defaults(PluginSettings::default())
            .await
            .expect("the bundled provider is installed");
        let root = plugins::plugin_cache_root(home.path()).join("deluxe-defaults/mcp/0.1.0");
        let component = fs::read(root.join("plugin.wasm")).expect("the component is readable");
        fs::write(root.join("user-note.txt"), b"keep this file")
            .expect("an unrelated cache file is writable");

        let second = manager
            .ensure_bundled_defaults(first.clone())
            .await
            .expect("a second startup is idempotent");

        assert_eq!(
            second, first,
            "a second startup does not churn plugin settings"
        );
        assert_eq!(
            fs::read(root.join("plugin.wasm")).expect("the component remains readable"),
            component,
            "a second startup does not rewrite the embedded Component"
        );
        assert_eq!(
            fs::read(root.join("user-note.txt")).expect("the unrelated file remains"),
            b"keep this file",
            "a second startup leaves other managed-cache files untouched"
        );
    }

    #[tokio::test]
    async fn native_plugin_manager_refreshes_a_stale_bundled_default() {
        // The cache is host-owned and the embedded bytes are authoritative, so a
        // copy left by an older build must be rewritten rather than reported as
        // fatal. Failing here used to blank the whole plugin list — including the
        // default MCP provider the user is asking for.
        let home = tempfile::tempdir().expect("a temporary home is available");
        let manager = NativePluginManager::new(home.path().to_path_buf());
        let first = manager
            .ensure_bundled_defaults(PluginSettings::default())
            .await
            .expect("the bundled defaults are installed");
        let root = plugins::plugin_cache_root(home.path()).join("deluxe-defaults/mcp/0.1.0");
        fs::write(
            root.join("plugin.wasm"),
            b"a stale component from an old build",
        )
        .expect("the cache copy is overwritable");

        let second = manager
            .ensure_bundled_defaults(first.clone())
            .await
            .expect("a mismatched bundled copy is refreshed, not fatal");

        assert_eq!(second, first, "the refresh does not churn plugin settings");
        let refreshed = fs::read(root.join("plugin.wasm")).expect("the component is readable");
        assert_ne!(
            refreshed, b"a stale component from an old build",
            "the stale bytes are overwritten with the embedded Component"
        );
        assert_eq!(
            plugins::discover(home.path(), &second)
                .global()
                .iter()
                .filter(|plugin| plugin.id == "mcp@deluxe-defaults")
                .count(),
            1,
            "the default MCP Component is discoverable after the refresh"
        );
    }

    #[tokio::test]
    async fn native_plugin_manager_respects_an_explicitly_disabled_bundled_default() {
        let home = tempfile::tempdir().expect("a temporary home is available");
        let manager = NativePluginManager::new(home.path().to_path_buf());
        let mut settings = PluginSettings::default();
        settings.set_enabled("mcp@deluxe-defaults", false);

        let returned = manager
            .ensure_bundled_defaults(settings.clone())
            .await
            .expect("an explicit disabled setting is preserved");

        // Only the MCP entry is asserted: `settings` starts with nothing but the
        // MCP id, and the startup pass legitimately *adds* the Hooks Component, so
        // a whole-struct equality could never hold. What must not change is that
        // the user's explicit off switch survives.
        assert!(
            returned.plugins.get("mcp@deluxe-defaults") == Some(&false),
            "the startup default does not force-enable the MCP Component the user disabled"
        );
        assert!(
            !plugins::plugin_cache_root(home.path())
                .join("deluxe-defaults/mcp/0.1.0")
                .exists(),
            "a disabled bundled MCP Component is not installed"
        );
        assert!(
            returned.is_enabled("hooks@deluxe-defaults"),
            "disabling MCP does not disable the separate Hooks Component"
        );
    }

    #[tokio::test]
    async fn native_plugin_manager_imports_a_wasmtime_plugin_into_its_cache() {
        let home = tempfile::tempdir().expect("a temp directory is available");
        let source = tempfile::tempdir().expect("a source plugin directory is available");
        fs::create_dir_all(source.path()).expect("the source plugin directory is writable");
        std::fs::write(
            source.path().join("plugin.json"),
            r#"{"name":"echo-tool","version":"1.0.0","runtime":{
                "module":"plugin.wasm",
                "apiVersion":"deluxe.harness/plugin@0.1"}}"#,
        )
        .expect("the source manifest is written");
        std::fs::write(
            source.path().join("plugin.wasm"),
            include_bytes!("../../plugin-src/echo-tool/plugin.wasm"),
        )
        .expect("the source component is written");

        let manager = NativePluginManager::new(home.path().to_path_buf());
        let installed = manager
            .install_local(source.path().join("plugin.wasm"))
            .await
            .expect("the Wasmtime plugin is imported");

        assert_eq!(installed.id, "echo-tool@deluxe-local");
        assert!(installed.copied, "a source outside the cache is copied");
        assert!(installed.root.join("plugin.wasm").is_file());

        let mut settings = PluginSettings::default();
        settings.set_enabled(&installed.id, true);
        let catalogue = plugins::discover(home.path(), &settings);
        assert_eq!(
            catalogue.global()[0].root,
            installed.root,
            "the imported component is discoverable through the normal cache path"
        );
    }

    #[tokio::test]
    async fn native_plugin_manager_rejects_a_wasm_file_not_declared_by_a_plugin_manifest() {
        let home = tempfile::tempdir().expect("a temp home is available");
        let source = tempfile::tempdir().expect("a source directory is available");
        fs::write(
            source.path().join("plugin.json"),
            r#"{"name":"echo-tool","version":"1.0.0","runtime":{
                "module":"different.wasm",
                "apiVersion":"deluxe.harness/plugin@0.1"}}"#,
        )
        .expect("the manifest is written");
        fs::write(
            source.path().join("plugin.wasm"),
            include_bytes!("../../plugin-src/echo-tool/plugin.wasm"),
        )
        .expect("the component is written");

        let manager = NativePluginManager::new(home.path().to_path_buf());
        let error = manager
            .install_local(source.path().join("plugin.wasm"))
            .await
            .expect_err("an undeclared component cannot be imported");

        assert!(
            error.message.contains("runtime.module"),
            "the error explains how to declare the component: {}",
            error.message
        );
    }
}
