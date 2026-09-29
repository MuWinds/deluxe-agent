//! `exec` — run a command and capture its output.
//!
//! Ported from `local-tool-bridge` (`core/src/tools/shell.rs`). This module adds
//! resource limits (timeout, output cap) and environment hygiene; the one guard
//! left is the destructive-command denylist in [`ToolSettings`], which is
//! checked by the agent loop before a call ever reaches here.
//!
//! One deliberate deviation from the reference: the argument is `command`
//! rather than Codex's `cmd`. This agent is not an MCP bridge for Codex, so
//! there is no schema to stay compatible with, and `command` is the name a
//! model reaches for unaided.
//!
//! # The command is a job
//!
//! Every command is registered with the background [`JobRegistry`] the moment
//! it starts, exactly as the DeepSeek Harness registers every foreground
//! command. That buys two behaviors from one path:
//!
//! * `runInBackground: true` returns the job id at once, and the command runs
//!   on with no execution timeout, to be read with `job_output` and stopped
//!   with `job_kill`.
//! * a foreground command that outlives its `timeoutMs` is **promoted** to the
//!   background rather than killed — the harness's `promoteOnTimeout` — so a
//!   slow build is not lost, and the result names the job it became.
//!
//! A foreground command that finishes inside its budget never exposes the id:
//! its record is dropped, and the model sees the ordinary result.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::jobs::{JobRegistry, JobStatus};
use super::settings::ToolSettings;
use super::{
    clamp_u64, optional_bool, optional_str, optional_u64, required_str, ObjectSchema, Tool,
    ToolDescriptor, ToolOutput,
};
use crate::error::{AgentError, Result};

pub struct Exec {
    /// The runtime a command is registered with. Shared with the job tools and
    /// with `task`, so a job started here is visible to every reader.
    jobs: Arc<JobRegistry>,
}

impl Exec {
    /// Shares the registry `task` and the `job_*` readers use, so a job started
    /// by `exec` is visible to every reader.
    pub fn new(jobs: Arc<JobRegistry>) -> Self {
        Self { jobs }
    }
}

/// Shell backends exposed to the tool caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    #[cfg(windows)]
    PowerShell,
    #[cfg(windows)]
    GitBash,
    #[cfg(windows)]
    Wsl,
    #[cfg(windows)]
    Cmd,
    #[cfg(unix)]
    Sh,
    #[cfg(unix)]
    Bash,
    #[cfg(unix)]
    Zsh,
    #[cfg(unix)]
    Fish,
}

impl ShellKind {
    /// Parses a configured shell name, defaulting to the platform's own.
    ///
    /// The default is `powershell` on Windows and `sh` elsewhere. A name the
    /// platform does not support is an [`AgentError::invalid_params`].
    fn parse(value: Option<&str>) -> Result<Self> {
        let default = if cfg!(windows) { "powershell" } else { "sh" };
        match value.unwrap_or(default) {
            #[cfg(windows)]
            "powershell" | "pwsh" => Ok(Self::PowerShell),
            #[cfg(windows)]
            "gitbash" | "git-bash" | "git_bash" => Ok(Self::GitBash),
            #[cfg(windows)]
            "wsl" => Ok(Self::Wsl),
            #[cfg(windows)]
            "cmd" | "cmd.exe" => Ok(Self::Cmd),
            #[cfg(unix)]
            "sh" => Ok(Self::Sh),
            #[cfg(unix)]
            "bash" => Ok(Self::Bash),
            #[cfg(unix)]
            "zsh" => Ok(Self::Zsh),
            #[cfg(unix)]
            "fish" => Ok(Self::Fish),
            other => Err(AgentError::invalid_params(format!(
                "Unsupported shell `{other}` on this platform"
            ))),
        }
    }

    /// The canonical name this backend is persisted and reported under.
    fn name(self) -> &'static str {
        match self {
            #[cfg(windows)]
            Self::PowerShell => "powershell",
            #[cfg(windows)]
            Self::GitBash => "gitbash",
            #[cfg(windows)]
            Self::Wsl => "wsl",
            #[cfg(windows)]
            Self::Cmd => "cmd",
            #[cfg(unix)]
            Self::Sh => "sh",
            #[cfg(unix)]
            Self::Bash => "bash",
            #[cfg(unix)]
            Self::Zsh => "zsh",
            #[cfg(unix)]
            Self::Fish => "fish",
        }
    }

    /// Builds the program and arguments for this backend.
    fn command_line(self, command: &str) -> Result<(&'static str, Vec<String>)> {
        match self {
            #[cfg(windows)]
            Self::PowerShell => Ok((
                "pwsh.exe",
                vec![
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    command.into(),
                ],
            )),
            #[cfg(windows)]
            Self::GitBash => Ok(("bash.exe", vec!["-lc".into(), command.into()])),
            #[cfg(windows)]
            Self::Wsl => Ok((
                "wsl.exe",
                vec!["-e".into(), "bash".into(), "-lc".into(), command.into()],
            )),
            #[cfg(windows)]
            Self::Cmd => Ok(("cmd.exe", vec!["/C".into(), command.into()])),
            #[cfg(unix)]
            Self::Sh => Ok(("sh", vec!["-lc".into(), command.into()])),
            #[cfg(unix)]
            Self::Bash => Ok(("bash", vec!["-lc".into(), command.into()])),
            #[cfg(unix)]
            Self::Zsh => Ok(("zsh", vec!["-lc".into(), command.into()])),
            #[cfg(unix)]
            Self::Fish => Ok(("fish", vec!["-lc".into(), command.into()])),
        }
    }
}

#[async_trait::async_trait]
impl Tool for Exec {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "exec".into(),
            summary: "Run a shell command and capture its output".into(),
            description: "Executes a command using the configured shell backend. Windows \
                          supports PowerShell, Git Bash, WSL, and cmd.exe; Unix platforms \
                          support sh, bash, zsh, and fish. Each invocation starts a fresh \
                          shell. Set `runInBackground: true` for a long-running command — a \
                          dev server, a watch build — and the call returns a job id at once \
                          instead of waiting. A foreground command that outlives its \
                          `timeoutMs` is moved to the background rather than killed, and the \
                          result names the job. The host caps the captured output and refuses \
                          commands that would destroy data irreversibly."
                .into(),
            guidelines: vec![
                "Each invocation starts a fresh shell: state such as `cd` or exported \
                 variables does not persist between calls, so use absolute paths or chain \
                 commands into one line."
                    .into(),
                "Reach for `exec` when no dedicated tool fits — running the project's tests, \
                 build commands, package managers, and content searches (`rg`)."
                    .into(),
                "Start a long-running command with `runInBackground: true`; it returns a job \
                 id at once and runs with no timeout. Track every job id you start — you are \
                 told in-session when a job finishes, so do not busy-poll — and read a \
                 background job's output with `job_output`, stopping jobs that no longer \
                 matter with `job_kill`."
                    .into(),
            ],
            host_validates_arguments: true,
            mutating: true,
            input_schema: ObjectSchema {
                schema_type: "object".into(),
                properties: serde_json::from_value(json!({
                    "command": { "type": "string", "description": "Command line to execute" },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory; defaults to the configured one",
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": 100,
                        "maximum": 600000,
                        "default": 60000,
                    },
                    "stdin": {
                        "type": "string",
                        "description": "Text piped to the process's stdin",
                    },
                    "runInBackground": {
                        "type": "boolean",
                        "default": false,
                        "description": "Start the command as a background job and return its \
                                        id at once, instead of waiting for it to finish",
                    },
                }))
                .expect("schema must be an object"),
                required: vec!["command".into()],
            },
        }
    }

    /// `exec` bounds itself: its foreground wait ends by promoting the command
    /// to a background job rather than failing, so the host's `default_timeout_ms`
    /// net must not pre-empt it.
    fn bounds_own_timeout(&self) -> bool {
        true
    }

    async fn execute(&self, arguments: Value, settings: &ToolSettings) -> Result<ToolOutput> {
        let started = Instant::now();
        let command = required_str(&arguments, "command")?;

        if command.trim().is_empty() {
            return Err(AgentError::invalid_params("`command` must not be empty"));
        }

        // The one guard left. It refuses rather than asks, because the damage it
        // prevents is the kind the user cannot undo.
        if let Some(reason) = settings.destructive_reason(&command) {
            return Err(AgentError::denied(format!(
                "Command refused by the safety denylist: {reason}. Turn off \
                 `blockDestructiveCommands` in the settings panel to run it anyway."
            )));
        }

        let shell = ShellKind::parse(Some(settings.default_shell.as_str()))?;

        let cwd = match optional_str(&arguments, "cwd") {
            Some(raw) => settings.resolve(&raw)?,
            None => settings.working_directory.clone(),
        };

        let timeout_ms = clamp_u64(
            optional_u64(&arguments, "timeoutMs", settings.default_timeout_ms),
            100,
            600_000,
        );
        let run_in_background = optional_bool(&arguments, "runInBackground", false);

        let (program, args) = shell.command_line(&command)?;
        let mut process = tokio::process::Command::new(program);
        process
            .args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A GUI build has no console, so without this the shell — and anything
        // it starts, `node` included — would flash a console window.
        crate::process::hide_console(&mut process);

        let child = process
            .spawn()
            .map_err(|error| AgentError::from_io("Failed to spawn shell", error))?;

        // The command is a job from this moment, whether the caller asked for
        // the background or is about to wait on it.
        let job_id = self.jobs.start_stream(
            "bash",
            command.clone(),
            child,
            optional_str(&arguments, "stdin"),
        );

        if run_in_background {
            return Ok(ToolOutput::text(format!(
                "started background job {job_id}\n[{}] $ {command}\n  (cwd: {})\n\
                 Read its output with job_output(jobId=\"{job_id}\"); stop it with job_kill.",
                shell.name(),
                cwd.display()
            )));
        }

        let read = self
            .jobs
            .read(&job_id, true, Duration::from_millis(timeout_ms))
            .await?;

        // Still running when the budget ran out: the harness promotes the
        // command instead of killing it, so the work is not lost. The job stays
        // registered and the model reads it on with `job_output`.
        if !read.snapshot.status.is_settled() {
            let output = read.text.trim_end();
            let output = if output.is_empty() {
                "(no output yet)"
            } else {
                output
            };
            return Ok(ToolOutput::text(format!(
                "[still running after {timeout_ms}ms; moved to background job {job_id}]\n\
                 [{}] $ {command}\n  (cwd: {})\n{output}\n\
                 Read more with job_output(jobId=\"{job_id}\"); stop it with job_kill.",
                shell.name(),
                cwd.display()
            )));
        }

        // Finished inside the budget: this is an ordinary foreground result, so
        // the record and any queued completion notice are dropped and the model
        // never sees the id.
        self.jobs.remove(&job_id);

        let exit_code = read.snapshot.exit_code;
        let is_error = match read.snapshot.status {
            JobStatus::Completed => exit_code.map(|code| code != 0).unwrap_or(false),
            _ => true,
        };

        let mut body = String::new();
        body.push_str(&format!("[{}] $ {command}\n", shell.name()));
        body.push_str(&format!("  (cwd: {})\n", cwd.display()));
        body.push_str(&format!(
            "exit: {}\n",
            exit_label(&read.snapshot.status, exit_code)
        ));
        if !read.text.trim().is_empty() {
            body.push('\n');
            body.push_str(read.text.trim_end());
            body.push('\n');
        }

        Ok(ToolOutput {
            content: vec![super::ContentBlock::text(body)],
            is_error,
            truncated: read.truncated,
            original_bytes: Some(read.bytes),
            duration_ms: Some(started.elapsed().as_millis() as u64),
            hunks: Vec::new(),
        })
    }
}

/// How a settled foreground command's exit reads in the result header.
fn exit_label(status: &JobStatus, exit_code: Option<i32>) -> String {
    match status {
        JobStatus::Killed => "terminated (cancelled)".into(),
        JobStatus::Failed => "failed to run".into(),
        _ => match exit_code {
            Some(code) => code.to_string(),
            None => "terminated by signal".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A command that keeps running until it is stopped.
    fn long_command() -> &'static str {
        #[cfg(windows)]
        {
            "ping -n 60 127.0.0.1"
        }
        #[cfg(unix)]
        {
            "sleep 60"
        }
    }

    #[test]
    fn exec_bounds_its_own_timeout() {
        // The host's `default_timeout_ms` net must not pre-empt the promotion.
        let exec = Exec::new(Arc::new(JobRegistry::new()));
        assert!(exec.bounds_own_timeout());
    }

    #[tokio::test]
    async fn run_in_background_returns_a_job_id_at_once() {
        let exec = Exec::new(Arc::new(JobRegistry::new()));
        let settings = ToolSettings::default();

        let output = exec
            .execute(
                json!({ "command": "echo hello", "runInBackground": true }),
                &settings,
            )
            .await
            .expect("the call returns");

        assert!(!output.is_error, "{}", output.as_text());
        assert!(
            output.as_text().contains("started background job bash-1"),
            "the call names the job: {}",
            output.as_text()
        );
    }

    #[tokio::test]
    async fn a_foreground_command_that_finishes_leaves_no_job() {
        let jobs = Arc::new(JobRegistry::new());
        let exec = Exec::new(jobs.clone());
        let settings = ToolSettings::default();

        let output = exec
            .execute(json!({ "command": "echo hi" }), &settings)
            .await
            .expect("the call returns");

        assert!(!output.is_error, "{}", output.as_text());
        assert!(output.as_text().contains("hi"), "{}", output.as_text());
        assert!(
            jobs.list().is_empty(),
            "a command that finished inside its budget must not leave a job record"
        );
    }

    #[tokio::test]
    async fn a_foreground_command_that_outlives_its_timeout_is_promoted() {
        let jobs = Arc::new(JobRegistry::new());
        let exec = Exec::new(jobs.clone());
        let settings = ToolSettings::default();

        let output = exec
            .execute(
                json!({ "command": long_command(), "timeoutMs": 100 }),
                &settings,
            )
            .await
            .expect("the call returns");

        assert!(!output.is_error, "{}", output.as_text());
        assert!(
            output.as_text().contains("moved to background job bash-1"),
            "the result names the job it became: {}",
            output.as_text()
        );

        // The command kept running, so it is listed and can still be stopped.
        let listed = jobs.list();
        assert_eq!(listed.len(), 1, "the promoted job stays registered");
        jobs.kill(&listed[0].id, Some("test cleanup")).unwrap();
    }

    #[test]
    fn parses_supported_shells() {
        #[cfg(windows)]
        {
            assert_eq!(
                ShellKind::parse(Some("powershell")).unwrap(),
                ShellKind::PowerShell
            );
            assert_eq!(
                ShellKind::parse(Some("gitbash")).unwrap(),
                ShellKind::GitBash
            );
            assert_eq!(ShellKind::parse(Some("wsl")).unwrap(), ShellKind::Wsl);
            assert_eq!(ShellKind::parse(Some("cmd")).unwrap(), ShellKind::Cmd);
        }
        #[cfg(unix)]
        {
            assert_eq!(ShellKind::parse(Some("sh")).unwrap(), ShellKind::Sh);
            assert_eq!(ShellKind::parse(Some("bash")).unwrap(), ShellKind::Bash);
            assert_eq!(ShellKind::parse(Some("zsh")).unwrap(), ShellKind::Zsh);
            assert_eq!(ShellKind::parse(Some("fish")).unwrap(), ShellKind::Fish);
        }
    }

    #[test]
    fn rejects_unknown_shell() {
        #[cfg(windows)]
        assert!(ShellKind::parse(Some("fish")).is_err());
        #[cfg(unix)]
        assert!(ShellKind::parse(Some("powershell")).is_err());
    }

    #[test]
    #[cfg(windows)]
    fn builds_gitbash_command() {
        let (program, args) = ShellKind::GitBash.command_line("git status").unwrap();
        assert_eq!(program, "bash.exe");
        assert_eq!(args, vec!["-lc", "git status"]);
    }

    #[test]
    #[cfg(windows)]
    fn builds_wsl_command_with_the_default_distro() {
        let (program, args) = ShellKind::Wsl.command_line("ls -la").unwrap();
        assert_eq!(program, "wsl.exe");
        assert_eq!(args, vec!["-e", "bash", "-lc", "ls -la"]);
    }

    #[test]
    #[cfg(unix)]
    fn builds_unix_command_lines() {
        let (program, args) = ShellKind::Sh.command_line("ls -la").unwrap();
        assert_eq!(program, "sh");
        assert_eq!(args, vec!["-lc", "ls -la"]);
    }
}
