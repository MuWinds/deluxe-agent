//! The tool layer: everything the model is allowed to do to this machine.
//!
//! The shape of this module is inherited from `local-tool-bridge`, but the
//! policy engine, path sandbox, and approval step are gone — this agent runs
//! with full permissions. What is left is schema validation (so a typo'd
//! argument fails loudly instead of being dropped) and resource bounding.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::attachments::ImageRef;
use crate::error::{AgentError, Result};

pub mod fs;
pub mod image;
pub mod jobs;
pub mod patch;
pub mod settings;
pub mod shell;
pub mod task;

pub use crate::harness::HunkLines;
pub use jobs::JobRegistry;
pub use settings::ToolSettings;

/// The JSON Schema subset the catalogue expresses.
///
/// This is deliberately not a general schema type: it carries exactly what the
/// tools declare, and it serialises straight into the `parameters` field of an
/// OpenAI function definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDescriptor {
    pub name: String,
    pub summary: String,
    pub description: String,
    /// Cross-tool behavioural rules injected into the system prompt.
    ///
    /// Kept apart from `description` on purpose. The schema description is read
    /// when the model decides how to call *this* tool; a guideline is a global
    /// preference ("edit with this tool, not by scripting that one") that no
    /// single tool's description can carry. They land in the `<rules>` section
    /// verbatim, deduplicated by the registry.
    pub guidelines: Vec<String>,
    pub input_schema: ObjectSchema,
    /// Whether the host validates arguments against `input_schema` before
    /// dispatch.
    ///
    /// `true` for the built-ins, whose schemas are hand-written and closed.
    /// `false` for a tool whose schema belongs to somebody else — an MCP
    /// server's, which may use keywords this narrow [`ObjectSchema`] cannot
    /// express, and where the server is the authority anyway. Refusing a call
    /// the server would have accepted is worse than not checking: the host
    /// would be inventing a rejection the real validator never made.
    pub host_validates_arguments: bool,
    /// Whether the tool can change something. Informational; nothing gates on
    /// it now that there is no approval step.
    pub mutating: bool,
}

/// One piece of a tool's output, in the shape the model receives it.
///
/// Text is what the transcript shows; an image is a durable reference that is
/// resolved into a `data:` URL only when a request is assembled. The two never
/// mix: [`ToolOutput::as_text`] ignores images entirely, so an image's base64
/// can never leak into a log line or a compaction prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { image: ImageRef },
}

impl ContentBlock {
    /// A text block, the shape almost every tool returns.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// The text this block contributes, if it is a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::Image { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Real file line numbers an `apply_patch` call resolved its hunks to.
    ///
    /// Computed at execution time, when the target file is read — the patch
    /// format's bare `@@` markers carry no ranges, so this is the only moment
    /// the numbers can be known for certain. Carried to the transcript for the
    /// diff's gutter and deliberately not part of `content`: the model sees
    /// the hunks it wrote, not the bookkeeping this field records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<HunkLines>,
}

impl ToolOutput {
    /// A failed call. `is_error` is what tells the model the block is a
    /// complaint rather than a result.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: true,
            truncated: false,
            original_bytes: None,
            duration_ms: None,
            hunks: Vec::new(),
        }
    }

    /// A successful result whose whole content is one block of text.
    ///
    /// The counterpart to [`ToolOutput::error`], for a tool that produces a
    /// plain answer rather than a rendering — `task`, whose result is a
    /// sub-agent's reply.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            is_error: false,
            truncated: false,
            original_bytes: None,
            duration_ms: None,
            hunks: Vec::new(),
        }
    }

    /// The text handed back to the model as the `tool` message body.
    pub fn as_text(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("")
    }

    /// The images this output carries, in the order they were produced.
    pub fn images(&self) -> Vec<ImageRef> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image { image } => Some(image.clone()),
                ContentBlock::Text { .. } => None,
            })
            .collect()
    }

    /// Caps the text output, cutting on a character boundary so a multi-byte
    /// character is never split into invalid UTF-8.
    ///
    /// Only text is measured and cut: an image block is a small reference, and
    /// dropping it would silently lose the picture the model asked for.
    pub fn truncate_to(mut self, max: usize) -> Self {
        let total: usize = self
            .content
            .iter()
            .filter_map(ContentBlock::as_text)
            .map(str::len)
            .sum();
        if total <= max {
            return self;
        }

        let mut remaining = max;
        let mut kept = Vec::new();
        for block in self.content {
            match block {
                ContentBlock::Image { .. } => kept.push(block),
                ContentBlock::Text { text } => {
                    if remaining == 0 {
                        continue;
                    }
                    if text.len() <= remaining {
                        remaining -= text.len();
                        kept.push(ContentBlock::Text { text });
                    } else {
                        let mut end = remaining;
                        while end > 0 && !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        kept.push(ContentBlock::text(&text[..end]));
                        remaining = 0;
                    }
                }
            }
        }

        self.content = kept;
        self.truncated = true;
        self.original_bytes = Some(total);
        self
    }
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// The catalogue entry the model sees: name, summary, schema and any
    /// cross-tool guidelines.
    fn descriptor(&self) -> ToolDescriptor;

    /// Runs one call.
    ///
    /// `arguments` is the raw JSON the model produced — validated against the
    /// descriptor's `input_schema` first unless `host_validates_arguments` is
    /// off. Returns [`AgentError`] for a call that could not run at all; a call
    /// that ran but failed is reported as an `is_error` [`ToolOutput`] instead,
    /// so the model sees the failure inside the conversation.
    async fn execute(&self, arguments: Value, settings: &ToolSettings) -> Result<ToolOutput>;

    /// Whether this tool bounds its own wall-clock time.
    ///
    /// The host wraps every call in `default_timeout_ms` as a safety net, but a
    /// tool that manages its own budget — `exec`, whose foreground wait ends by
    /// promoting the command to a background job rather than failing — would be
    /// cut off by that shorter net. Such a tool opts out, and the host uses a
    /// generous ceiling instead.
    fn bounds_own_timeout(&self) -> bool {
        false
    }
}

/// The tool catalogue.
///
/// `Clone` is shallow and cheap — the entries are `Arc`s — and exists for one
/// caller: `task`, which hands a sub-agent the registry *as it stood before
/// `task` itself was registered*. That snapshot is what bounds delegation to a
/// single level; see [`crate::tools::task`].
#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// The background job runtime every tool in this registry shares.
    ///
    /// Held here rather than in `main` so a tool that starts a job (`exec`,
    /// `task`) and the tools that read one (`job_output`, `job_list`,
    /// `job_kill`) are wired to the same registry without a separate handle to
    /// thread through. The `task` tool's sub-registry is a clone, so a
    /// delegated job is visible to its parent.
    jobs: Arc<JobRegistry>,
}

impl ToolRegistry {
    /// The registry every agent starts from: `read_file`, `list_dir`, `exec`,
    /// `apply_patch` and the three `job_*` tools, sharing one job runtime.
    pub fn with_builtins() -> Self {
        let job_registry = Arc::new(JobRegistry::new());
        let mut registry = Self {
            tools: BTreeMap::new(),
            jobs: job_registry.clone(),
        };
        registry.register(Arc::new(fs::ReadFile));
        registry.register(Arc::new(fs::ListDir));
        registry.register(Arc::new(shell::Exec::new(job_registry.clone())));
        registry.register(Arc::new(patch::ApplyPatch));
        registry.register(Arc::new(jobs::JobOutput::new(job_registry.clone())));
        registry.register(Arc::new(jobs::JobList::new(job_registry.clone())));
        registry.register(Arc::new(jobs::JobKill::new(job_registry)));
        registry
    }

    /// The built-ins plus `read_image`.
    ///
    /// Registered only for a model that declares image input. A tool that hands
    /// pictures to a model that cannot see them is a wasted call and a
    /// confusing provider error, so the capability decides whether the tool
    /// exists at all — the same gate the DeepSeek Harness applies.
    pub fn with_image_input() -> Self {
        let mut registry = Self::with_builtins();
        registry.register(Arc::new(image::ReadImage));
        registry
    }

    /// The background job runtime this registry's tools share.
    pub fn jobs(&self) -> &Arc<JobRegistry> {
        &self.jobs
    }

    /// Adds (or replaces) a tool, keyed by the name its descriptor declares.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.descriptor().name, tool);
    }

    /// The tool registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Like [`get`](Self::get), but an unknown name is an
    /// [`AgentError::tool_not_found`] rather than `None`.
    pub fn require(&self, name: &str) -> Result<&Arc<dyn Tool>> {
        self.get(name)
            .ok_or_else(|| AgentError::tool_not_found(name))
    }

    /// Every registered tool's descriptor, in name order (the registry is a
    /// `BTreeMap`), which is what keeps the system prompt byte-stable.
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools.values().map(|tool| tool.descriptor()).collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// Renders the catalogue as the OpenAI `tools` request field.
/// Renders a tool schema from a runtime-owned descriptor list.
pub fn to_openai_tools_from_descriptors(descriptors: &[ToolDescriptor]) -> Value {
    Value::Array(descriptors.iter().map(to_openai_tool).collect())
}

fn to_openai_tool(descriptor: &ToolDescriptor) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": descriptor.name,
            "description": descriptor.description,
            "parameters": descriptor.input_schema,
        }
    })
}

/// Validates arguments against the declared schema.
///
/// Unknown keys are rejected rather than ignored: a silently dropped `path`
/// typo is how a tool ends up running against the wrong target.
pub fn validate_arguments(schema: &ObjectSchema, arguments: &Value) -> Result<()> {
    let object = arguments
        .as_object()
        .ok_or_else(|| AgentError::invalid_params("`arguments` must be a JSON object"))?;

    for required in &schema.required {
        if !object.contains_key(required) {
            return Err(AgentError::invalid_params(format!(
                "Missing required argument `{required}`"
            )));
        }
    }

    for (key, value) in object {
        let Some(expected) = schema.properties.get(key) else {
            return Err(AgentError::invalid_params(format!(
                "Unknown argument `{key}`"
            )));
        };
        let Some(kind) = expected.get("type").and_then(Value::as_str) else {
            continue;
        };

        let matches = match kind {
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        };

        if !matches {
            return Err(AgentError::invalid_params(format!(
                "Argument `{key}` must be of type {kind}"
            )));
        }

        if let Some(allowed) = expected.get("enum").and_then(Value::as_array) {
            if !allowed.contains(value) {
                return Err(AgentError::invalid_params(format!(
                    "Argument `{key}` must be one of {}",
                    allowed
                        .iter()
                        .map(|item| item.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
    }

    Ok(())
}

/// Reads a required string argument.
///
/// Errs when the key is absent or is not a string. A number where a string was
/// promised is a model mistake worth reporting, not something to coerce.
pub fn required_str(arguments: &Value, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            AgentError::invalid_params(format!("Missing required string argument `{key}`"))
        })
}

/// Reads an optional string argument; `None` when the key is absent or is not a
/// string.
pub fn optional_str(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Reads an optional unsigned integer argument, falling back to `default` when
/// the key is absent or the wrong type.
pub fn optional_u64(arguments: &Value, key: &str, default: u64) -> u64 {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(default)
}

/// Reads an optional boolean argument, falling back to `default` when the key
/// is absent or the wrong type.
pub fn optional_bool(arguments: &Value, key: &str, default: bool) -> bool {
    arguments
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// Clamps `value` into `min..=max`. The bounds are assumed ordered; a reversed
/// pair panics, as [`u64::clamp`] does.
///
/// # Panics
///
/// Panics if `min > max`.
pub fn clamp_u64(value: u64, min: u64, max: u64) -> u64 {
    value.clamp(min, max)
}
