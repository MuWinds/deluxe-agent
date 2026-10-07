//! Tool settings: where tools run, how long they may take, and the one guard
//! that is left.
//!
//! There is no sandbox and no approval step — the model runs with full
//! permissions on this machine. What remains here is resource bounding (timeout,
//! output cap) and an optional denylist for commands that destroy data
//! irreversibly, because that is the one mistake a user cannot undo.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{AgentError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolSettings {
    /// Where relative paths resolve, and where `exec` starts when it is not
    /// given its own `cwd`.
    pub working_directory: PathBuf,
    pub default_shell: String,
    pub default_timeout_ms: u64,
    pub max_output_chars: usize,
    /// Refuse commands that would destroy data irreversibly.
    ///
    /// On by default. Set it to `false` for genuinely unrestricted access; it is
    /// the only thing standing between the model and a wiped disk.
    pub block_destructive_commands: bool,
}

fn default_shell() -> String {
    if cfg!(windows) {
        "powershell".into()
    } else {
        "sh".into()
    }
}

fn default_working_directory() -> PathBuf {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            working_directory: default_working_directory(),
            default_shell: default_shell(),
            default_timeout_ms: 60_000,
            max_output_chars: 20_000,
            block_destructive_commands: true,
        }
    }
}

/// Commands refused when `block_destructive_commands` is on, as
/// `(pattern, reason)`.
///
/// The patterns are compiled once by [`destructive_regexes`], not per command:
/// the common case (a safe command) matches none of them, so compiling eagerly
/// on every call was pure overhead on the `exec` path.
const DESTRUCTIVE_PATTERNS: &[(&str, &str)] = &[
    (
        r"(?i)\brm\s+(-[a-z]*\s+)*-[a-z]*[rf]",
        "recursive/forced file deletion (`rm -rf`)",
    ),
    (r"(?i)\bmkfs(\.\w+)?\b", "filesystem formatting"),
    (r"(?i)\bdd\s+.*\bof=/dev/", "raw device writes"),
    (r"(?i)>\s*/dev/(sd|nvme|hd)", "raw device redirection"),
    (r"(?i)\bformat\s+[a-z]:", "Windows volume formatting"),
    (r"(?i)\bdiskpart\b", "Windows disk partitioning"),
    (
        r"(?i)\b(del|erase)\s+.*(/s|/q)",
        "recursive/quiet Windows deletion",
    ),
    (
        r"(?i)\b(shutdown|restart|reboot)\b",
        "system shutdown/restart",
    ),
    (r"(?i):\(\)\s*\{.*:\|:.*\};", "fork bomb"),
];

impl ToolSettings {
    /// Resolves a model-supplied path.
    ///
    /// Absolute paths pass through; relative ones are joined to the working
    /// directory. There is no containment check — that was the sandbox.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf> {
        if raw.trim().is_empty() {
            return Err(AgentError::invalid_params("`path` must not be empty"));
        }

        let expanded = expand_user(raw);
        Ok(if expanded.is_absolute() {
            expanded
        } else {
            self.working_directory.join(expanded)
        })
    }

    /// The denylist reason for a command, if there is one and the guard is on.
    pub fn destructive_reason(&self, command: &str) -> Option<&'static str> {
        if !self.block_destructive_commands {
            return None;
        }

        destructive_regexes()
            .iter()
            .find_map(|(name, regex)| regex.is_match(command).then_some(*name))
    }
}

/// The compiled denylist, built on first use.
///
/// A pattern that fails to compile is logged and skipped: the table is a
/// hard-coded constant, so a failure is a programming error, and refusing to
/// run any command because one entry was malformed would be worse than dropping
/// that entry.
fn destructive_regexes() -> &'static [(&'static str, Regex)] {
    static COMPILED: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        DESTRUCTIVE_PATTERNS
            .iter()
            .filter_map(|(pattern, name)| match Regex::new(pattern) {
                Ok(regex) => Some((*name, regex)),
                Err(error) => {
                    tracing::warn!(pattern, %error, "ignoring an uncompilable denylist pattern");
                    None
                }
            })
            .collect()
    })
}

/// Expands a leading `~` into the user's home directory.
fn expand_user(raw: &str) -> PathBuf {
    let trimmed = raw.trim();

    if trimmed == "~" {
        return default_working_directory();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        return default_working_directory().join(rest);
    }

    Path::new(trimmed).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> ToolSettings {
        ToolSettings {
            working_directory: PathBuf::from("/work"),
            ..Default::default()
        }
    }

    #[test]
    fn absolute_paths_pass_through() {
        let resolved = settings().resolve("/etc/hosts").unwrap();
        assert_eq!(resolved, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn relative_paths_join_the_working_directory() {
        let resolved = settings().resolve("src/main.rs").unwrap();
        assert_eq!(resolved, PathBuf::from("/work").join("src/main.rs"));
    }

    #[test]
    fn parent_segments_are_left_to_the_operating_system() {
        // With no sandbox there is nothing to escape, so `..` is just a path
        // component and the filesystem resolves it.
        let resolved = settings().resolve("../elsewhere").unwrap();
        assert!(resolved.ends_with("elsewhere"));
    }

    #[test]
    fn an_empty_path_is_still_an_error() {
        assert!(settings().resolve("   ").is_err());
    }

    #[test]
    fn the_denylist_catches_rm_rf_and_disk_commands() {
        let settings = settings();
        for command in [
            "rm -rf /tmp/x",
            "rm -fr ./build",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda",
            "diskpart",
            "format c:",
            "shutdown /s /t 0",
            "del /s /q C:\\temp",
        ] {
            assert!(
                settings.destructive_reason(command).is_some(),
                "expected `{command}` to be refused"
            );
        }
    }

    #[test]
    fn the_denylist_leaves_ordinary_commands_alone() {
        let settings = settings();
        for command in ["ls -la", "git status", "cargo test", "rm notes.txt"] {
            assert!(
                settings.destructive_reason(command).is_none(),
                "expected `{command}` to pass"
            );
        }
    }

    #[test]
    fn turning_the_guard_off_lets_everything_through() {
        let settings = ToolSettings {
            block_destructive_commands: false,
            ..Default::default()
        };
        assert!(settings.destructive_reason("rm -rf /").is_none());
    }

    #[test]
    fn every_denylist_pattern_compiles() {
        // A pattern that failed to compile is skipped, silently widening the
        // gap in the guard; the table is a constant, so this must never happen.
        assert_eq!(
            destructive_regexes().len(),
            DESTRUCTIVE_PATTERNS.len(),
            "a denylist pattern failed to compile"
        );
    }
}
