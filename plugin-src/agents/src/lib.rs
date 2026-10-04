//! The agents provider Component.
//!
//! Reads the sub-agent roles a configuration scope holds and offers them to the
//! model as one `task` tool. It owns the whole layout — which directory holds
//! roles, which file marks one, how the frontmatter is shaped, and how the tool
//! is described — so the host never learns any of it. Executing the tool reaches
//! the host's generic `run-agent` capability, which owns the nested loop; the
//! Component only names the role and hands over its instructions.
//!
//! The instance is bound to one root (`~/.deluxe-agents`), and always looks in
//! `agents/` below it. A missing directory contributes no tool, not a failure.

use std::collections::BTreeMap;

use serde_json::{json, Value};

wit_bindgen::generate!({
    path: "../../wit",
    world: "harness-plugin",
    async: true,
});

use deluxe::harness::host;
use exports::deluxe::harness::plugin::Guest as PluginGuest;

/// The directory a scope's roles live in, relative to the scope root.
const AGENTS_DIR: &str = "agents";
/// A summary is capped at this many characters, so one long description cannot
/// crowd the tool's catalogue.
const MAX_DESCRIPTION_CHARS: usize = 200;

struct AgentsProvider;

impl PluginGuest for AgentsProvider {
    async fn configure() -> Result<(), String> {
        Ok(())
    }

    async fn describe() -> Result<String, String> {
        // Sub-agents contribute tools, not model metadata; the host ignores
        // every key this could carry.
        Ok("{}".into())
    }

    async fn list_tools() -> String {
        let agents = load_agents().await;
        if agents.is_empty() {
            return "[]".into();
        }

        let catalogue = agents
            .iter()
            .map(|agent| match &agent.description {
                Some(description) => format!("- `{}`: {description}", agent.name),
                None => format!("- `{}`", agent.name),
            })
            .collect::<Vec<_>>()
            .join("\n");

        let descriptor = json!({
            "name": "task",
            "summary": "Delegate a job to one of a scope's sub-agents",
            "description": format!(
                "Runs one of the sub-agent roles configured for this scope. The sub-agent \
                 works in its own conversation with its own tools and returns only its final \
                 answer, so the `prompt` must carry everything it needs. Available agents:\n\
                 {catalogue}"
            ),
            "guidelines": [
                "Delegate to `task` when one of the listed agents is written for the job at \
                 hand — they encode a reusable workflow. Give the sub-agent a complete, \
                 self-contained brief: it cannot see this conversation and cannot ask you a \
                 question."
            ],
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "The name of the agent to run",
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task for the sub-agent, complete and self-contained",
                    },
                    "runInBackground": {
                        "type": "boolean",
                        "default": false,
                        "description": "Delegate as a background job and return its id at once, \
                                        instead of waiting for the sub-agent to finish",
                    },
                },
                "required": ["agent", "prompt"],
            },
            "hostValidatesArguments": true,
            "mutating": true,
        });

        serde_json::to_string(&vec![descriptor]).unwrap_or_else(|_| "[]".into())
    }

    async fn execute_tool(name: String, arguments_json: String) -> Result<String, String> {
        if name != "task" {
            return Err(format!("Unknown tool `{name}`"));
        }

        let arguments: Value = serde_json::from_str(&arguments_json)
            .map_err(|_| "Tool arguments must be JSON".to_string())?;
        let requested = arguments
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing required string argument `agent`".to_string())?;
        let prompt = arguments
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing required string argument `prompt`".to_string())?;
        let background = arguments
            .get("runInBackground")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let agents = load_agents().await;
        let agent = agents
            .iter()
            .find(|agent| agent.name == requested)
            .ok_or_else(|| {
                let available = agents
                    .iter()
                    .map(|agent| agent.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Unknown agent `{requested}`. Available: {available}")
            })?;

        let request = json!({
            "name": agent.name,
            "instructions": agent.instructions,
            "prompt": prompt,
            "background": background,
        });
        let response = host::run_agent(request.to_string()).await?;
        let response: Value = serde_json::from_str(&response)
            .map_err(|_| "The nested agent returned malformed output".to_string())?;

        if let Some(job_id) = response.get("jobId").and_then(Value::as_str) {
            return Ok(tool_text(&format!(
                "started background job {job_id}\nThe `{}` agent is running in the \
                 background. Read its answer with job_output(jobId=\"{job_id}\").",
                agent.name
            )));
        }

        match response.get("answer").and_then(Value::as_str) {
            Some(answer) => Ok(tool_text(answer)),
            None => Err("The nested agent returned no answer".into()),
        }
    }

    async fn list_event_handlers() -> String {
        "[]".into()
    }

    async fn handle_event(_handler_id: String, _event_json: String) -> Result<String, String> {
        Err("agents provider has no event handlers".into())
    }

    async fn open_surface(_request_json: String) -> Result<String, String> {
        Err("agents provider has no UI".into())
    }

    async fn handle_action(_action_json: String) -> Result<String, String> {
        Err("agents provider has no UI".into())
    }

    async fn close_surface(_surface_id: String) {}
}

/// One role, as the tool's catalogue and execution need it.
#[derive(Debug)]
struct Agent {
    /// The frontmatter `name`, or the file stem when it is absent.
    name: String,
    /// The one-line summary, when one could be found.
    description: Option<String>,
    /// The role's instructions — the file's body, which becomes the nested
    /// agent's system prompt.
    instructions: String,
}

/// Reads every role below the bound scope's `agents/` directory, sorted by name.
async fn load_agents() -> Vec<Agent> {
    let paths = match host::list_plugin_files(AGENTS_DIR.into()).await {
        Ok(paths) => paths,
        // No agents directory yet is the normal empty case.
        Err(_) => return Vec::new(),
    };

    let mut found: BTreeMap<String, Agent> = BTreeMap::new();
    for path in paths {
        let Some(stem) = agent_stem(&path) else {
            continue;
        };
        let Ok(bytes) = host::read_plugin_file(path.clone()).await else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let Some(agent) = read_one(&text, stem) else {
            continue;
        };
        // A name declared twice keeps the first one found.
        found.entry(agent.name.clone()).or_insert(agent);
    }
    found.into_values().collect()
}

/// The role name when `path` is exactly `agents/<name>.md`.
///
/// Only one level deep: a role's own subdirectories are references, not roles.
/// A leading underscore marks a fragment, and any other extension is not a role.
fn agent_stem(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(AGENTS_DIR)?.strip_prefix('/')?;
    if rest.contains('/') {
        return None;
    }
    let stem = rest.strip_suffix(".md")?;
    if stem.is_empty() || stem.starts_with('_') {
        return None;
    }
    Some(stem)
}

/// Parses one `agents/*.md`, falling back to the file stem for its name.
fn read_one(text: &str, stem: &str) -> Option<Agent> {
    let front = Frontmatter::split(text);

    // The instructions are the whole point of a role; without them there is
    // nothing to hand a nested agent.
    let instructions = front.body(text).trim();
    if instructions.is_empty() {
        return None;
    }

    let name = front
        .get("name")
        .map(str::to_string)
        .unwrap_or_else(|| stem.to_string());
    let description = front
        .get("description")
        .map(|text| cap_chars(text, MAX_DESCRIPTION_CHARS))
        .or_else(|| first_prose_line(instructions));

    Some(Agent {
        name,
        description,
        instructions: instructions.to_string(),
    })
}

/// A tool result, in the shape the host decodes.
fn tool_text(text: &str) -> String {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
    .to_string()
}

/// The first line of prose, skipping headings.
///
/// A role's real files omit the frontmatter, so their first line — "You are the
/// … Agent" — is the only available summary.
fn first_prose_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| cap_chars(line, MAX_DESCRIPTION_CHARS))
}

/// A leading `---` block's scalar fields, with the body that follows it.
///
/// Only `name` and `description` are read, and both are plain scalars, so this
/// is a narrow hand-rolled splitter rather than a YAML parser. Plugins are
/// written by other people on other machines, so a Windows-authored file's CRLF
/// endings, a description containing a colon, a quoted value, and a block whose
/// closing `---` is missing all have to parse the same way here as they did in
/// the host.
struct Frontmatter {
    fields: BTreeMap<String, String>,
    /// Byte offset where the frontmatter block ends and the body begins.
    body_start: usize,
}

impl Frontmatter {
    /// Splits `text` into its leading `---` block. Never fails.
    ///
    /// A file with no block keeps its whole text as the body; a block that is
    /// opened but never closed yields no fields and an empty body, because
    /// guessing where the prose starts would silently swallow a name.
    fn split(text: &str) -> Self {
        let mut rest = text;
        // Leading blank lines are tolerated; anything else before the first
        // `---` means there is no frontmatter.
        loop {
            let Some((line, after)) = next_line(rest) else {
                return Self::no_block();
            };
            if line.trim().is_empty() {
                rest = after;
                continue;
            }
            if line.trim() != "---" {
                return Self::no_block();
            }
            rest = after;
            break;
        }

        let mut fields = BTreeMap::new();
        loop {
            let Some((line, after)) = next_line(rest) else {
                return Self {
                    fields: BTreeMap::new(),
                    body_start: text.len(),
                };
            };
            if line.trim() == "---" {
                let body_start = text.len() - after.len();
                return Self { fields, body_start };
            }
            // Split on the *first* colon: a description routinely contains one,
            // a key never does.
            if let Some((key, value)) = line.split_once(':') {
                let value = unquote(value.trim());
                if !value.is_empty() {
                    fields.insert(key.trim().to_string(), value);
                }
            }
            rest = after;
        }
    }

    /// The whole text is the body.
    fn no_block() -> Self {
        Self {
            fields: BTreeMap::new(),
            body_start: 0,
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    fn body<'a>(&self, text: &'a str) -> &'a str {
        text.get(self.body_start..).unwrap_or("")
    }
}

/// The next line and everything after it, or `None` at the end of the text.
///
/// Splitting on `\n` and trimming the caller's line is what makes a
/// Windows-authored file parse identically to a Unix one.
fn next_line(text: &str) -> Option<(&str, &str)> {
    if text.is_empty() {
        return None;
    }
    match text.find('\n') {
        Some(index) => Some((&text[..index], &text[index + 1..])),
        None => Some((text, "")),
    }
}

/// Removes one layer of matching quotes, if the value is quoted.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

/// Caps text on a character boundary, so a multi-byte character is never split.
fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut capped: String = text.chars().take(max).collect();
    capped.push('…');
    capped
}

export!(AgentsProvider);

#[cfg(test)]
mod tests {
    use super::*;

    /// A role shaped like the real ones: no frontmatter, the first line names
    /// the role, and the body is the whole system prompt. CRLF, as a
    /// Windows-authored file would be.
    const EXAMPLE_ROLE: &str = "You are the Example Agent for this plugin.\r\n\r\nPurpose:\r\n- Translate a request into production-ready code.\r\n";

    #[test]
    fn a_real_role_file_is_named_by_itself_and_its_body_is_the_prompt() {
        let agent = read_one(EXAMPLE_ROLE, "example-agent").expect("the role parses");

        assert_eq!(agent.name, "example-agent");
        assert_eq!(
            agent.description.as_deref(),
            Some("You are the Example Agent for this plugin."),
            "with no frontmatter the first line of prose is the summary"
        );
        assert!(
            agent.instructions.contains("Translate a request"),
            "the body is carried whole, since it becomes the system prompt"
        );
    }

    #[test]
    fn frontmatter_can_name_a_role_and_summarise_it() {
        let agent = read_one(
            "---\nname: implementation\ndescription: Write the code\n---\n\nDo the work.\n",
            "impl",
        )
        .expect("the role parses");

        assert_eq!(agent.name, "implementation");
        assert_eq!(agent.description.as_deref(), Some("Write the code"));
        assert_eq!(agent.instructions, "Do the work.");
    }

    #[test]
    fn a_description_may_contain_a_colon_and_quotes() {
        let agent = read_one(
            "---\nname: a\ndescription: \"Use when: the build fails\"\n---\n\nBody.\n",
            "a",
        )
        .expect("the role parses");

        assert_eq!(
            agent.description.as_deref(),
            Some("Use when: the build fails")
        );
    }

    #[test]
    fn an_unterminated_block_is_treated_as_absent() {
        // Reading the fields anyway would swallow the name, and there is no
        // body to hand a nested agent, so the file is skipped rather than
        // offered as a role with the wrong instructions.
        assert!(read_one("---\nname: a\ndescription: b\n", "dir").is_none());
    }

    #[test]
    fn an_empty_role_file_is_skipped() {
        // Nothing to hand a nested agent, so the name must not reach the
        // catalogue.
        assert!(read_one("   \n\n", "blank").is_none());
        assert!(read_one("---\nname: a\n---\n\n  ", "a").is_none());
    }

    #[test]
    fn only_agents_name_markdown_at_the_top_level() {
        assert_eq!(agent_stem("agents/sample.md"), Some("sample"));
        assert_eq!(
            agent_stem("agents/_fragment.md"),
            None,
            "a fragment is not a role"
        );
        assert_eq!(agent_stem("agents/role.yaml"), None);
        assert_eq!(agent_stem("agents/nested/role.md"), None, "one level only");
        assert_eq!(agent_stem("agents/"), None);
        assert_eq!(agent_stem("other/role.md"), None);
    }

    #[test]
    fn a_long_description_is_capped() {
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 50);
        let agent = read_one(
            &format!("---\nname: verbose\ndescription: {long}\n---\n\nBody.\n"),
            "verbose",
        )
        .expect("the role parses");

        let description = agent.description.as_ref().unwrap();
        assert_eq!(description.chars().count(), MAX_DESCRIPTION_CHARS + 1);
        assert!(description.ends_with('…'));
    }
}
