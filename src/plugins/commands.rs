//! `commands/*.md`: the slash commands a plugin contributes.
//!
//! A command is a Markdown file whose body becomes the prompt when the user
//! types `/name`. Unlike a skill — which the model reaches for on its own — a
//! command is something the *user* invokes, so nothing here touches the system
//! prompt: the expansion happens in the composer, before the turn exists.
//!
//! # Three things the real files forced
//!
//! * Two shapes, both in use. `figma`'s commands have no frontmatter and are
//!   named by their filename; `boss-skill`'s carry `name:` (sometimes already
//!   namespaced, as `boss:plan`) and `vercel`'s carry only a `description`.
//!   `cloudflare`'s carry `argument-hint` and `allowed-tools` as well.
//! * Not every `.md` in the directory is a command. `vercel/commands/` holds
//!   `_conventions.md` — authoring guidance the other commands follow — and each
//!   command beside a `.md.tmpl` source. A plain `*.md` glob would invent a
//!   `_conventions` command and count every command twice.
//! * `$ARGUMENTS` is real, and a command that lacks it still has to receive
//!   what the user typed, or `/figma:implement-from-figma <url>` would silently
//!   lose the url.
//!
//! # What is deliberately not done
//!
//! `allowed-tools` is parsed but not enforced. Honouring it would mean narrowing
//! the tool registry for the duration of one turn, and this agent has no
//! approval step to hang that on — a command's own prose already says what it
//! intends, and the tools it names are the ones the model would reach for
//! anyway.

use std::collections::BTreeMap;
use std::path::Path;

use super::{cap_chars, first_prose_line, frontmatter, MAX_DESCRIPTION_CHARS};

/// One command, as the composer and the expansion need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// What the user types after `/`.
    ///
    /// Namespaced by plugin — `cloudflare:build-agent` — because that is the
    /// convention the plugins document for themselves, and because two plugins
    /// offering a `deploy` have to stay apart. A frontmatter `name` overrides
    /// it, since an author who names a command has made a deliberate choice.
    pub name: String,
    /// The one-line summary, when one could be found.
    pub description: Option<String>,
    /// What replaces the invocation in the prompt.
    pub template: String,
    /// The plugin this came from, by its short name.
    pub plugin: String,
}

/// The directory a plugin's commands live in, relative to its root.
///
/// There is no manifest field for this: `figma`'s `plugin.json` declares
/// `skills` and `apps` and leaves `commands` unset, and no plugin in the real
/// install declares one. The directory is the interface.
const COMMANDS_DIR: &str = "commands";

/// Reads every command a plugin declares.
///
/// `plugin` is the plugin's short name, which is what namespaces the command.
/// Sorted by name, so the composer's list is stable across runs.
pub fn load(plugin: &str, root: &Path) -> Vec<Command> {
    let mut found: BTreeMap<String, Command> = BTreeMap::new();

    let Ok(entries) = std::fs::read_dir(root.join(COMMANDS_DIR)) else {
        return Vec::new();
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Only `.md`. A `.md.tmpl` beside its rendered `.md` is the author's
        // source, not a second command.
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // A leading underscore marks a fragment the other commands follow.
        if stem.starts_with('_') {
            continue;
        }

        if let Some(command) = read_one(plugin, &path, stem) {
            // Two files cannot share a name within one plugin, but the
            // frontmatter is free to name them the same; the first one read
            // wins, which is the rule skills already follow.
            found.entry(command.name.clone()).or_insert(command);
        }
    }

    found.into_values().collect()
}

/// Parses one `commands/*.md`, falling back to the filename stem for its name.
fn read_one(plugin: &str, path: &Path, stem: &str) -> Option<Command> {
    let text = std::fs::read_to_string(path).ok()?;
    let front = frontmatter::split(&text);
    let body = front.body();

    // A body is the whole point of a command; without one there is nothing to
    // send, and registering it would put a name in the composer that does
    // nothing when typed.
    let template = body.trim();
    if template.is_empty() {
        return None;
    }

    let name = match front.get("name") {
        Some(name) => name.to_string(),
        None => format!("{plugin}:{stem}"),
    };

    let description = front
        .get("description")
        .map(|text| cap_chars(text, MAX_DESCRIPTION_CHARS))
        .or_else(|| first_prose_line(body));

    Some(Command {
        name,
        description,
        template: template.to_string(),
        plugin: plugin.to_string(),
    })
}

/// Expands a leading `/name` into that command's template.
///
/// `None` when the text is not a command invocation, which is the common case
/// and includes a leading `/` that is merely a path — `/usr/bin/env` must reach
/// the model as typed, not be reported as an unknown command.
///
/// The arguments are substituted for `$ARGUMENTS`, or appended when the
/// template has no such placeholder.
pub fn expand(text: &str, commands: &[&Command]) -> Option<String> {
    let rest = text.trim_start().strip_prefix('/')?;

    // The name runs to the first whitespace. A `:` in it is expected —
    // `boss:plan` is a real name — and is not a separator.
    let (name, arguments) = match rest.split_once(char::is_whitespace) {
        Some((name, arguments)) => (name, arguments.trim()),
        None => (rest, ""),
    };
    if name.is_empty() {
        return None;
    }

    let command = commands.iter().find(|command| command.name == name)?;
    Some(substitute(&command.template, arguments))
}

/// Puts the user's arguments into a command's template.
fn substitute(template: &str, arguments: &str) -> String {
    if arguments.is_empty() {
        // A placeholder left behind would reach the model as a literal
        // `$ARGUMENTS`. A command that quotes it — `"$ARGUMENTS"` — then reads
        // as an empty string, which is the honest answer.
        return template.replace("$ARGUMENTS", "");
    }

    if template.contains("$ARGUMENTS") {
        return template.replace("$ARGUMENTS", arguments);
    }

    // Most commands have no placeholder — `figma`'s four take their url through
    // prose alone — so the arguments are appended rather than dropped.
    format!("{template}\n\n{arguments}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// `figma`'s shape: no frontmatter, named by the file, and no `$ARGUMENTS`
    /// placeholder even though the body takes arguments.
    const FIGMA: &str = "# /connect-figma-components\n\nCreate or update parserless Figma Code Connect template files for components.\n\n## Arguments\n\n- `figma_url`: Figma URL with `node-id`\n";

    /// `cloudflare`'s shape: a description, an argument hint, and a
    /// `$ARGUMENTS` placeholder in the body.
    const CLOUDFLARE: &str = "---\ndescription: Build an AI agent on Cloudflare using the Agents SDK\nargument-hint: [agent-description]\nallowed-tools: [Read, Glob, Grep, Bash]\n---\n\n# Build AI Agent on Cloudflare\n\n## Arguments\n\nThe user invoked this command with: $ARGUMENTS\n";

    fn write_command(root: &Path, file: &str, contents: &str) {
        let dir = root.join(COMMANDS_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(file), contents).unwrap();
    }

    #[test]
    fn a_command_without_frontmatter_is_named_by_its_file() {
        let temp = tempfile::tempdir().unwrap();
        write_command(temp.path(), "connect-figma-components.md", FIGMA);

        let commands = load("figma", temp.path());

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "figma:connect-figma-components");
        assert_eq!(
            commands[0].description.as_deref(),
            Some("Create or update parserless Figma Code Connect template files for components."),
            "with no frontmatter the first line of prose is the summary"
        );
        assert!(commands[0]
            .template
            .starts_with("# /connect-figma-components"));
    }

    #[test]
    fn frontmatter_supplies_the_description_and_the_body_is_the_template() {
        let temp = tempfile::tempdir().unwrap();
        write_command(temp.path(), "build-agent.md", CLOUDFLARE);

        let commands = load("cloudflare", temp.path());

        assert_eq!(commands[0].name, "cloudflare:build-agent");
        assert_eq!(
            commands[0].description.as_deref(),
            Some("Build an AI agent on Cloudflare using the Agents SDK")
        );
        assert!(
            !commands[0].template.starts_with("---"),
            "the frontmatter is not part of the prompt: {}",
            commands[0].template
        );
        assert!(commands[0].template.contains("$ARGUMENTS"));
    }

    #[test]
    fn a_frontmatter_name_wins_over_the_filename() {
        // `boss-skill` ships `boss-plan.md` whose frontmatter says
        // `name: boss:plan` — the author's name is the one that is used.
        let temp = tempfile::tempdir().unwrap();
        write_command(
            temp.path(),
            "boss-plan.md",
            "---\nname: boss:plan\ndescription: 只跑规划环节\n---\n\n# /boss:plan\n\nBody.\n",
        );

        let commands = load("boss-skill", temp.path());

        assert_eq!(commands[0].name, "boss:plan");
        assert_eq!(commands[0].description.as_deref(), Some("只跑规划环节"));
    }

    #[test]
    fn a_fragment_is_not_a_command() {
        // `vercel/commands/_conventions.md` is authoring guidance for the
        // other commands, not something to type.
        let temp = tempfile::tempdir().unwrap();
        write_command(
            temp.path(),
            "_conventions.md",
            "# Command Conventions\n\nEvery command follows this structure.\n",
        );
        write_command(
            temp.path(),
            "deploy.md",
            "---\ndescription: Deploy\n---\n\nBody.\n",
        );

        let commands = load("vercel", temp.path());

        assert_eq!(
            commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["vercel:deploy"],
            "the underscore-prefixed file is skipped"
        );
    }

    #[test]
    fn a_template_source_beside_its_command_is_not_a_second_command() {
        // `vercel/commands/` really does hold `deploy.md` and `deploy.md.tmpl`.
        let temp = tempfile::tempdir().unwrap();
        write_command(
            temp.path(),
            "deploy.md",
            "---\ndescription: Deploy\n---\n\nBody.\n",
        );
        write_command(
            temp.path(),
            "deploy.md.tmpl",
            "---\ndescription: Deploy\n---\n\nBody.\n",
        );

        let commands = load("vercel", temp.path());

        assert_eq!(commands.len(), 1, "only the rendered `.md` is a command");
    }

    #[test]
    fn a_command_with_no_body_is_not_registered() {
        // Nothing to send, so a name in the composer would do nothing.
        let temp = tempfile::tempdir().unwrap();
        write_command(temp.path(), "empty.md", "---\ndescription: nothing\n---\n");
        write_command(
            temp.path(),
            "real.md",
            "---\ndescription: real\n---\n\nBody.\n",
        );

        let commands = load("p", temp.path());

        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "p:real");
    }

    #[test]
    fn a_plugin_with_no_commands_directory_yields_nothing() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load("p", temp.path()).is_empty());
    }

    #[test]
    fn commands_are_sorted_by_name() {
        let temp = tempfile::tempdir().unwrap();
        write_command(
            temp.path(),
            "zebra.md",
            "---\ndescription: z\n---\n\nBody.\n",
        );
        write_command(
            temp.path(),
            "alpha.md",
            "---\ndescription: a\n---\n\nBody.\n",
        );

        let commands = load("p", temp.path());

        assert_eq!(
            commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["p:alpha", "p:zebra"]
        );
    }

    /// One command to expand against.
    fn one(template: &str) -> Vec<Command> {
        vec![Command {
            name: "p:do".to_string(),
            description: None,
            template: template.to_string(),
            plugin: "p".to_string(),
        }]
    }

    #[test]
    fn arguments_replace_the_placeholder() {
        let commands = one("The user invoked this command with: $ARGUMENTS\n");
        let refs = commands.iter().collect::<Vec<_>>();

        let expanded = expand("/p:do fix the build", &refs).unwrap();

        assert_eq!(
            expanded,
            "The user invoked this command with: fix the build\n"
        );
    }

    #[test]
    fn a_quoted_placeholder_is_substituted_too() {
        // `vercel/deploy.md` writes `"$ARGUMENTS"`, quotes and all.
        let commands = one("If \"$ARGUMENTS\" contains \"prod\":\n");
        let refs = commands.iter().collect::<Vec<_>>();

        assert_eq!(
            expand("/p:do prod", &refs).unwrap(),
            "If \"prod\" contains \"prod\":\n"
        );
    }

    #[test]
    fn a_command_without_a_placeholder_still_receives_the_arguments() {
        // `figma`'s commands take a url through prose alone; dropping it would
        // make the command useless.
        let commands = one("# /do\n\nImplement the design.\n");
        let refs = commands.iter().collect::<Vec<_>>();

        let expanded = expand("/p:do https://figma.com/x?node-id=1", &refs).unwrap();

        assert!(
            expanded.starts_with("# /do\n\nImplement the design."),
            "{expanded}"
        );
        assert!(
            expanded.contains("https://figma.com/x?node-id=1"),
            "the url reaches the model: {expanded}"
        );
    }

    #[test]
    fn no_arguments_leaves_no_placeholder_behind() {
        let commands = one("With: $ARGUMENTS\n");
        let refs = commands.iter().collect::<Vec<_>>();

        let expanded = expand("/p:do", &refs).unwrap();

        assert_eq!(expanded, "With: \n");
        assert!(
            !expanded.contains("$ARGUMENTS"),
            "a literal placeholder would reach the model"
        );
    }

    #[test]
    fn surrounding_whitespace_in_the_invocation_is_trimmed() {
        let commands = one("Args: [$ARGUMENTS]\n");
        let refs = commands.iter().collect::<Vec<_>>();

        let expanded = expand("   /p:do   spaced out   ", &refs).unwrap();

        assert_eq!(expanded, "Args: [spaced out]\n");
    }

    #[test]
    fn a_path_is_not_an_unknown_command() {
        // The whole reason `expand` returns `Option`: `/usr/bin/env` is text,
        // not a failed lookup.
        let commands = one("Body.\n");
        let refs = commands.iter().collect::<Vec<_>>();

        assert_eq!(expand("/usr/bin/env python", &refs), None);
        assert_eq!(expand("/p:unknown", &refs), None);
        assert_eq!(expand("no leading slash", &refs), None);
        assert_eq!(expand("/", &refs), None);
    }

    #[test]
    fn the_longest_matching_name_wins_nothing_but_the_exact_one_matches() {
        // `p:do` must not match `p:do-more`, which a prefix search would.
        let commands = one("Body.\n");
        let refs = commands.iter().collect::<Vec<_>>();

        assert!(expand("/p:do", &refs).is_some());
        assert_eq!(expand("/p:do-more", &refs), None);
    }

    #[test]
    fn a_colon_in_the_name_is_part_of_the_name() {
        let commands = [Command {
            name: "boss:plan".to_string(),
            description: None,
            template: "Plan: $ARGUMENTS\n".to_string(),
            plugin: "boss-skill".to_string(),
        }];
        let refs = commands.iter().collect::<Vec<_>>();

        assert_eq!(
            expand("/boss:plan the thing", &refs).unwrap(),
            "Plan: the thing\n"
        );
    }

    #[test]
    fn the_real_figma_commands_parse_from_this_machines_install() {
        // Not a fixture: the actual files, so the shapes the tests above
        // encode are the shapes that ship. Skipped where figma is absent, so
        // the suite still passes on a machine that has never installed it.
        let root = directories::UserDirs::new()
            .map(|dirs| {
                crate::plugins::plugin_cache_root(dirs.home_dir()).join("openai-curated/figma")
            })
            .filter(|path| path.is_dir());
        let Some(root) = root else {
            return;
        };
        let Some(version) = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.is_dir())
        else {
            return;
        };

        let commands = load("figma", &version);

        let names = commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "figma:connect-figma-components",
                "figma:create-design-system-rules",
                "figma:implement-from-figma",
                "figma:review-design-parity",
            ],
            "the four commands figma ships, sorted"
        );
        assert!(
            commands.iter().all(|c| c.description.is_some()),
            "each has prose to summarise it: {commands:#?}"
        );
    }
}
