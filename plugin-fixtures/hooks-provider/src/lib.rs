use std::sync::{Mutex, OnceLock};

wit_bindgen::generate!({
    path: "../../wit",
    world: "harness-plugin",
    async: true,
});

use exports::deluxe::harness::plugin::Guest;

struct HooksProvider;

#[derive(Clone)]
struct HookSpec {
    id: String,
    label: String,
    matcher: Option<regex::Regex>,
    command: String,
}

static HOOKS: OnceLock<Mutex<Vec<HookSpec>>> = OnceLock::new();

fn configured_hooks() -> Vec<HookSpec> {
    HOOKS
        .get()
        .and_then(|hooks| hooks.lock().ok().map(|hooks| hooks.clone()))
        .unwrap_or_default()
}

async fn parse_hooks() -> Result<Vec<HookSpec>, String> {
    let bytes = match deluxe::harness::host::read_plugin_file(".hooks.json".into()).await {
        Ok(bytes) => bytes,
        Err(error) if error.starts_with("plugin_file_not_found:") => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let text =
        String::from_utf8(bytes).map_err(|_| "Hooks configuration is not UTF-8".to_string())?;
    let config: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| "Hooks configuration is invalid".to_string())?;
    let Some(groups) = config
        .get("hooks")
        .and_then(|hooks| hooks.get("PostToolUse").cloned())
        .and_then(|groups| groups.as_array().cloned())
    else {
        return Ok(Vec::new());
    };

    let mut result = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        let pattern = group
            .get("matcher")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        let matcher = if pattern.is_empty() {
            None
        } else {
            let Ok(matcher) = regex::Regex::new(pattern) else {
                continue;
            };
            Some(matcher)
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
            result.push(HookSpec {
                id: format!("hook-{group_index}-{entry_index}"),
                label: command.to_string(),
                matcher: matcher.clone(),
                command: command.to_string(),
            });
        }
    }
    Ok(result)
}

fn matches_tool(spec: &HookSpec, tool: &str) -> bool {
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

fn event_field<'a>(event: &'a serde_json::Value, name: &str) -> &'a str {
    event.get(name).and_then(serde_json::Value::as_str).unwrap_or("")
}

fn ui_document(request: &serde_json::Value) -> String {
    let plugin_id = event_field(request, "pluginId");
    let surface_id = event_field(request, "surfaceId");
    let children: Vec<serde_json::Value> = configured_hooks()
        .into_iter()
        .map(|hook| {
            serde_json::json!({
                "type": "section",
                "id": hook.id,
                "title": hook.label,
                "children": [{
                    "type": "text",
                    "text": "PostToolUse hook",
                    "emphasis": "normal"
                }]
            })
        })
        .collect();
    serde_json::json!({
        "schemaVersion": 1,
        "pluginId": plugin_id,
        "surfaceId": surface_id,
        "revision": 1,
        "title": "Hooks",
        "root": {
            "type": "column",
            "children": children
        }
    })
    .to_string()
}

impl Guest for HooksProvider {
    async fn configure() -> Result<(), String> {
        let hooks = parse_hooks().await?;
        HOOKS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .map_err(|_| "hook state is poisoned")?
            .clone_from(&hooks);
        Ok(())
    }

    async fn list_tools() -> String {
        "[]".into()
    }

    async fn execute_tool(_name: String, _arguments_json: String) -> Result<String, String> {
        Err("hooks provider has no regular tools".into())
    }

    async fn list_hooks() -> String {
        serde_json::to_string(
            &configured_hooks()
                .into_iter()
                .map(|hook| {
                    serde_json::json!({
                        "id": hook.id,
                        "label": hook.label,
                        "tools": ["*"]
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into())
    }

    async fn invoke_hook(hook_id: String, event_json: String) -> Result<String, String> {
        let hook = configured_hooks()
            .into_iter()
            .find(|hook| hook.id == hook_id)
            .ok_or_else(|| "unknown hook".to_string())?;
        let event: serde_json::Value =
            serde_json::from_str(&event_json).map_err(|_| "hook event is invalid")?;
        if !matches_tool(&hook, event_field(&event, "tool")) {
            return Ok(r#"{"output":"","failed":false,"matched":false}"#.into());
        }
        let arguments = serde_json::json!({
            "command": hook.command,
            "cwd": event_field(&event, "project")
        })
        .to_string();
        let result = deluxe::harness::host::invoke_tool("exec".into(), arguments).await?;
        let result: serde_json::Value =
            serde_json::from_str(&result).map_err(|_| "host exec output is invalid")?;
        let output = result
            .get("content")
            .and_then(serde_json::Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let failed = result
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Ok(serde_json::json!({
            "output": output,
            "failed": failed,
            "matched": true
        })
        .to_string())
    }

    async fn open_surface(request_json: String) -> Result<String, String> {
        let request: serde_json::Value =
            serde_json::from_str(&request_json).map_err(|_| "surface request is invalid")?;
        Ok(ui_document(&request))
    }

    async fn handle_action(_action_json: String) -> Result<String, String> {
        Err("hooks provider has no UI actions".into())
    }

    async fn close_surface(_surface_id: String) {}
}

export!(HooksProvider);
