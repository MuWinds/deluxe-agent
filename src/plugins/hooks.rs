//! `hooks.json`: the commands a plugin runs in response to agent events.
//!
//! Codex fires hooks around its agent loop; this agent supports the one whose
//! intent survives the translation — `PostToolUse`, run after a tool call has
//! finished. The other event names are recognised by their position in the file
//! and deliberately ignored: firing a hook this agent cannot honour *correctly*
//! would be worse than not firing it, because the plugin author would have no
//! way to tell the difference from the outside.
//!
//! # The matcher, and why it needs translating
//!
//! A group's `matcher` names the tools it fires for, as a regular expression —
//! the format's own example is `Write|Edit`. Those are *Codex's* tool names, and
//! this agent spells the same operations differently: it has `apply_patch`, not
//! `Write` and `Edit`; `read_file`, not `Read`; `exec`, not `Bash`. Matching the
//! pattern literally against this agent's tool names would leave every hook in
//! every real plugin dead code, so a match is tried against the tool's own name
//! and against the Codex names for the same operation — see [`aliases`]. That
//! translation is the whole reason this module exists: without it, `PostToolUse`
//! support would be a feature that never runs.
//!
//! # What is deliberately not done
//!
//! Nothing about a hook's *contract* beyond "run this command after that tool".
//! Codex lets a hook feed JSON back to influence the loop; the only hooks in the
//! real install are drafts that print a reminder, and inventing a richer
//! protocol than the plugins use would be guessing. The command's output is
//! appended to the tool result, which is what the drafts ask for — see
//! [`crate::agent`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

/// The one event this agent acts on.
const POST_TOOL_USE: &str = "PostToolUse";

/// A plugin's hooks, at its root. There is no manifest field pointing at it, for
/// the same reason `.mcp.json` has none: the name is the interface.
const HOOKS_FILE: &str = "hooks.json";

/// One `PostToolUse` hook, ready to run.
#[derive(Debug, Clone)]
pub struct Hook {
    /// The matcher, compiled once at load. `None` means "every tool", which is
    /// how the format spells an omitted or empty `matcher`.
    matcher: Option<Regex>,
    /// The pattern as written, for logs and the transcript.
    pub pattern: String,
    /// The command to run, as the plugin wrote it — a relative path is resolved
    /// against [`Hook::root`] by the shell that runs it.
    pub command: String,
    /// The plugin this came from, `name@marketplace`.
    pub plugin: String,
    /// The plugin's root: the command's working directory, so a hook's
    /// `./scripts/check.sh` resolves the way its author meant.
    pub root: PathBuf,
}

impl Hook {
    /// Whether this hook fires for a tool call.
    ///
    /// Matched against the tool's own name and against the Codex names for the
    /// same operation, because a plugin was written for Codex and names the
    /// operation the Codex way.
    pub fn matches(&self, tool: &str) -> bool {
        let Some(matcher) = &self.matcher else {
            return true;
        };
        std::iter::once(tool)
            .chain(aliases(tool).iter().copied())
            .any(|name| matcher.is_match(name))
    }
}

/// Codex's names for the operations this agent spells differently.
///
/// The left-hand names are this agent's tools; the strings are the tool names
/// Codex exposes for the same job. A matcher written for Codex names one of
/// those, so a match is tried against each in turn. Only the tools whose Codex
/// counterpart has a different spelling appear here — a name the two agree on
/// needs no entry, since the tool's own name is always tried first.
fn aliases(tool: &str) -> &'static [&'static str] {
    match tool {
        "apply_patch" => &["Write", "Edit", "MultiEdit", "NotebookEdit"],
        "read_file" => &["Read", "NotebookRead"],
        "exec" => &["Bash", "Shell"],
        "list_dir" => &["LS", "Glob", "Grep"],
        _ => &[],
    }
}

/// `hooks.json`, of which only the `PostToolUse` list is read.
///
/// A map keyed by event name rather than a struct, because the set of events is
/// Codex's to grow and an event this agent has never heard of must be ignored
/// rather than fail the parse.
#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: BTreeMap<String, Vec<HookGroup>>,
}

/// One matcher and the commands it runs.
#[derive(Debug, Deserialize)]
struct HookGroup {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<HookEntry>,
}

/// One command within a group.
#[derive(Debug, Deserialize)]
struct HookEntry {
    /// The discriminator. Only `command` is something this agent can run; a
    /// kind it has not seen is skipped on its own.
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

/// Reads every `PostToolUse` hook a plugin declares.
///
/// `plugin` is the `name@marketplace` id, which is what the transcript and the
/// log lines name. Never fails: a missing file is the common case, and a file
/// that will not parse, a matcher that will not compile, or an entry of an
/// unknown kind each costs only itself — the same rule discovery follows
/// everywhere else, so one bad hook cannot take a plugin's skills down with it.
pub fn load(plugin: &str, root: &Path) -> Vec<Hook> {
    let path = root.join(HOOKS_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };

    let file: HooksFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(
                plugin,
                path = %path.display(),
                %error,
                "skipping a plugin's unreadable hooks.json"
            );
            return Vec::new();
        }
    };

    let mut hooks = Vec::new();
    for (event, groups) in &file.hooks {
        if event != POST_TOOL_USE {
            tracing::debug!(
                plugin,
                event,
                "ignoring a hook event this agent does not support"
            );
            continue;
        }

        for group in groups {
            let pattern = group.matcher.as_deref().unwrap_or("").trim().to_string();
            let matcher = match compile(&pattern) {
                Ok(matcher) => matcher,
                Err(error) => {
                    tracing::warn!(
                        plugin,
                        pattern,
                        %error,
                        "skipping a hook group with an invalid matcher"
                    );
                    continue;
                }
            };

            for entry in &group.hooks {
                if entry.kind.as_deref() != Some("command") {
                    tracing::debug!(
                        plugin,
                        kind = entry.kind.as_deref().unwrap_or(""),
                        "ignoring a hook that is not a command"
                    );
                    continue;
                }
                let Some(command) = entry
                    .command
                    .as_deref()
                    .map(str::trim)
                    .filter(|command| !command.is_empty())
                else {
                    continue;
                };

                hooks.push(Hook {
                    matcher: matcher.clone(),
                    pattern: pattern.clone(),
                    command: command.to_string(),
                    plugin: plugin.to_string(),
                    root: root.to_path_buf(),
                });
            }
        }
    }

    hooks
}

/// Compiles a matcher, where an empty pattern means "every tool".
///
/// An empty `matcher` is not a pattern that matches nothing — it is the format's
/// way of saying the group has no filter, and reading it the other way would
/// silently disable the hook.
fn compile(pattern: &str) -> Result<Option<Regex>, regex::Error> {
    if pattern.is_empty() {
        return Ok(None);
    }
    Regex::new(pattern).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real `figma` plugin's `hooks.json`, verbatim.
    const FIGMA: &str = r#"{
      "hooks": {
        "PostToolUse": [
          {
            "matcher": "Write|Edit",
            "hooks": [
              {
                "type": "command",
                "command": "./scripts/post_write_figma_parity_check.sh"
              }
            ]
          }
        ]
      }
    }"#;

    fn write_hooks(root: &Path, body: &str) {
        std::fs::write(root.join(HOOKS_FILE), body).unwrap();
    }

    #[test]
    fn a_real_hooks_file_parses() {
        let temp = tempfile::tempdir().unwrap();
        write_hooks(temp.path(), FIGMA);

        let hooks = load("figma@openai-curated", temp.path());

        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].pattern, "Write|Edit");
        assert_eq!(
            hooks[0].command,
            "./scripts/post_write_figma_parity_check.sh"
        );
        assert_eq!(hooks[0].plugin, "figma@openai-curated");
        assert_eq!(
            hooks[0].root,
            temp.path(),
            "the command runs from the plugin root"
        );
    }

    #[test]
    fn a_codex_matcher_fires_for_this_agents_equivalent_tool() {
        // The load-bearing test for the alias table. `figma` says `Write|Edit`;
        // this agent edits with `apply_patch`. Without the translation the hook
        // would never fire, and the feature would be dead code.
        let temp = tempfile::tempdir().unwrap();
        write_hooks(temp.path(), FIGMA);
        let hook = &load("figma@openai-curated", temp.path())[0];

        assert!(
            hook.matches("apply_patch"),
            "Write|Edit must reach apply_patch"
        );
        assert!(
            !hook.matches("read_file"),
            "a write hook must not fire on a read"
        );
        assert!(!hook.matches("exec"));
    }

    #[test]
    fn the_alias_table_covers_each_translated_tool() {
        let hook = |pattern: &str| Hook {
            matcher: compile(pattern).unwrap(),
            pattern: pattern.into(),
            command: "x".into(),
            plugin: "p@m".into(),
            root: PathBuf::from("/p"),
        };

        assert!(hook("Read").matches("read_file"));
        assert!(hook("Bash").matches("exec"));
        assert!(hook("Glob").matches("list_dir"));
        // A name the two agree on needs no alias.
        assert!(hook("exec").matches("exec"));
        // And a Codex name must not leak onto an unrelated tool.
        assert!(!hook("Read").matches("apply_patch"));
    }

    #[test]
    fn an_empty_matcher_matches_every_tool() {
        // `""` is the format's "no filter", not "match nothing" — reading it the
        // other way would silently disable the hook.
        let temp = tempfile::tempdir().unwrap();
        write_hooks(
            temp.path(),
            r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"./always.sh"}]}]}}"#,
        );

        let hook = &load("p@m", temp.path())[0];
        assert_eq!(hook.pattern, "");
        for tool in ["apply_patch", "read_file", "exec", "anything"] {
            assert!(hook.matches(tool), "an absent matcher means every tool");
        }
    }

    #[test]
    fn a_matcher_is_a_regular_expression() {
        // The format documents the matcher as a regex, so `.*` and anchors work
        // rather than being read as literal text.
        let temp = tempfile::tempdir().unwrap();
        write_hooks(
            temp.path(),
            r#"{"hooks":{"PostToolUse":[
                 {"matcher":"^exec$","hooks":[{"type":"command","command":"./a.sh"}]}
               ]}}"#,
        );

        let hook = &load("p@m", temp.path())[0];
        assert!(hook.matches("exec"));
        assert!(!hook.matches("apply_patch"));
    }

    #[test]
    fn an_event_this_agent_does_not_support_is_ignored() {
        // `PreToolUse` is real in the format and meaningless here. It must be
        // dropped without taking the `PostToolUse` group beside it down.
        let temp = tempfile::tempdir().unwrap();
        write_hooks(
            temp.path(),
            r#"{"hooks":{
                 "PreToolUse":[{"matcher":".*","hooks":[{"type":"command","command":"./before.sh"}]}],
                 "PostToolUse":[{"matcher":".*","hooks":[{"type":"command","command":"./after.sh"}]}]
               }}"#,
        );

        let hooks = load("p@m", temp.path());
        assert_eq!(hooks.len(), 1, "only PostToolUse is honoured");
        assert_eq!(hooks[0].command, "./after.sh");
    }

    #[test]
    fn a_hook_that_is_not_a_command_is_skipped() {
        // A future hook kind must be dropped on its own, not treated as a
        // malformed file.
        let temp = tempfile::tempdir().unwrap();
        write_hooks(
            temp.path(),
            r#"{"hooks":{"PostToolUse":[{"matcher":".*","hooks":[
                 {"type":"prompt","command":"ignore me"},
                 {"type":"command","command":"./real.sh"}
               ]}]}}"#,
        );

        let hooks = load("p@m", temp.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "./real.sh");
    }

    #[test]
    fn a_broken_matcher_costs_only_its_own_group() {
        let temp = tempfile::tempdir().unwrap();
        write_hooks(
            temp.path(),
            r#"{"hooks":{"PostToolUse":[
                 {"matcher":"[unclosed","hooks":[{"type":"command","command":"./bad.sh"}]},
                 {"matcher":".*","hooks":[{"type":"command","command":"./good.sh"}]}
               ]}}"#,
        );

        let hooks = load("p@m", temp.path());
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "./good.sh");
    }

    #[test]
    fn several_commands_in_one_group_all_load() {
        let temp = tempfile::tempdir().unwrap();
        write_hooks(
            temp.path(),
            r#"{"hooks":{"PostToolUse":[{"matcher":"Edit","hooks":[
                 {"type":"command","command":"./one.sh"},
                 {"type":"command","command":"./two.sh"}
               ]}]}}"#,
        );

        let hooks = load("p@m", temp.path());
        let commands: Vec<&str> = hooks.iter().map(|hook| hook.command.as_str()).collect();
        assert_eq!(commands, vec!["./one.sh", "./two.sh"]);
    }

    #[test]
    fn a_missing_or_broken_file_yields_no_hooks() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            load("p@m", temp.path()).is_empty(),
            "most plugins ship no hooks"
        );

        write_hooks(temp.path(), "{ not json");
        assert!(
            load("p@m", temp.path()).is_empty(),
            "a broken file must not panic"
        );
    }
}
