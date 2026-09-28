//! `SKILL.md`: the one file that makes a plugin's skill usable.
//!
//! A skill is a directory holding a `SKILL.md` whose YAML frontmatter carries a
//! `name` and a `description`, and whose body is natural-language instructions.
//! Codex uses *progressive disclosure*: the session prompt lists only the name
//! and the description, and the model reads the whole file when a description
//! matches the task. That is what this module supports — it never loads the body
//! into the prompt, and the model reaches it through the ordinary `read_file`
//! tool, which is exactly how Codex does it.
//!
//! # Why the frontmatter is parsed by hand
//!
//! The only keys this agent reads are `name` and `description`, both plain
//! scalars. Pulling in a YAML parser to read two strings would be a large
//! dependency for a small need, and the codebase already prefers narrow
//! hand-rolled parsers for exactly this reason (see `ObjectSchema` in
//! `tools::mod`). The split itself lives in [`super::frontmatter`], because a
//! command's `commands/*.md` opens with the same block and the edge handling is
//! not worth having twice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{cap_chars, frontmatter, MAX_DESCRIPTION_CHARS};

/// One skill, as the prompt needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The skill's name: the frontmatter `name`, or the directory name when
    /// the frontmatter is absent or unusable.
    pub name: String,
    /// The one-line summary, when the frontmatter carried one.
    pub description: Option<String>,
    /// Absolute path to the `SKILL.md`, which is what the model is told to read.
    pub path: PathBuf,
    /// The plugin this came from, as `name@marketplace`.
    ///
    /// Carried so the prompt can say where a skill came from — with two
    /// marketplaces installed, "figma" alone does not identify a plugin — and so
    /// a log line about a skill names its owner.
    pub plugin: String,
}

/// The directory a plugin's skills live in, relative to its root.
const SKILLS_DIR: &str = "skills";
/// The file that marks a directory as a skill.
const SKILL_FILE: &str = "SKILL.md";

/// Reads every skill a plugin declares.
///
/// The standard `skills/<name>/SKILL.md` layout is always scanned; the
/// manifest's `skills` field is a *supplement*, not a replacement, so a path
/// there adds skills rather than redirecting the search. Skills are returned
/// sorted by name, which keeps the prompt byte-stable across runs — the same
/// property `Agent::build_system_prompt` relies on to keep provider prompt
/// caches warm.
///
/// A name declared twice keeps the first one found, so the standard location
/// wins a conflict with an extra declared path.
pub fn load(plugin: &str, root: &Path, declared: Option<&str>) -> Vec<Skill> {
    let mut found: BTreeMap<String, Skill> = BTreeMap::new();

    collect_dir(plugin, &root.join(SKILLS_DIR), &mut found);

    if let Some(declared) = declared {
        let declared = declared.trim();
        if !declared.is_empty() {
            // Paths in the format are relative and begin with `./`.
            let path = root.join(declared.trim_start_matches("./"));
            if path != root.join(SKILLS_DIR) {
                collect(plugin, &path, &mut found);
            }
        }
    }

    found.into_values().collect()
}

/// Collects either a single `SKILL.md` or every skill directory under `dir`.
fn collect(plugin: &str, path: &Path, found: &mut BTreeMap<String, Skill>) {
    if path.is_dir() {
        collect_dir(plugin, path, found);
    } else if path.is_file() {
        if let Some(skill) = read_one(plugin, path, None) {
            found.entry(skill.name.clone()).or_insert(skill);
        }
    }
}

/// Collects `<dir>/<name>/SKILL.md` for every immediate subdirectory.
///
/// Only one level deep: that is the layout the format specifies, and recursing
/// would pick up the `references/` and `scripts/` directories a skill is
/// allowed to carry as if they were skills.
fn collect_dir(plugin: &str, dir: &Path, found: &mut BTreeMap<String, Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }
        let file = skill_dir.join(SKILL_FILE);
        if !file.is_file() {
            continue;
        }
        // The directory name is the fallback name, which is why it is passed in
        // rather than derived from the file.
        let fallback = skill_dir.file_name().and_then(|name| name.to_str());
        if let Some(skill) = read_one(plugin, &file, fallback) {
            found.entry(skill.name.clone()).or_insert(skill);
        }
    }
}

/// Parses one `SKILL.md`, falling back to `fallback_name` when the frontmatter
/// is missing, unterminated, or carries no usable `name`.
fn read_one(plugin: &str, path: &Path, fallback_name: Option<&str>) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let front = frontmatter::split(&text);

    let name = front
        .get("name")
        .map(str::to_string)
        .or_else(|| fallback_name.map(str::to_string))?;

    let description = front
        .get("description")
        .map(|text| cap_chars(text, MAX_DESCRIPTION_CHARS));

    Some(Skill {
        name,
        description,
        path: path.to_path_buf(),
        plugin: plugin.to_string(),
    })
}

/// Renders the `<skills>` section of the system prompt.
///
/// `None` when there are no skills, so a plugin-free install gets a prompt with
/// no empty section in it.
pub fn render(skills: &[&Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut section = String::from(
        "<skills>\n\
         Reusable instructions contributed by installed plugins. The list carries only a \
         summary: when one matches the task, read its SKILL.md in full before acting.\n",
    );
    for skill in skills {
        section.push_str("- `");
        section.push_str(&skill.name);
        section.push_str("` (");
        section.push_str(&skill.plugin);
        section.push(')');
        if let Some(description) = &skill.description {
            section.push_str(": ");
            section.push_str(description);
        }
        section.push_str("\n  SKILL.md: ");
        section.push_str(&skill.path.display().to_string());
        section.push('\n');
    }
    section.push_str("</skills>");
    Some(section)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A `SKILL.md` shaped exactly like the real `computer-use` one, CRLF and
    /// all — the installs on this machine really do use Windows endings.
    const COMPUTER_USE: &str = "---\r\nname: computer-use\r\ndescription: Control Windows apps from Codex\r\n---\r\n\r\n# Computer Use\r\n\r\nUse this skill to automate the UI of Microsoft Windows apps.\r\n";

    fn skill_file(root: &Path, dir: &str, contents: &str) -> PathBuf {
        let path = root.join(dir);
        fs::create_dir_all(&path).unwrap();
        let file = path.join(SKILL_FILE);
        fs::write(&file, contents).unwrap();
        file
    }

    #[test]
    fn skills_are_discovered_in_the_standard_layout() {
        let temp = tempfile::tempdir().unwrap();
        skill_file(temp.path(), "skills/computer-use", COMPUTER_USE);
        skill_file(
            temp.path(),
            "skills/latex-compile",
            "---\nname: latex-compile\ndescription: Compile LaTeX\n---\n",
        );

        let skills = load("latex@openai-bundled", temp.path(), Some("./skills/"));

        assert_eq!(
            skills.iter().map(|skill| skill.name.as_str()).collect::<Vec<_>>(),
            vec!["computer-use", "latex-compile"],
            "sorted by name, so the prompt is byte-stable"
        );
        assert_eq!(skills[0].plugin, "latex@openai-bundled");
        assert!(skills[0].path.ends_with("SKILL.md"));
        assert!(skills[0].path.is_absolute());
    }

    #[test]
    fn a_skill_without_frontmatter_falls_back_to_its_directory_name() {
        let temp = tempfile::tempdir().unwrap();
        skill_file(temp.path(), "skills/repo-triage", "# Repo Triage\n\nDo the thing.");

        let skills = load("p@m", temp.path(), None);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "repo-triage");
        assert_eq!(skills[0].description, None);
    }

    #[test]
    fn nested_directories_inside_a_skill_are_not_skills() {
        // A skill may carry `references/` and `scripts/`; recursing would
        // mistake them for skills.
        let temp = tempfile::tempdir().unwrap();
        skill_file(temp.path(), "skills/real", "---\nname: real\n---\n");
        fs::create_dir_all(temp.path().join("skills/real/references")).unwrap();
        fs::write(temp.path().join("skills/real/references/notes.md"), "x").unwrap();

        let skills = load("p@m", temp.path(), None);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "real");
    }

    #[test]
    fn a_directory_without_a_skill_file_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("skills/empty")).unwrap();
        skill_file(temp.path(), "skills/real", "---\nname: real\n---\n");

        assert_eq!(load("p@m", temp.path(), None).len(), 1);
    }

    #[test]
    fn a_plugin_with_no_skills_directory_yields_nothing() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load("p@m", temp.path(), None).is_empty());
    }

    #[test]
    fn the_standard_location_wins_a_name_conflict() {
        // `skills` supplements the standard layout; it does not redirect it.
        let temp = tempfile::tempdir().unwrap();
        skill_file(
            temp.path(),
            "skills/dup",
            "---\nname: dup\ndescription: from the standard location\n---\n",
        );
        skill_file(
            temp.path(),
            "extra/dup",
            "---\nname: dup\ndescription: from the declared path\n---\n",
        );

        let skills = load("p@m", temp.path(), Some("./extra"));

        assert_eq!(skills.len(), 1, "the same name is one skill, not two");
        assert_eq!(skills[0].description.as_deref(), Some("from the standard location"));
    }

    #[test]
    fn a_declared_path_pointing_back_at_the_standard_one_adds_nothing() {
        let temp = tempfile::tempdir().unwrap();
        skill_file(temp.path(), "skills/one", "---\nname: one\n---\n");

        assert_eq!(load("p@m", temp.path(), Some("./skills/")).len(), 1);
    }

    #[test]
    fn a_long_description_is_capped() {
        let temp = tempfile::tempdir().unwrap();
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 50);
        skill_file(
            temp.path(),
            "skills/verbose",
            &format!("---\nname: verbose\ndescription: {long}\n---\n"),
        );

        let skills = load("p@m", temp.path(), None);

        let description = skills[0].description.as_ref().unwrap();
        assert_eq!(description.chars().count(), MAX_DESCRIPTION_CHARS + 1, "capped plus the ellipsis");
        assert!(description.ends_with('…'));
    }

    #[test]
    fn the_section_names_each_skill_and_where_to_read_it() {
        let temp = tempfile::tempdir().unwrap();
        skill_file(temp.path(), "skills/computer-use", COMPUTER_USE);
        let skills = load("computer-use@openai-bundled", temp.path(), None);

        let section = render(&skills.iter().collect::<Vec<_>>()).unwrap();

        assert!(section.starts_with("<skills>"));
        assert!(section.ends_with("</skills>"));
        assert!(section.contains("`computer-use`"), "{section}");
        assert!(
            section.contains("computer-use@openai-bundled"),
            "the plugin is named, so two marketplaces cannot be confused: {section}"
        );
        assert!(section.contains("Control Windows apps from Codex"), "{section}");

        // The path in the prompt has to be one the model can actually read, so
        // it is checked against the filesystem rather than against a second
        // hand-built string — building that string independently is how a
        // separator mismatch would slip through unnoticed.
        let path = &skills[0].path;
        assert!(path.is_file(), "the advertised path must exist: {}", path.display());
        assert!(section.contains(&path.display().to_string()), "{section}");

        // Progressive disclosure: the body is not inlined.
        assert!(
            !section.contains("Use this skill to automate"),
            "only the summary goes in the prompt: {section}"
        );
    }

    #[test]
    fn a_skill_without_a_description_still_renders() {
        let temp = tempfile::tempdir().unwrap();
        skill_file(temp.path(), "skills/bare", "---\nname: bare\n---\n");
        let skills = load("p@m", temp.path(), None);

        let section = render(&skills.iter().collect::<Vec<_>>()).unwrap();

        assert!(section.contains("`bare` (p@m)"), "{section}");
        assert!(section.contains("SKILL.md:"), "{section}");
    }

    #[test]
    fn no_skills_means_no_section() {
        assert_eq!(render(&[]), None);
    }
}
