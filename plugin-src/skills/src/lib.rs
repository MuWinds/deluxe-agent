//! The skills provider Component.
//!
//! Reads the skills a configuration scope holds and renders them into the
//! catalog the host appends verbatim to the system prompt. It owns the whole
//! layout — which directory holds skills, which file marks one, how the
//! frontmatter is shaped, and how the catalog is worded — so the host never
//! learns any of it: the host only binds the instance to a scope root and
//! appends what comes back.
//!
//! The instance is bound to one root (`~/.deluxe-agents` for the global scope,
//! `<project>/.deluxe-agents` for a project's), and always looks in `skills/`
//! below it. A missing directory contributes nothing, not a failure.
//!
//! It also implements the harness `plugin` interface, inertly, so the plugin
//! platform can load it as an ordinary Component.

use std::collections::BTreeMap;

wit_bindgen::generate!({
    path: "../../wit",
    world: "prompt-provider",
    async: true,
});

use deluxe::harness::host;
use exports::deluxe::harness::plugin::Guest as PluginGuest;
use exports::deluxe::harness::prompt::Guest as PromptGuest;

/// The directory a scope's skills live in, relative to the scope root.
const SKILLS_DIR: &str = "skills";
/// The file that marks a directory as a skill.
const SKILL_FILE: &str = "SKILL.md";
/// A summary is capped at this many characters, so one long description cannot
/// crowd the prompt's skill list.
const MAX_DESCRIPTION_CHARS: usize = 200;

struct SkillsProvider;

impl PromptGuest for SkillsProvider {
    async fn prompt_sections() -> Result<String, String> {
        let paths = match host::list_plugin_files(SKILLS_DIR.into()).await {
            Ok(paths) => paths,
            // No skills directory yet is the normal empty case.
            Err(error) if error.starts_with("plugin_file_not_found:") => return Ok(String::new()),
            Err(error) => return Err(error),
        };

        let mut found: BTreeMap<String, Skill> = BTreeMap::new();
        for path in paths {
            let Some(directory) = skill_directory(&path) else {
                continue;
            };
            let bytes = host::read_plugin_file(path.clone()).await?;
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            let skill = read_one(&text, &path, directory);
            // A name declared twice keeps the first one found.
            found.entry(skill.name.clone()).or_insert(skill);
        }

        if found.is_empty() {
            return Ok(String::new());
        }

        // The prompt tells the model to read each `SKILL.md`, so it needs an
        // absolute path. The bound root is one; joining is the guest's job
        // because only it knows the layout below it.
        let root = host::configuration_root().await;
        let root = root.replace('\\', "/");
        let root = root.trim_end_matches('/');

        let mut section = String::from(
            "<skills>\n\
             Reusable, task-specific instructions available globally and in this \
             project. This catalog lists only each skill's name and summary; do not \
             act on a skill from its summary alone. When the user names a skill or \
             the task clearly matches one, read its SKILL.md in full with `read_file` \
             before taking task actions, then follow it. Resolve any relative path a \
             skill mentions against its SKILL.md directory.\n",
        );
        for skill in found.into_values() {
            section.push_str("- `");
            section.push_str(&skill.name);
            section.push('`');
            if let Some(description) = &skill.description {
                section.push_str(": ");
                section.push_str(description);
            }
            section.push_str("\n  SKILL.md: ");
            section.push_str(root);
            section.push('/');
            section.push_str(&skill.path);
            section.push('\n');
        }
        section.push_str("</skills>");
        Ok(section)
    }
}

impl PluginGuest for SkillsProvider {
    async fn configure() -> Result<(), String> {
        Ok(())
    }

    async fn describe() -> Result<String, String> {
        // A prompt provider contributes instructions, not model metadata; the
        // host ignores every key this could carry.
        Ok("{}".into())
    }

    async fn list_tools() -> String {
        "[]".into()
    }

    async fn execute_tool(_name: String, _arguments_json: String) -> Result<String, String> {
        Err("prompt provider provides no tools".into())
    }

    async fn list_event_handlers() -> String {
        "[]".into()
    }

    async fn handle_event(_handler_id: String, _event_json: String) -> Result<String, String> {
        Err("prompt provider has no event handlers".into())
    }

    async fn open_surface(_request_json: String) -> Result<String, String> {
        Err("prompt provider has no UI".into())
    }

    async fn handle_action(_action_json: String) -> Result<String, String> {
        Err("prompt provider has no UI".into())
    }

    async fn close_surface(_surface_id: String) {}
}

/// One skill, as the rendered section needs it.
#[derive(Debug)]
struct Skill {
    /// The frontmatter `name`, or the directory name when it is absent.
    name: String,
    /// The one-line summary, when the frontmatter carried one.
    description: Option<String>,
    /// The `SKILL.md` path, relative to the scope root the host bound.
    path: String,
}

/// The directory name when `path` is exactly `skills/<name>/SKILL.md`.
///
/// Only one level deep: that is the layout the format specifies, and recursing
/// would pick up the `references/` and `scripts/` directories a skill is
/// allowed to carry as if they were skills.
fn skill_directory(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(SKILLS_DIR)?.strip_prefix('/')?;
    let (name, file) = rest.split_once('/')?;
    if name.is_empty() || file != SKILL_FILE {
        return None;
    }
    Some(name)
}

/// Parses one `SKILL.md`, falling back to the directory name for its name.
fn read_one(text: &str, path: &str, directory: &str) -> Skill {
    let front = Frontmatter::split(text);
    let name = front
        .get("name")
        .map(str::to_string)
        .unwrap_or_else(|| directory.to_string());
    let description = front
        .get("description")
        .map(|text| cap_chars(text, MAX_DESCRIPTION_CHARS));
    Skill {
        name,
        description,
        path: path.to_string(),
    }
}

/// A leading `---` block's scalar fields.
///
/// Only `name` and `description` are read, and both are plain scalars, so this
/// is a narrow hand-rolled splitter rather than a YAML parser. Plugins are
/// written by other people on other machines, so a Windows-authored file's CRLF
/// endings, a description containing a colon, a quoted value, and a block whose
/// closing `---` is missing all have to parse the same way here as they did in
/// the host.
struct Frontmatter {
    fields: BTreeMap<String, String>,
}

impl Frontmatter {
    /// Splits `text` into its leading `---` block. Never fails: a file with no
    /// block, or with an opening `---` that is never closed, yields no fields.
    fn split(text: &str) -> Self {
        let mut rest = text;
        // Leading blank lines are tolerated; anything else before the first
        // `---` means there is no frontmatter.
        loop {
            let Some((line, after)) = next_line(rest) else {
                return Self::empty();
            };
            if line.trim().is_empty() {
                rest = after;
                continue;
            }
            if line.trim() != "---" {
                return Self::empty();
            }
            rest = after;
            break;
        }

        let mut fields = BTreeMap::new();
        loop {
            let Some((line, after)) = next_line(rest) else {
                // Without the closing `---` there is no way to tell fields from
                // prose, and guessing would silently swallow a name.
                return Self::empty();
            };
            if line.trim() == "---" {
                return Self { fields };
            }
            // Split on the *first* colon: a description routinely contains one
            // ("Control Windows apps: a walkthrough"), a key never does.
            if let Some((key, value)) = line.split_once(':') {
                let value = unquote(value.trim());
                if !value.is_empty() {
                    // A repeated key keeps its last value, as YAML would.
                    fields.insert(key.trim().to_string(), value);
                }
            }
            rest = after;
        }
    }

    fn empty() -> Self {
        Self {
            fields: BTreeMap::new(),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
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

export!(SkillsProvider);

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SKILL.md` with CRLF line endings, which the files on this machine
    /// really do use.
    const DESKTOP_UI: &str = "---\r\nname: desktop-ui\r\ndescription: Automate desktop windows\r\n---\r\n\r\n# Desktop UI\r\n\r\nUse this skill to automate the UI of desktop applications.\r\n";

    #[test]
    fn the_standard_layout_names_a_skill_and_summarises_it() {
        let skill = read_one(DESKTOP_UI, "skills/desktop-ui/SKILL.md", "desktop-ui");

        assert_eq!(skill.name, "desktop-ui");
        assert_eq!(
            skill.description.as_deref(),
            Some("Automate desktop windows"),
            "the trailing carriage return must not survive into the value"
        );
        assert_eq!(skill.path, "skills/desktop-ui/SKILL.md");
    }

    #[test]
    fn a_skill_without_frontmatter_falls_back_to_its_directory_name() {
        let skill = read_one(
            "# Deploy\n\nDo the thing.",
            "skills/deploy/SKILL.md",
            "deploy",
        );

        assert_eq!(skill.name, "deploy");
        assert_eq!(skill.description, None);
    }

    #[test]
    fn a_description_may_contain_a_colon_and_quotes() {
        let skill = read_one(
            "---\nname: a\ndescription: \"Use when: the build fails\"\n---\n",
            "skills/a/SKILL.md",
            "a",
        );

        assert_eq!(skill.name, "a");
        assert_eq!(
            skill.description.as_deref(),
            Some("Use when: the build fails")
        );
    }

    #[test]
    fn a_long_description_is_capped() {
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 50);
        let skill = read_one(
            &format!("---\nname: verbose\ndescription: {long}\n---\n"),
            "skills/verbose/SKILL.md",
            "verbose",
        );

        let description = skill.description.as_ref().unwrap();
        assert_eq!(
            description.chars().count(),
            MAX_DESCRIPTION_CHARS + 1,
            "capped plus the ellipsis"
        );
        assert!(description.ends_with('…'));
    }

    #[test]
    fn an_unterminated_block_is_treated_as_absent() {
        // Reading the fields anyway would swallow the name, so the directory
        // name wins instead.
        let skill = read_one(
            "---\nname: a\ndescription: b\n",
            "skills/dir/SKILL.md",
            "dir",
        );

        assert_eq!(skill.name, "dir");
        assert_eq!(skill.description, None);
    }

    #[test]
    fn only_skills_name_skill_md_at_the_top_level() {
        assert_eq!(
            skill_directory("skills/desktop-ui/SKILL.md"),
            Some("desktop-ui")
        );
        assert_eq!(
            skill_directory("skills/SKILL.md"),
            None,
            "one level too few"
        );
        assert_eq!(
            skill_directory("skills/real/references/notes.md"),
            None,
            "a skill's own subdirectories are not skills"
        );
        assert_eq!(skill_directory("skills/real/README.md"), None);
        assert_eq!(skill_directory("other/real/SKILL.md"), None);
        assert_eq!(skill_directory("skills/"), None);
    }

    #[test]
    fn skills_are_sorted_by_name_so_the_prompt_is_byte_stable() {
        let mut found: BTreeMap<String, Skill> = BTreeMap::new();
        for name in ["zeta", "alpha", "mid"] {
            let path = format!("skills/{name}/SKILL.md");
            let skill = read_one(&format!("---\nname: {name}\n---\n"), &path, name);
            found.entry(skill.name.clone()).or_insert(skill);
        }

        let names: Vec<String> = found.into_values().map(|skill| skill.name).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }
}
