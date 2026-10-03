//! Versioned, bounded UI snapshots shared by IPC, renderers, and plugins.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{code, AgentError, Result};

pub const UI_SCHEMA_VERSION: u32 = 1; // 1 = declarative snapshot protocol.
const MAX_NODES: usize = 512;
const MAX_DEPTH: usize = 24;
const MAX_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfaceRequest {
    pub plugin_id: String,
    pub project: PathBuf,
    pub surface_id: String,
    pub request_id: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUiDocument {
    pub schema_version: u32,
    pub plugin_id: String,
    pub surface_id: String,
    pub revision: u64,
    pub title: String,
    pub root: UiNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TextEmphasis {
    #[default]
    Normal,
    Strong,
    Muted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UiNode {
    Empty,
    Column {
        children: Vec<UiNode>,
    },
    Row {
        children: Vec<UiNode>,
    },
    Section {
        id: String,
        title: String,
        children: Vec<UiNode>,
    },
    Text {
        text: String,
        #[serde(default)]
        emphasis: TextEmphasis,
    },
    Button {
        id: String,
        label: String,
        action: String,
        enabled: bool,
    },
    TextInput {
        id: String,
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        action: String,
    },
    Checkbox {
        id: String,
        label: String,
        value: bool,
        action: String,
    },
    Select {
        id: String,
        value: String,
        options: Vec<SelectOption>,
        action: String,
    },
    Progress {
        value: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Divider,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
pub enum UiValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Strings(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginUiAction {
    pub surface: SurfaceRequest,
    pub revision: u64,
    pub control_id: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<UiValue>,
}

/// Decodes a JSON document. Returns `Err` for malformed data.
pub fn decode_document(json: &str) -> Result<PluginUiDocument> {
    serde_json::from_str(json).map_err(|error| invalid(format!("Invalid UI JSON: {error}")))
}

/// Validates identity, monotonic revision, tree budgets, IDs, and declared actions.
///
/// Returns `Err` when a plugin snapshot violates any host protocol constraint.
pub fn validate_document(
    document: &PluginUiDocument,
    request: &SurfaceRequest,
    actions: &[String],
    previous_revision: Option<u64>,
) -> Result<()> {
    if document.schema_version != UI_SCHEMA_VERSION
        || document.plugin_id != request.plugin_id
        || document.surface_id != request.surface_id
        || document.revision == 0
        || previous_revision.is_some_and(|old| document.revision <= old)
    {
        return Err(invalid(
            "UI identity, schema version, or revision is invalid",
        ));
    }
    check_text(&document.title)?;
    let mut ids = HashSet::new();
    let mut stack = vec![(&document.root, 1)];
    let mut count = 0;
    while let Some((node, depth)) = stack.pop() {
        count += 1;
        if count > MAX_NODES || depth > MAX_DEPTH {
            return Err(invalid("UI tree exceeds the node or depth limit"));
        }
        let mut texts: Vec<&str> = Vec::new();
        let mut identity: Option<(&str, Option<&str>)> = None;
        match node {
            UiNode::Column { children } | UiNode::Row { children } => {
                stack.extend(children.iter().map(|child| (child, depth + 1)));
            }
            UiNode::Section {
                id,
                title,
                children,
            } => {
                identity = Some((id, None));
                texts.push(title);
                stack.extend(children.iter().map(|child| (child, depth + 1)));
            }
            UiNode::Text { text, .. } => texts.push(text),
            UiNode::Button {
                id, label, action, ..
            }
            | UiNode::Checkbox {
                id, label, action, ..
            } => {
                identity = Some((id, Some(action)));
                texts.push(label);
            }
            UiNode::TextInput {
                id,
                value,
                placeholder,
                action,
            } => {
                identity = Some((id, Some(action)));
                texts.push(value);
                texts.extend(placeholder.as_deref());
            }
            UiNode::Select {
                id,
                value,
                options,
                action,
            } => {
                identity = Some((id, Some(action)));
                if options.len() > 128 {
                    return Err(invalid("Select has too many options"));
                }
                let mut values = HashSet::new();
                for option in options {
                    if !values.insert(&option.value) {
                        return Err(invalid("Select option values must be unique"));
                    }
                    texts.extend([option.value.as_str(), option.label.as_str()]);
                }
                if !values.contains(value) {
                    return Err(invalid("Select value is not a declared option"));
                }
            }
            UiNode::Progress { value, label } => {
                if !value.is_finite() || !(0.0..=1.0).contains(value) {
                    return Err(invalid("Progress value must be finite and between 0 and 1"));
                }
                texts.extend(label.as_deref());
            }
            UiNode::Empty | UiNode::Divider => {}
        }
        if let Some((id, action)) = identity {
            if !valid_identifier(id) || !ids.insert(id) {
                return Err(invalid("UI control IDs must be valid and unique"));
            }
            if let Some(action) = action {
                if !valid_identifier(action) || !actions.iter().any(|allowed| allowed == action) {
                    return Err(invalid("UI action was not declared by the plugin"));
                }
            }
        }
        for text in texts {
            check_text(text)?;
        }
    }
    Ok(())
}

/// Checks an action against its snapshot, including control kind and allowed values.
///
/// Returns `Err` for stale, fabricated, disabled, or incorrectly typed actions.
pub fn validate_action(document: &PluginUiDocument, action: &PluginUiAction) -> Result<()> {
    if action.revision != document.revision
        || action.surface.plugin_id != document.plugin_id
        || action.surface.surface_id != document.surface_id
    {
        return Err(invalid("UI action targets a stale snapshot"));
    }
    let mut stack = vec![&document.root];
    while let Some(node) = stack.pop() {
        let valid = match node {
            UiNode::Column { children }
            | UiNode::Row { children }
            | UiNode::Section { children, .. } => {
                stack.extend(children);
                false
            }
            UiNode::Button {
                id,
                action: name,
                enabled,
                ..
            } => {
                id == &action.control_id
                    && name == &action.action
                    && *enabled
                    && action.value.is_none()
            }
            UiNode::TextInput {
                id, action: name, ..
            } => {
                id == &action.control_id
                    && name == &action.action
                    && matches!(&action.value, Some(UiValue::String(value)) if value.len() <= MAX_TEXT_BYTES)
            }
            UiNode::Checkbox {
                id, action: name, ..
            } => {
                id == &action.control_id
                    && name == &action.action
                    && matches!(action.value, Some(UiValue::Bool(_)))
            }
            UiNode::Select {
                id,
                action: name,
                options,
                ..
            } => {
                id == &action.control_id
                    && name == &action.action
                    && matches!(&action.value, Some(UiValue::String(value)) if options.iter().any(|option| &option.value == value))
            }
            _ => false,
        };
        if valid {
            return Ok(());
        }
    }
    Err(invalid("UI action does not match an enabled control"))
}

/// Accepts short protocol identifiers without path separators or control characters.
pub fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn check_text(value: &str) -> Result<()> {
    if value.len() > MAX_TEXT_BYTES {
        Err(invalid("UI text field exceeds its limit"))
    } else {
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::new(code::PLUGIN_INVALID_OUTPUT, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn request() -> SurfaceRequest {
        SurfaceRequest {
            plugin_id: "echo@test".into(),
            project: "/p".into(),
            surface_id: "main".into(),
            request_id: 1,
        }
    }

    pub fn document() -> PluginUiDocument {
        PluginUiDocument {
            schema_version: UI_SCHEMA_VERSION,
            plugin_id: "echo@test".into(),
            surface_id: "main".into(),
            revision: 1,
            title: "Echo".into(),
            root: UiNode::Button {
                id: "go".into(),
                label: "Go".into(),
                action: "increment".into(),
                enabled: true,
            },
        }
    }

    #[test]
    fn snapshots_round_trip_and_ignore_unknown_fields() {
        let text = serde_json::to_string(&document()).expect("snapshot serializes");
        assert_eq!(
            decode_document(&text).expect("snapshot decodes"),
            document()
        );
        let mut json: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        json["future"] = true.into();
        assert_eq!(
            decode_document(&json.to_string()).expect("future fields ignored"),
            document()
        );
    }

    #[test]
    fn snapshots_reject_identity_revision_and_undeclared_actions() {
        let mut doc = document();
        let actions = vec!["increment".into()];
        assert!(validate_document(&doc, &request(), &actions, None).is_ok());
        assert!(validate_document(&doc, &request(), &actions, Some(1)).is_err());
        assert!(validate_document(&doc, &request(), &[], None).is_err());
        doc.schema_version += 1;
        assert!(validate_document(&doc, &request(), &actions, None).is_err());
        doc = document();
        doc.plugin_id = "other@test".into();
        assert!(validate_document(&doc, &request(), &actions, None).is_err());
    }

    #[test]
    fn tree_budgets_and_duplicate_ids_are_enforced() {
        let mut doc = document();
        let actions = vec!["increment".into()];
        doc.root = UiNode::Column {
            children: vec![doc.root.clone(), doc.root.clone()],
        };
        assert!(validate_document(&doc, &request(), &actions, None).is_err());
        doc.root = UiNode::Column {
            children: vec![UiNode::Empty; MAX_NODES],
        };
        assert!(validate_document(&doc, &request(), &actions, None).is_err());
        doc.root = UiNode::Empty;
        for _ in 0..MAX_DEPTH {
            doc.root = UiNode::Column {
                children: vec![doc.root],
            };
        }
        assert!(validate_document(&doc, &request(), &actions, None).is_err());
        doc.root = UiNode::Text {
            text: "x".repeat(MAX_TEXT_BYTES + 1),
            emphasis: TextEmphasis::Normal,
        };
        assert!(validate_document(&doc, &request(), &actions, None).is_err());
    }

    #[test]
    fn disabled_stale_and_fabricated_actions_are_rejected() {
        let mut doc = document();
        let mut action = PluginUiAction {
            surface: request(),
            revision: 1,
            control_id: "go".into(),
            action: "increment".into(),
            value: None,
        };
        assert!(validate_action(&doc, &action).is_ok());
        action.revision = 0;
        assert!(validate_action(&doc, &action).is_err());
        action.revision = 1;
        if let UiNode::Button { enabled, .. } = &mut doc.root {
            *enabled = false;
        }
        assert!(validate_action(&doc, &action).is_err());
        action.control_id = "forged".into();
        assert!(validate_action(&doc, &action).is_err());
    }

    #[test]
    fn select_only_accepts_declared_values_and_progress_is_finite() {
        let mut doc = document();
        doc.root = UiNode::Select {
            id: "choice".into(),
            value: "a".into(),
            options: vec![SelectOption {
                value: "a".into(),
                label: "A".into(),
            }],
            action: "select".into(),
        };
        let mut action = PluginUiAction {
            surface: request(),
            revision: 1,
            control_id: "choice".into(),
            action: "select".into(),
            value: Some(UiValue::String("b".into())),
        };
        assert!(validate_action(&doc, &action).is_err());
        action.value = Some(UiValue::String("a".into()));
        assert!(validate_action(&doc, &action).is_ok());
        doc.root = UiNode::Progress {
            value: f32::NAN,
            label: None,
        };
        assert!(validate_document(&doc, &request(), &[], None).is_err());
    }
}
