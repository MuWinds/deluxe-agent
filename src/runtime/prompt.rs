//! Native prompt rendering from implementation-neutral snapshots.

use std::path::Path;

use crate::harness::ports::PromptProvider;
use crate::harness::{ProjectInstruction, PromptContext};
use crate::tools::ToolDescriptor;

pub struct NativePromptProvider;

impl NativePromptProvider {
    /// Creates the native prompt renderer and OpenAI-compatible schema adapter.
    pub fn new() -> Self {
        Self
    }
}

impl Default for NativePromptProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptProvider for NativePromptProvider {
    fn build_system_prompt(&self, context: &PromptContext) -> crate::error::Result<String> {
        Ok(build_system_prompt(context))
    }

    fn tool_schema(&self, tools: &[ToolDescriptor]) -> serde_json::Value {
        crate::tools::to_openai_tools_from_descriptors(tools)
    }
}

const PROJECT_CONTEXT_FILES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "README.md"];
const MAX_PROJECT_CONTEXT_BYTES: usize = 16_000;

const HOST_RULES: &str = "\
- Read a file before you change it.
- Relative paths resolve against the working directory; absolute paths are used \
as given. You have full read and write access to this machine.
- Say in one short sentence what you are about to do before you call a tool, so \
the user can follow along.
- Do not repeat a failing call unchanged. If a tool returns an error, read it and \
adjust. If it says the call was refused, the host blocked it — do not try again \
and do not look for a way around it.
- When the task is done, reply with a short summary of what changed. Do not call \
a tool just to confirm.
</rules>";

const SUB_AGENT_RULES: &str = "\
- You are a sub-agent: another agent delegated a single task to you. Complete it \
and answer with the result. You cannot ask the caller anything, so state any \
assumption you had to make rather than stopping.
- When the task is done, reply with your findings. Do not call a tool just to \
confirm.
</rules>";

/// Reads project instruction files in their stable precedence order.
///
/// Missing, unreadable, and blank files are skipped. Each trimmed file is capped
/// at 16,000 bytes on a UTF-8 character boundary to preserve the original prompt
/// contract without allowing a large README to dominate the context.
pub fn read_project_instructions(project: &Path) -> Vec<ProjectInstruction> {
    let mut instructions = Vec::new();
    for name in PROJECT_CONTEXT_FILES {
        let Ok(contents) = std::fs::read_to_string(project.join(name)) else {
            continue;
        };
        let contents = contents.trim();
        if contents.is_empty() {
            continue;
        }
        instructions.push(ProjectInstruction {
            path: name.to_string(),
            content: cap_bytes(contents).to_string(),
        });
    }
    instructions
}

/// Builds the fixed root-agent prompt from tools and plugin-provided summaries.
pub fn build_system_prompt(context: &PromptContext) -> String {
    let mut prompt =
        String::from("You are a useful coding agent running on the user's own machine.\n\n");
    prompt.push_str(&tool_section(&context.tools));

    // Each plugin formats its own text; the host only separates the blocks so a
    // section cannot run into the tool list or the rules.
    for section in &context.plugin_sections {
        if section.trim().is_empty() {
            continue;
        }
        prompt.push('\n');
        prompt.push_str(section);
        prompt.push('\n');
    }

    prompt.push('\n');
    prompt.push_str(&rule_section(&context.tools, HOST_RULES));

    if !context.project_instructions.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&render_project_context(&context.project_instructions));
    }
    prompt
}

/// Builds the fixed delegated-agent prompt from its instructions and tools.
///
/// The instructions are a Component's own text, handed over through the host's
/// `run-agent` capability; the host only frames them with the tool list and the
/// sub-agent rules.
pub fn build_role_prompt(instructions: &str, tools: &[ToolDescriptor]) -> String {
    format!(
        "{instructions}\n\n{}{}",
        tool_section(tools),
        rule_section(tools, SUB_AGENT_RULES)
    )
}

fn tool_section(tools: &[ToolDescriptor]) -> String {
    let entries = tools
        .iter()
        .map(|tool| format!("- `{}`: {}", tool.name, tool.summary))
        .collect::<Vec<_>>()
        .join("\n");
    format!("<tools>\n{entries}\n</tools>\n")
}

fn rule_section(tools: &[ToolDescriptor], host_rules: &str) -> String {
    let mut rules = String::from("<rules>\n");
    let mut seen = std::collections::HashSet::new();
    for rule in tools.iter().flat_map(|tool| &tool.guidelines) {
        if seen.insert(rule) {
            rules.push_str("- ");
            rules.push_str(rule);
            rules.push('\n');
        }
    }
    rules.push_str(host_rules);
    rules
}

fn render_project_context(context: &[ProjectInstruction]) -> String {
    let mut rendered = String::from("Project-specific instructions and guidelines:\n");
    for instruction in context {
        rendered.push_str(&format!(
            "\n<project_instructions path=\"{}\">\n{}\n</project_instructions>\n",
            instruction.path, instruction.content
        ));
    }
    rendered
}

fn cap_bytes(text: &str) -> &str {
    if text.len() <= MAX_PROJECT_CONTEXT_BYTES {
        return text;
    }
    let mut end = MAX_PROJECT_CONTEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tools::{ObjectSchema, ToolRegistry};
    use serde_json::json;

    fn prompt_tools() -> Vec<ToolDescriptor> {
        ToolRegistry::with_builtins().descriptors()
    }

    fn prompt_context() -> PromptContext {
        PromptContext {
            tools: prompt_tools(),
            plugin_sections: Vec::new(),
            project_instructions: Vec::new(),
        }
    }

    #[test]
    fn builtin_tool_prompt_and_host_rules_are_stable() {
        let prompt = build_system_prompt(&prompt_context());
        assert!(prompt.starts_with(
            "You are a useful coding agent running on the user's own machine.\n\n<tools>\n"
        ));
        for name in ["apply_patch", "exec", "list_dir", "read_file"] {
            assert!(
                prompt.contains(&format!("- `{name}`:")),
                "the catalogue is missing `{name}`: {prompt}"
            );
        }
        assert!(prompt.contains("one patch rather than two"));
        assert!(prompt.contains("Prefer `read_file` over `exec`"));
        assert!(
            prompt.ends_with(HOST_RULES),
            "host rule bytes remain stable"
        );
    }

    #[test]
    fn plugin_sections_precede_rules_and_empty_sections_are_omitted() {
        let mut context = prompt_context();
        context
            .plugin_sections
            .push("<contributed>\nA block a Component wrote.\n</contributed>".into());

        let prompt = build_system_prompt(&context);
        assert!(prompt.contains("<contributed>"));
        assert!(prompt.contains("A block a Component wrote."));
        assert!(
            prompt.find("<contributed>").unwrap() < prompt.find("<rules>").unwrap(),
            "contributed text keeps its established place before the rules"
        );

        let empty = build_system_prompt(&prompt_context());
        assert!(!empty.contains("<contributed>"), "{empty}");

        // A Component that contributes only whitespace must not leave a gap.
        let mut blank = prompt_context();
        blank.plugin_sections.push("  \n\t".into());
        assert_eq!(build_system_prompt(&blank), empty);
    }

    #[test]
    fn project_instruction_order_and_role_prompt_bytes_are_stable() {
        let mut context = prompt_context();
        context.project_instructions = vec![
            ProjectInstruction {
                path: "AGENTS.md".into(),
                content: "agent rules".into(),
            },
            ProjectInstruction {
                path: "CLAUDE.md".into(),
                content: "claude rules".into(),
            },
            ProjectInstruction {
                path: "README.md".into(),
                content: "readme".into(),
            },
        ];
        let prompt = build_system_prompt(&context);
        let agents = prompt.find("path=\"AGENTS.md\"").unwrap();
        let claude = prompt.find("path=\"CLAUDE.md\"").unwrap();
        let readme = prompt.find("path=\"README.md\"").unwrap();
        assert!(agents < claude && claude < readme);

        let role_prompt = build_role_prompt("Do the assigned work.", &context.tools);
        assert!(role_prompt.starts_with("Do the assigned work.\n\n<tools>\n"));
        assert!(role_prompt.ends_with(SUB_AGENT_RULES));
        assert!(!role_prompt.contains(HOST_RULES));
    }

    #[test]
    fn native_tool_schema_keeps_the_openai_shape() {
        let tool = ToolDescriptor {
            name: "example".into(),
            summary: "An example".into(),
            description: "An example tool".into(),
            guidelines: Vec::new(),
            input_schema: ObjectSchema {
                schema_type: "object".into(),
                properties: serde_json::from_value(json!({})).unwrap(),
                required: Vec::new(),
            },
            host_validates_arguments: true,
            mutating: false,
        };
        let schema = crate::tools::to_openai_tools_from_descriptors(&[tool]);
        assert_eq!(schema[0]["type"], "function");
        assert_eq!(schema[0]["function"]["name"], "example");
    }

    #[test]
    fn project_context_caps_utf8_safely_and_skips_missing_files() {
        let dir = tempfile::tempdir().expect("a temp project exists");
        std::fs::write(
            dir.path().join("AGENTS.md"),
            format!("{}\n", "界".repeat(6_000)),
        )
        .expect("the instruction file is written");

        let context = read_project_instructions(dir.path());

        assert_eq!(context.len(), 1);
        assert_eq!(context[0].path, "AGENTS.md");
        assert!(context[0].content.len() <= MAX_PROJECT_CONTEXT_BYTES);
        assert!(context[0]
            .content
            .is_char_boundary(context[0].content.len()));
    }
}
