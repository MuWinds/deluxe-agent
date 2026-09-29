//! `apply_patch` — create, update, delete, and move files with a patch.
//!
//! Ported from `local-tool-bridge` (`core/src/tools/codex.rs`). The patch parser
//! is self-contained on purpose: patch semantics should not depend on shell
//! quoting or on a platform-specific `patch` binary being installed.
//!
//! The line-ending handling is the part worth preserving. A patch applied to a
//! CRLF file must write CRLF back, and a hunk must still match when the file's
//! terminators differ from the patch's, or every edit to a Windows file fails.

use std::time::Instant;

use serde_json::{json, Value};

use super::settings::ToolSettings;
use super::{required_str, ObjectSchema, Tool, ToolDescriptor, ToolOutput};
use crate::error::{AgentError, Result};

pub struct ApplyPatch;

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "apply_patch".into(),
            summary: "Create, update, delete, or move files with a patch".into(),
            description: "Applies a file-oriented patch document directly to the filesystem. \
                          Supported operations are Add File, Delete File, Update File, and \
                          Update File with Move to. Relative paths resolve against the \
                          configured working directory."
                .into(),
            guidelines: vec![
                "Edit files with `apply_patch`, never by scripting `exec` around `sed`, \
                 `echo`, or shell redirections."
                    .into(),
                "Read the target file first, so the patch context matches the bytes on disk."
                    .into(),
                "If two changes touch the same block or nearby lines, put them in one patch \
                 rather than two."
                    .into(),
                "Keep matched context as small as possible while still unique — do not pad \
                 hunks with large unchanged regions."
                    .into(),
            ],
            host_validates_arguments: true,
            mutating: true,
            input_schema: ObjectSchema {
                schema_type: "object".into(),
                properties: serde_json::from_value(json!({
                    "patch": {
                        "type": "string",
                        "description": "A patch document beginning with *** Begin Patch and \
                                        ending with *** End Patch",
                    },
                }))
                .expect("schema must be an object"),
                required: vec!["patch".into()],
            },
        }
    }

    async fn execute(&self, arguments: Value, settings: &ToolSettings) -> Result<ToolOutput> {
        let started = Instant::now();
        let patch = required_str(&arguments, "patch")?;
        let changes = parse_patch(&patch)?;
        if changes.is_empty() {
            return Err(AgentError::invalid_params(
                "Patch contains no file operations",
            ));
        }

        let mut applied = Vec::new();
        let mut hunks_resolved = Vec::new();
        for change in changes {
            match change {
                PatchChange::Add { path, content } => {
                    let path = settings.resolve(&path)?;
                    if path.exists() {
                        return Err(AgentError::invalid_params(format!(
                            "Cannot add `{}`: file already exists",
                            path.display()
                        )));
                    }
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|error| {
                            AgentError::from_io("Failed to create patch parent", error)
                        })?;
                    }
                    tokio::fs::write(&path, &content)
                        .await
                        .map_err(|error| AgentError::from_io("Failed to add file", error))?;
                    applied.push(format!("A {}", path.display()));
                    // A new file's lines are its content lines, one per patch
                    // line, numbered from one.
                    hunks_resolved.push(super::HunkLines {
                        path: path.display().to_string(),
                        lines: (1..=content.iter().filter(|b| **b == b'\n').count())
                            .map(Some)
                            .collect(),
                    });
                }
                PatchChange::Delete { path } => {
                    let path = settings.resolve(&path)?;
                    if path.is_dir() {
                        return Err(AgentError::invalid_params(format!(
                            "Cannot delete directory `{}` with apply_patch",
                            path.display()
                        )));
                    }
                    tokio::fs::remove_file(&path)
                        .await
                        .map_err(|error| AgentError::from_io("Failed to delete file", error))?;
                    applied.push(format!("D {}", path.display()));
                }
                PatchChange::Update {
                    path,
                    move_to,
                    hunks,
                } => {
                    let path = settings.resolve(&path)?;
                    let original = tokio::fs::read_to_string(&path).await.map_err(|error| {
                        AgentError::from_io("Failed to read patch target", error)
                    })?;
                    let (updated, positions) = apply_hunks(&original, &hunks)?;
                    let target = if let Some(move_to) = move_to {
                        settings.resolve(&move_to)?
                    } else {
                        path.clone()
                    };
                    if let Some(parent) = target.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|error| {
                            AgentError::from_io("Failed to create patch target parent", error)
                        })?;
                    }
                    tokio::fs::write(&target, updated).await.map_err(|error| {
                        AgentError::from_io("Failed to write patched file", error)
                    })?;
                    if target != path {
                        tokio::fs::remove_file(&path).await.map_err(|error| {
                            AgentError::from_io("Failed to remove moved source", error)
                        })?;
                    }
                    applied.push(format!(
                        "U {}{}",
                        path.display(),
                        if target != path {
                            format!(" -> {}", target.display())
                        } else {
                            String::new()
                        }
                    ));
                    // The positions are the one fact worth keeping from this
                    // execution: where each hunk landed in the file as it was
                    // read. Everything the UI numbers against freezes here.
                    hunks_resolved.push(super::HunkLines {
                        path: path.display().to_string(),
                        lines: hunk_line_numbers(&hunks, &positions),
                    });
                }
            }
        }

        Ok(ToolOutput {
            content: vec![super::ContentBlock::text(format!(
                "Applied {} patch operation(s) in {} ms:\n{}",
                applied.len(),
                started.elapsed().as_millis(),
                applied.join("\n")
            ))],
            is_error: false,
            truncated: false,
            original_bytes: None,
            duration_ms: Some(started.elapsed().as_millis() as u64),
            hunks: hunks_resolved,
        })
    }
}

#[derive(Debug)]
enum PatchChange {
    Add {
        path: String,
        content: Vec<u8>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Vec<String>>,
    },
}

fn parse_patch(patch: &str) -> Result<Vec<PatchChange>> {
    let lines: Vec<&str> = patch.lines().collect();
    if lines.first().copied() != Some("*** Begin Patch")
        || lines.last().copied() != Some("*** End Patch")
    {
        return Err(AgentError::invalid_params(
            "Invalid apply_patch envelope; expected *** Begin Patch / *** End Patch",
        ));
    }

    let mut i = 1;
    let mut changes = Vec::new();
    // `i + 1 < lines.len()` keeps the trailing `*** End Patch` out of the body.
    while i + 1 < lines.len() {
        let header = lines[i];

        if let Some(path) = header.strip_prefix("*** Add File: ") {
            i += 1;
            let mut content = String::new();
            while i + 1 < lines.len() && !lines[i].starts_with("*** ") {
                let line = lines[i];
                if let Some(rest) = line.strip_prefix('+') {
                    content.push_str(rest);
                    content.push('\n');
                } else {
                    return Err(AgentError::invalid_params(
                        "Add File lines must begin with `+`",
                    ));
                }
                i += 1;
            }
            changes.push(PatchChange::Add {
                path: path.trim().into(),
                content: content.into_bytes(),
            });
            continue;
        }

        if let Some(path) = header.strip_prefix("*** Delete File: ") {
            changes.push(PatchChange::Delete {
                path: path.trim().into(),
            });
            i += 1;
            continue;
        }

        if let Some(path) = header.strip_prefix("*** Update File: ") {
            let source = path.trim().to_string();
            i += 1;

            let mut move_to = None;
            if i + 1 < lines.len() && lines[i].starts_with("*** Move to: ") {
                move_to = Some(lines[i]["*** Move to: ".len()..].trim().to_string());
                i += 1;
            }

            let mut hunks = Vec::new();
            let mut current = Vec::new();
            while i + 1 < lines.len() && !lines[i].starts_with("*** ") {
                if lines[i].starts_with("@@") {
                    if !current.is_empty() {
                        hunks.push(std::mem::take(&mut current));
                    }
                } else {
                    current.push(lines[i].to_string());
                }
                i += 1;
            }
            if !current.is_empty() {
                hunks.push(current);
            }

            changes.push(PatchChange::Update {
                path: source,
                move_to,
                hunks,
            });
            continue;
        }

        return Err(AgentError::invalid_params(format!(
            "Unknown apply_patch operation `{header}`"
        )));
    }

    Ok(changes)
}

/// Applies the hunks, and reports where each landed.
///
/// The returned positions are the 1-based line in `original` each hunk's
/// context matched at — the same search the splice uses, so the number a hunk
/// reports is the number it really replaced. The splice mutates the working
/// copy, so a later hunk's match comes back shifted by whatever the earlier
/// ones inserted or removed; the shift is subtracted back out, or the numbers
/// a multi-hunk patch reports would drift past the file it is patching.
fn apply_hunks(original: &str, hunks: &[Vec<String>]) -> Result<(String, Vec<usize>)> {
    let mut lines: Vec<String> = original.split_inclusive('\n').map(str::to_string).collect();
    if !original.is_empty() && !original.ends_with('\n') && lines.is_empty() {
        lines.push(original.to_string());
    }
    let mut positions = Vec::with_capacity(hunks.len());
    let mut shift: i64 = 0;

    for hunk in hunks {
        let old: Vec<String> = hunk
            .iter()
            .filter_map(|line| line.strip_prefix(' ').or_else(|| line.strip_prefix('-')))
            .map(|line| format_with_original_ending(line, original))
            .collect();
        let new: Vec<String> = hunk
            .iter()
            .filter_map(|line| match line.chars().next() {
                Some(' ') | Some('+') => line.get(1..),
                _ => None,
            })
            .map(|line| format_with_original_ending(line, original))
            .collect();

        let position = find_sequence(&lines, &old).ok_or_else(|| {
            AgentError::invalid_params("Patch hunk context did not match the target file")
        })?;
        positions.push(((position + 1) as i64 - shift).max(1) as usize);
        shift += new.len() as i64 - old.len() as i64;
        lines.splice(position..position + old.len(), new);
    }

    Ok((lines.concat(), positions))
}

/// One line number per patch line, against the file as it was read.
///
/// Context and removed lines exist in the original file and show the position
/// they were matched at. An added line shows the number it will occupy once the
/// patch has landed — the deleted slot when it rewrites a `-` line, the
/// insertion point otherwise — and each add in a run numbers on from the last,
/// so a three-line insertion reads `42 43 44` rather than one number three
/// times. `None` goes to the blank separators between hunks, which the patch
/// never numbered.
fn hunk_line_numbers(hunks: &[Vec<String>], positions: &[usize]) -> Vec<Option<usize>> {
    let mut numbers = Vec::new();
    for (hunk, position) in hunks.iter().zip(positions) {
        // `line_no` walks the original file from the hunk's matched start;
        // `next_add` is where the next inserted line will land.
        let mut line_no = *position;
        let mut next_add = *position;
        for line in hunk {
            match line.as_bytes().first() {
                Some(b' ') => {
                    numbers.push(Some(line_no));
                    line_no += 1;
                    next_add = line_no;
                }
                Some(b'-') => {
                    numbers.push(Some(line_no));
                    line_no += 1;
                    next_add = line_no - 1;
                }
                Some(b'+') => {
                    numbers.push(Some(next_add));
                    next_add += 1;
                }
                _ => numbers.push(None),
            }
        }
    }
    numbers
}

fn format_with_original_ending(line: &str, original: &str) -> String {
    if original.contains("\r\n") {
        format!("{}\r\n", line.strip_suffix('\r').unwrap_or(line))
    } else {
        format!("{line}\n")
    }
}

fn find_sequence(lines: &[String], needle: &[String]) -> Option<usize> {
    if needle.is_empty() {
        return Some(lines.len());
    }
    lines.windows(needle.len()).position(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(a, b)| a.trim_end_matches(['\r', '\n']) == b.trim_end_matches(['\r', '\n']))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_patch_without_the_envelope() {
        assert!(parse_patch("*** Add File: a.txt\n+hi").is_err());
    }

    #[test]
    fn parses_an_add_operation() {
        let changes = parse_patch("*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch")
            .expect("patch must parse");
        assert_eq!(changes.len(), 1);
        match &changes[0] {
            PatchChange::Add { path, content } => {
                assert_eq!(path, "a.txt");
                assert_eq!(content, b"hello\n");
            }
            other => panic!("expected an add, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_delete_operation() {
        let changes = parse_patch("*** Begin Patch\n*** Delete File: a.txt\n*** End Patch")
            .expect("patch must parse");
        assert!(matches!(&changes[0], PatchChange::Delete { path } if path == "a.txt"));
    }

    #[test]
    fn parses_an_update_with_move() {
        let patch = "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-old\n+new\n*** End Patch";
        let changes = parse_patch(patch).expect("patch must parse");
        match &changes[0] {
            PatchChange::Update {
                path,
                move_to,
                hunks,
            } => {
                assert_eq!(path, "a.txt");
                assert_eq!(move_to.as_deref(), Some("b.txt"));
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0], vec!["-old", "+new"]);
            }
            other => panic!("expected an update, got {other:?}"),
        }
    }

    #[test]
    fn applies_a_simple_hunk() {
        let (updated, _) = apply_hunks("old\n", &[vec!["-old".into(), "+new".into()]]).unwrap();
        assert_eq!(updated, "new\n");
    }

    #[test]
    fn preserves_crlf_when_the_target_uses_it() {
        let (updated, _) = apply_hunks("old\r\n", &[vec!["-old".into(), "+new".into()]]).unwrap();
        assert_eq!(updated, "new\r\n");
    }

    #[test]
    fn matches_a_hunk_when_only_the_terminators_differ() {
        // The patch says LF, the file is CRLF: the hunk must still land.
        let (updated, _) = apply_hunks("a\r\nold\r\nb\r\n", &[vec!["-old".into(), "+new".into()]])
            .expect("hunk must match despite the terminator difference");
        assert_eq!(updated, "a\r\nnew\r\nb\r\n");
    }

    #[test]
    fn a_hunk_that_does_not_match_is_an_error() {
        assert!(apply_hunks("actual\n", &[vec!["-expected".into()]]).is_err());
    }

    #[test]
    fn an_empty_hunk_appends_at_the_end() {
        let (updated, _) = apply_hunks("a\n", &[vec!["+b".into()]]).unwrap();
        assert_eq!(updated, "a\nb\n");
    }
}
