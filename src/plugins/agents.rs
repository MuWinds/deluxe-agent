//! `agents/*.md`: the named sub-agent roles a plugin contributes.
//!
//! A file's whole body is the role's system prompt — the real files carry no
//! frontmatter — and the role's name is the file stem. Codex lists these roles
//! and lets the model delegate a job to one; this agent does the same through
//! the `task` tool, which runs the named role in a sub-conversation and returns
//! its answer. See [`crate::tools::task`].
//!
//! # Not every file in the directory is a role
//!
//! `figma/agents/` holds four `.md` roles beside an `openai.yaml`, which is the
//! plugin's *interface* metadata — display name, icons, default prompt — and is
//! not an agent at all. A scan that took every file would invent a role named
//! `openai.yaml` and offer it to the model, so only `.md` is read.
//!
//! # Why the summary is the first line of prose
//!
//! The real role files have no frontmatter, so there is nowhere else to get a
//! one-line description, and the catalogue the model chooses from wants one.
//! Their first line — "You are the … Agent for this plugin" — names the role
//! and reads as a summary, which is the same fallback [`super::commands`] uses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{cap_chars, first_prose_line, frontmatter, MAX_DESCRIPTION_CHARS};

/// One role, as the catalogue and the `task` tool need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRole {
    /// What the model names when it delegates: the file stem, unless the
    /// frontmatter overrides it.
    pub name: String,
    /// The one-line summary for the catalogue, when one could be found.
    pub description: Option<String>,
    /// The role's instructions — the file's body, which becomes the sub-agent's
    /// system prompt.
    pub instructions: String,
    /// The plugin this came from, as `name@marketplace`.
    pub plugin: String,
    /// Absolute path to the `.md`, which the transcript names.
    pub path: PathBuf,
}

/// The directory a plugin's roles live in, relative to its root.
///
/// Like `commands/` and `.mcp.json`, there is no manifest field pointing at it:
/// the directory is the interface.
const AGENTS_DIR: &str = "agents";

/// Reads every role a plugin declares, sorted by name.
///
/// Sorted so the `task` tool's description — which lists them — is stable across
/// runs, the same property the skill catalogue relies on.
pub fn load(plugin: &str, root: &Path) -> Vec<AgentRole> {
    let mut found: BTreeMap<String, AgentRole> = BTreeMap::new();

    let Ok(entries) = std::fs::read_dir(root.join(AGENTS_DIR)) else {
        return Vec::new();
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Only `.md`. `openai.yaml` is interface metadata, not a role.
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // A leading underscore marks a fragment, as it does for commands.
        if stem.starts_with('_') {
            continue;
        }

        if let Some(role) = read_one(plugin, &path, stem) {
            found.entry(role.name.clone()).or_insert(role);
        }
    }

    found.into_values().collect()
}

/// Parses one `agents/*.md`, falling back to the filename stem for its name.
fn read_one(plugin: &str, path: &Path, stem: &str) -> Option<AgentRole> {
    let text = std::fs::read_to_string(path).ok()?;
    let front = frontmatter::split(&text);

    // The instructions are the whole point of a role; without them there is
    // nothing to hand a sub-agent, and offering the name would be a promise the
    // `task` tool could not keep.
    let instructions = front.body().trim();
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

    Some(AgentRole {
        name,
        description,
        instructions: instructions.to_string(),
        plugin: plugin.to_string(),
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// `figma`'s shape: no frontmatter, the first line names the role, and the
    /// body is the whole system prompt.
    const FIGMA: &str = "You are the Figma Implementation Agent for this plugin.\n\n\
        Purpose:\n\
        - Translate a Figma node into production-ready code with strong visual parity.\n\n\
        Rules:\n\
        - Always get `get_design_context` and `get_screenshot` before implementation.\n";

    fn write_agent(root: &Path, file: &str, contents: &str) {
        let dir = root.join(AGENTS_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(file), contents).unwrap();
    }

    #[test]
    fn a_real_agent_file_is_named_by_itself_and_its_body_is_the_prompt() {
        let temp = tempfile::tempdir().unwrap();
        write_agent(temp.path(), "figma-implementation-agent.md", FIGMA);

        let roles = load("figma@openai-curated", temp.path());

        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].name, "figma-implementation-agent");
        assert_eq!(roles[0].plugin, "figma@openai-curated");
        assert_eq!(
            roles[0].description.as_deref(),
            Some("You are the Figma Implementation Agent for this plugin."),
            "with no frontmatter the first line of prose is the summary"
        );
        assert!(
            roles[0]
                .instructions
                .contains("Always get `get_design_context`"),
            "the body is carried whole, since it becomes the system prompt"
        );
        assert!(roles[0].path.is_absolute() || roles[0].path.is_file());
    }

    #[test]
    fn the_interface_metadata_is_not_a_role() {
        // The trap the real directory sets: `openai.yaml` sits beside the roles
        // and is not one. Offering it to the model would be a role that cannot
        // run.
        let temp = tempfile::tempdir().unwrap();
        write_agent(temp.path(), "figma-implementation-agent.md", FIGMA);
        fs::write(
            temp.path().join(AGENTS_DIR).join("openai.yaml"),
            "interface:\n  display_name: \"Figma\"\n",
        )
        .unwrap();

        let roles = load("figma@openai-curated", temp.path());

        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].name, "figma-implementation-agent");
    }

    #[test]
    fn roles_are_sorted_so_the_catalogue_is_stable() {
        let temp = tempfile::tempdir().unwrap();
        write_agent(temp.path(), "zeta.md", "You are Zeta.\n");
        write_agent(temp.path(), "alpha.md", "You are Alpha.\n");

        let roles = load("p@m", temp.path());
        let names: Vec<&str> = roles.iter().map(|role| role.name.as_str()).collect();

        assert_eq!(names, vec!["alpha", "zeta"]);
    }

    #[test]
    fn frontmatter_can_name_a_role_and_summarise_it() {
        let temp = tempfile::tempdir().unwrap();
        write_agent(
            temp.path(),
            "impl.md",
            "---\nname: implementation\ndescription: Write the code\n---\n\nDo the work.\n",
        );

        let roles = load("p@m", temp.path());

        assert_eq!(roles[0].name, "implementation");
        assert_eq!(roles[0].description.as_deref(), Some("Write the code"));
        assert_eq!(roles[0].instructions, "Do the work.");
    }

    #[test]
    fn an_empty_role_file_is_skipped() {
        // Nothing to hand a sub-agent, so the name must not reach the catalogue.
        let temp = tempfile::tempdir().unwrap();
        write_agent(temp.path(), "blank.md", "   \n\n");
        write_agent(temp.path(), "real.md", "You are Real.\n");

        let roles = load("p@m", temp.path());

        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].name, "real");
    }

    #[test]
    fn a_plugin_with_no_agents_directory_yields_nothing() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load("p@m", temp.path()).is_empty());
    }
}
