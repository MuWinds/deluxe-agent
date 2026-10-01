use std::sync::{Mutex, OnceLock};

wit_bindgen::generate!({
    path: "../../wit",
    world: "harness-plugin",
    async: true,
});

use exports::deluxe::harness::plugin::Guest;

struct EchoTool;

#[derive(Clone)]
struct HookSpec {
    id: String,
    label: String,
    matcher: Option<regex::Regex>,
    command: String,
}

static HOOKS: OnceLock<Mutex<Vec<HookSpec>>> = OnceLock::new();
static PLUGIN_ROOT: OnceLock<Mutex<String>> = OnceLock::new();

fn request_field(request: &str, field: &str, fallback: &str) -> String {
    serde_json::from_str::<serde_json::Value>(request)
        .ok()
        .and_then(|value| {
            value
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| fallback.into())
}

fn hooks_from_config(config: &serde_json::Value) -> Result<Vec<HookSpec>, String> {
    let Some(groups) = config
        .get("hooksJson")
        .and_then(serde_json::Value::as_str)
        .and_then(|hooks| serde_json::from_str::<serde_json::Value>(hooks).ok())
        .and_then(|hooks| hooks.get("hooks").cloned())
        .and_then(|hooks| hooks.get("PostToolUse").cloned())
    else {
        return Ok(Vec::new());
    };
    let Some(groups) = groups.as_array() else {
        return Ok(Vec::new());
    };

    let mut result = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        let pattern = group
            .get("matcher")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let matcher = if pattern.is_empty() {
            None
        } else {
            match regex::Regex::new(&pattern) {
                Ok(matcher) => Some(matcher),
                Err(_) => continue,
            }
        };
        let Some(entries) = group.get("hooks").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for (entry_index, entry) in entries.iter().enumerate() {
            if entry.get("type").and_then(serde_json::Value::as_str) != Some("command") {
                continue;
            }
            let Some(command) = entry
                .get("command")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|command| !command.is_empty())
            else {
                continue;
            };
            let id = format!("hook-{group_index}-{entry_index}");
            result.push(HookSpec {
                id,
                label: command.to_string(),
                matcher: matcher.clone(),
                command: command.to_string(),
            });
        }
    }
    Ok(result)
}

fn hook_matches(spec: &HookSpec, tool: &str) -> bool {
    let Some(matcher) = &spec.matcher else {
        return true;
    };
    let aliases: &[&str] = match tool {
        "apply_patch" => &["Write", "Edit", "MultiEdit", "NotebookEdit"],
        "read_file" => &["Read", "NotebookRead"],
        "exec" => &["Bash", "Shell"],
        "list_dir" => &["LS", "Glob", "Grep"],
        _ => &[],
    };
    std::iter::once(tool)
        .chain(aliases.iter().copied())
        .any(|name| matcher.is_match(name))
}

fn configured_hooks() -> Vec<HookSpec> {
    HOOKS
        .get()
        .and_then(|hooks| hooks.lock().ok().map(|hooks| hooks.clone()))
        .unwrap_or_default()
}

fn configured_root() -> String {
    PLUGIN_ROOT
        .get()
        .and_then(|root| root.lock().ok().map(|root| root.clone()))
        .unwrap_or_default()
}

impl Guest for EchoTool {
    async fn configure(config_json: String) -> Result<(), String> {
        let config: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|_| "provider config is invalid")?;
        let hooks = hooks_from_config(&config)?;
        HOOKS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .map_err(|_| "hook state is poisoned")?
            .clone_from(&hooks);
        PLUGIN_ROOT
            .get_or_init(|| Mutex::new(String::new()))
            .lock()
            .map_err(|_| "plugin root state is poisoned")?
            .clone_from(
                &config
                    .get("pluginRoot")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        Ok(())
    }

    async fn list_tools() -> String {
        r#"[{"name":"wasm_echo","summary":"Echo fixture","description":"Returns a bounded fixture result","guidelines":[],"inputSchema":{"type":"object","properties":{},"required":[]},"hostValidatesArguments":true,"mutating":false}]"#.into()
    }

    async fn execute_tool(name: String, arguments_json: String) -> Result<String, String> {
        if name != "wasm_echo" {
            return Err(format!("unknown fixture tool: {name}"));
        }
        Ok(format!(
            r#"{{"content":[{{"type":"text","text":{arguments_json:?}}}],"isError":false,"truncated":false}}"#
        ))
    }

    async fn list_hooks() -> String {
        let descriptors: Vec<serde_json::Value> = configured_hooks()
            .into_iter()
            .map(|hook| {
                serde_json::json!({
                    "id": hook.id,
                    "label": hook.label,
                    "tools": ["*"],
                })
            })
            .collect();
        serde_json::to_string(&descriptors).unwrap_or_else(|_| "[]".into())
    }

    async fn invoke_hook(hook_id: String, event_json: String) -> Result<String, String> {
        let hook = configured_hooks()
            .into_iter()
            .find(|hook| hook.id == hook_id)
            .ok_or_else(|| format!("unknown fixture hook: {hook_id}"))?;
        let tool = request_field(&event_json, "tool", "");
        if !hook_matches(&hook, &tool) {
            return Ok(r#"{"output":"","failed":false,"matched":false}"#.into());
        }
        let arguments = serde_json::json!({
            "command": hook.command,
            "cwd": configured_root(),
        })
        .to_string();
        let execution = deluxe::harness::host::invoke_tool("exec".into(), arguments).await?;
        let execution: serde_json::Value =
            serde_json::from_str(&execution).map_err(|_| "host exec output is invalid")?;
        let output = execution
            .get("content")
            .and_then(serde_json::Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let failed = execution
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        serde_json::to_string(&serde_json::json!({
            "output": output,
            "failed": failed,
            "matched": true,
        }))
        .map_err(|_| "hook output is invalid".into())
    }

    async fn list_mcp_servers() -> String {
        "[]".into()
    }

    async fn list_mcp_tools(_server: String) -> String {
        "[]".into()
    }

    async fn invoke_mcp_tool(
        _server: String,
        _name: String,
        _arguments_json: String,
    ) -> Result<String, String> {
        Err("echo fixture has no MCP servers".into())
    }

    async fn open_surface(request_json: String) -> Result<String, String> {
        let plugin_id = request_field(&request_json, "pluginId", "echo@test");
        let surface_id = request_field(&request_json, "surfaceId", "main");
        Ok(format!(
            r#"{{"schemaVersion":1,"pluginId":{plugin_id:?},"surfaceId":{surface_id:?},"revision":1,"title":"Echo","root":{{"type":"button","id":"echo","label":"Echo","action":"echo","enabled":true}}}}"#
        ))
    }

    async fn handle_action(action_json: String) -> Result<String, String> {
        let plugin_id = request_field(&action_json, "pluginId", "echo@test");
        let surface_id = request_field(&action_json, "surfaceId", "main");
        Ok(format!(
            r#"{{"schemaVersion":1,"pluginId":{plugin_id:?},"surfaceId":{surface_id:?},"revision":2,"title":"Echo","root":{{"type":"text","text":"handled","emphasis":"normal"}}}}"#
        ))
    }

    async fn close_surface(_surface_id: String) {}
}

export!(EchoTool);
