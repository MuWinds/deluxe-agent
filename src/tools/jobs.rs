//! Background jobs — the generic runtime DeepSeek Harness calls `ctx.jobs`.
//!
//! A job is work that outlives the tool call that started it. Two shapes share
//! one registry:
//!
//! * a **stream job** — a process whose stdout/stderr is pumped into a shared
//!   buffer and read incrementally (`exec`);
//! * a **result job** — a future that produces one final piece of text once it
//!   settles (`task`'s background sub-agent).
//!
//! The distinction mirrors the harness's `JobHooks.readOutput`: a job that has
//! one is read as a delta, one that does not is read once, after it settles.
//!
//! # Why the registry is shared
//!
//! Jobs must be reachable from every tool call that follows the one that
//! started them, so the registry lives on [`ToolRegistry`] and is cloned into
//! each tool that needs it. The `task` tool hands its sub-agent the parent's
//! registry snapshot, which shares this same `Arc`, so a delegated job is
//! visible to the parent exactly as if the parent had started it.
//!
//! # Lifetime
//!
//! A process job's child is owned by the task that reads it and is spawned with
//! `kill_on_drop(true)`, so the window closing — which drops the runtime and
//! its tasks — stops every background process, the way the harness stops and
//! awaits a job when its owning composition tears down.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::settings::ToolSettings;
use super::{
    clamp_u64, optional_bool, optional_str, optional_u64, required_str, ObjectSchema, Tool,
    ToolDescriptor, ToolOutput,
};
use crate::error::{AgentError, Result};

/// The cap on what one job keeps in memory, matching `exec`'s foreground cap.
const MAX_CAPTURE_BYTES: usize = 512 * 1024;

/// How long `job_output(wait: true)` waits when no `timeoutMs` is given, and
/// the ceiling a model-supplied one is clamped to.
const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 600_000;

/// The longest label rendered in a list line, so one runaway command cannot
/// blow up the transcript.
const MAX_LABEL_CHARS: usize = 120;

/// How many jobs the registry retains. Once exceeded, the oldest *settled* jobs
/// are dropped — a running job is never dropped. Without this a long-lived
/// window that starts many background jobs would hold every one of their
/// (up to 512 KB) output buffers for the life of the process.
const MAX_JOBS: usize = 64;

pub type JobId = String;

/// How a job ended, or that it has not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    /// Cancellation was requested and the producer is stopping.
    Stopping,
    /// The work finished: a process exited, or a result job produced its text.
    /// A nonzero process exit is still `Completed` — the code is the result,
    /// and the model reads it, exactly as `exec` reports a nonzero exit.
    Completed,
    Killed,
    Failed,
}

impl JobStatus {
    /// Whether the job has reached a terminal state.
    pub fn is_settled(&self) -> bool {
        matches!(self, Self::Completed | Self::Killed | Self::Failed)
    }

    /// The lowercase word the status is printed as in tool output.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Killed => "killed",
            Self::Failed => "failed",
        }
    }
}

/// A read-only projection of one job, handed to `job_list` and the readers.
#[derive(Debug, Clone)]
pub struct JobSnapshot {
    pub id: JobId,
    pub kind: &'static str,
    pub label: String,
    pub status: JobStatus,
    /// Kind-specific status detail, e.g. `exit code: 3`.
    pub detail: Option<String>,
    /// A process job's exit code, absent for result jobs and before exit.
    pub exit_code: Option<i32>,
}

impl JobSnapshot {
    /// The status with its detail, as a list line and a read footer show it.
    pub fn status_line(&self) -> String {
        match &self.detail {
            Some(detail) => format!("{} ({detail})", self.status.label()),
            None => self.status.label().to_string(),
        }
    }
}

/// Output returned by [`JobRegistry::read`].
pub struct JobRead {
    /// A stream job's new bytes since the last read, or a settled result job's
    /// final text. Empty while a result job is still running.
    pub text: String,
    pub snapshot: JobSnapshot,
    /// Whether the producer dropped output to stay within its cap.
    pub truncated: bool,
    /// Total bytes the job has captured so far.
    pub bytes: usize,
}

/// One stream's captured bytes and the cursor a single reader advances.
#[derive(Default)]
struct Stream {
    bytes: Vec<u8>,
    consumed: usize,
    truncated: bool,
}

#[derive(Default)]
struct StreamBuffer {
    stdout: Stream,
    stderr: Stream,
}

/// What a job produces: live bytes, or one final string.
enum JobPayload {
    Stream(Mutex<StreamBuffer>),
    Result(Mutex<Option<String>>),
}

struct Job {
    id: JobId,
    kind: &'static str,
    label: String,
    started_ms: u128,
    /// `watch` so a waiter can be woken the moment the job settles.
    status: watch::Sender<JobStatus>,
    detail: Mutex<Option<String>>,
    exit_code: Mutex<Option<i32>>,
    cancel: CancellationToken,
    /// Set once a terminal state has been reported, so a completion notice is
    /// never emitted twice — a read, a kill and a foreground wait all claim it.
    reported: AtomicBool,
    payload: JobPayload,
}

impl Job {
    /// The job's current status, read through the `watch` channel.
    fn status(&self) -> JobStatus {
        self.status.borrow().clone()
    }

    /// A point-in-time copy of this job for `job_list` and the window.
    fn snapshot(&self) -> JobSnapshot {
        JobSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            label: self.label.clone(),
            status: self.status(),
            detail: self.detail.lock().map(|d| d.clone()).unwrap_or(None),
            exit_code: self.exit_code.lock().ok().and_then(|code| *code),
        }
    }
}

/// The shared job registry.
pub struct JobRegistry {
    /// Registration order, which is what `job_list` shows.
    jobs: Mutex<Vec<Arc<Job>>>,
    next_id: AtomicU64,
    /// Completion notices not yet drained by the agent loop.
    pending: Mutex<Vec<(JobId, String)>>,
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRegistry {
    /// An empty registry: one that has neither started nor recorded a job.
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Registers a spawned process as a stream job and returns its id.
    ///
    /// The child is owned by the reader task from here on; `stdin` is written
    /// and closed before the pumps start, so a command that reads its input to
    /// EOF still sees it.
    pub fn start_stream(
        self: &Arc<Self>,
        kind: &'static str,
        label: impl Into<String>,
        mut child: Child,
        stdin: Option<String>,
    ) -> JobId {
        let job = self.register(
            kind,
            label,
            JobPayload::Stream(Mutex::new(StreamBuffer::default())),
        );
        let registry = Arc::clone(self);
        let reader = Arc::clone(&job);
        let cancel = job.cancel.clone();

        tokio::spawn(async move {
            match stdin {
                Some(input) => {
                    if let Some(mut handle) = child.stdin.take() {
                        use tokio::io::AsyncWriteExt;
                        let _ = handle.write_all(input.as_bytes()).await;
                        let _ = handle.shutdown().await;
                    }
                }
                None => drop(child.stdin.take()),
            }

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let pump_out = pump(stdout, Arc::clone(&reader), StreamSide::Stdout);
            let pump_err = pump(stderr, Arc::clone(&reader), StreamSide::Stderr);
            let wait = async {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = child.start_kill();
                        child.wait().await
                    }
                    status = child.wait() => status,
                }
            };

            let (_, _, status) = tokio::join!(pump_out, pump_err, wait);

            let (final_status, detail, code) = if cancel.is_cancelled() {
                (JobStatus::Killed, Some("cancelled".to_string()), None)
            } else {
                match status {
                    Ok(exit) => {
                        let code = exit.code();
                        let detail = match code {
                            Some(code) => format!("exit code: {code}"),
                            None => "terminated by signal".to_string(),
                        };
                        (JobStatus::Completed, Some(detail), code)
                    }
                    Err(error) => (JobStatus::Failed, Some(error.to_string()), None),
                }
            };

            registry.settle(&reader, final_status, detail, code);
        });

        job.id.clone()
    }

    /// Registers a future as a result job and returns its id.
    ///
    /// The future is *built* by `make` from the job's own id rather than passed
    /// in ready-made, because a result job's work may need to name itself: a
    /// delegated sub-agent tags every event it forwards with the job that owns
    /// it, and it can only do that once the id exists — which is here, and not
    /// before the call. `make` runs synchronously, before the job is spawned, so
    /// the id it is handed is the one this call returns.
    ///
    /// The future produces the job's final text; an error is kept as the
    /// output too, so `job_output` shows why the job failed rather than an
    /// empty body.
    pub fn start_result<F, Fut>(
        self: &Arc<Self>,
        kind: &'static str,
        label: impl Into<String>,
        make: F,
    ) -> JobId
    where
        F: FnOnce(JobId, CancellationToken) -> Fut,
        Fut: std::future::Future<Output = Result<String>> + Send + 'static,
    {
        let job = self.register(kind, label, JobPayload::Result(Mutex::new(None)));
        let registry = Arc::clone(self);
        let runner = Arc::clone(&job);
        let cancel = job.cancel.clone();
        let work = make(job.id.clone(), cancel.clone());

        tokio::spawn(async move {
            let outcome = tokio::select! {
                _ = cancel.cancelled() => Err(AgentError::cancelled()),
                result = work => result,
            };

            let (status, detail, text) = match outcome {
                Ok(text) => (JobStatus::Completed, None, text),
                Err(_error) if cancel.is_cancelled() => (
                    JobStatus::Killed,
                    Some("cancelled".to_string()),
                    String::new(),
                ),
                Err(error) => {
                    let message = error.to_string();
                    (
                        JobStatus::Failed,
                        Some(message.clone()),
                        format!("Error: {message}"),
                    )
                }
            };

            if let JobPayload::Result(slot) = &runner.payload {
                *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(text);
            }
            registry.settle(&runner, status, detail, None);
        });

        job.id.clone()
    }

    /// Allocates an id, records the job and returns it.
    ///
    /// The id is `{kind}-{n}` with `n` from a relaxed counter — uniqueness is
    /// all that is needed, not ordering across threads.
    fn register(
        &self,
        kind: &'static str,
        label: impl Into<String>,
        payload: JobPayload,
    ) -> Arc<Job> {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("{kind}-{n}");
        let (status, _) = watch::channel(JobStatus::Running);
        let job = Arc::new(Job {
            id,
            kind,
            label: label.into(),
            started_ms: now_ms(),
            status,
            detail: Mutex::new(None),
            exit_code: Mutex::new(None),
            cancel: CancellationToken::new(),
            reported: AtomicBool::new(false),
            payload,
        });
        let dropped = {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            jobs.push(Arc::clone(&job));
            prune_jobs(&mut jobs)
        };
        if !dropped.is_empty() {
            // A dropped job's completion notice goes with it: the model must
            // not be told to `job_output` an id that no longer resolves.
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|(id, _)| !dropped.contains(id));
        }
        job
    }

    /// Records a terminal state once, then queues the completion notice.
    ///
    /// First-wins: a late producer outcome cannot overwrite the settlement a
    /// reader already saw. The notice is suppressed when the job was already
    /// reported — a read, a kill or a foreground wait has claimed it.
    fn settle(&self, job: &Arc<Job>, status: JobStatus, detail: Option<String>, code: Option<i32>) {
        if job.status().is_settled() {
            return;
        }

        *job.detail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = detail;
        *job.exit_code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = code;
        job.status.send_replace(status.clone());

        if !job.reported.swap(true, Ordering::SeqCst) {
            let elapsed = now_ms().saturating_sub(job.started_ms) as f64 / 1000.0;
            let notice = format!(
                "background job {} ({}: {}) finished [{}] after {elapsed:.1}s. Read its output \
                 with job_output.",
                job.id,
                job.kind,
                truncate_label(&job.label),
                status.label()
            );
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((job.id.clone(), notice));
        }
    }

    /// The job with `id`, or an [`AgentError::invalid_params`] naming it.
    fn get(&self, id: &str) -> Result<Arc<Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|job| job.id == id)
            .cloned()
            .ok_or_else(|| AgentError::invalid_params(format!("Unknown job `{id}`")))
    }

    /// Reads a job's new output, optionally waiting for it to settle first.
    pub async fn read(&self, id: &str, wait: bool, timeout: Duration) -> Result<JobRead> {
        let job = self.get(id)?;

        if wait {
            let mut rx = job.status.subscribe();
            if !rx.borrow().is_settled() {
                let _ = tokio::time::timeout(timeout, rx.changed()).await;
            }
        }

        let snapshot = job.snapshot();
        // A read of a settled job reports the terminal state, which is what
        // suppresses a duplicate completion notice.
        if snapshot.status.is_settled() {
            job.reported.store(true, Ordering::SeqCst);
        }

        let (text, truncated, bytes) = match &job.payload {
            JobPayload::Stream(buffer) => {
                let mut buffer = buffer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let text = take_delta(&mut buffer);
                let truncated = buffer.stdout.truncated || buffer.stderr.truncated;
                let bytes = buffer.stdout.bytes.len() + buffer.stderr.bytes.len();
                (text, truncated, bytes)
            }
            JobPayload::Result(slot) => {
                let text = if snapshot.status.is_settled() {
                    slot.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone()
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let bytes = text.len();
                (text, false, bytes)
            }
        };

        Ok(JobRead {
            text,
            snapshot,
            truncated,
            bytes,
        })
    }

    /// Snapshots every job, in registration order (oldest first).
    pub fn list(&self) -> Vec<JobSnapshot> {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|job| job.snapshot())
            .collect()
    }

    /// Requests cancellation and returns the job's state at that moment.
    ///
    /// The job settles as `killed` once its work actually stops, which is why
    /// the snapshot returned here usually reads `stopping`.
    pub fn kill(&self, id: &str, reason: Option<&str>) -> Result<JobSnapshot> {
        let job = self.get(id)?;
        if job.status().is_settled() {
            return Ok(job.snapshot());
        }

        // A kill is a report: the caller asked for the end, so a completion
        // notice would be telling it something it already decided.
        job.reported.store(true, Ordering::SeqCst);
        if let Some(reason) = reason {
            tracing::info!(job = id, reason, "job cancellation requested");
        }
        job.cancel.cancel();
        job.status.send_replace(JobStatus::Stopping);
        Ok(job.snapshot())
    }

    /// Drops a job record and any notice it queued.
    ///
    /// Used when a foreground command finished inside its timeout: the result
    /// was returned directly, so the model must not also see the job id or a
    /// notice for it. The notice can already be queued — the job task settles
    /// before the foreground waiter returns — hence the purge.
    pub fn remove(&self, id: &str) {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|job| job.id != id);
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(job_id, _)| job_id != id);
    }

    /// Takes every queued completion notice.
    pub fn drain_notifications(&self) -> Vec<String> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .map(|(_, notice)| notice)
            .collect()
    }
}

/// Drops the oldest settled jobs once `jobs` is over [`MAX_JOBS`], returning
/// the ids it dropped.
///
/// Oldest-first by registration order, which is also the order `job_list`
/// shows, so the jobs the model is most likely to still reference survive.
/// A running job is skipped: it still owns a process or a future.
fn prune_jobs(jobs: &mut Vec<Arc<Job>>) -> Vec<JobId> {
    if jobs.len() <= MAX_JOBS {
        return Vec::new();
    }

    let mut drop_budget = jobs.len() - MAX_JOBS;
    let mut dropped = Vec::new();
    jobs.retain(|job| {
        if drop_budget > 0 && job.status().is_settled() {
            drop_budget -= 1;
            dropped.push(job.id.clone());
            false
        } else {
            true
        }
    });
    dropped
}

/// Which pipe a pump is draining.
#[derive(Clone, Copy)]
enum StreamSide {
    Stdout,
    Stderr,
}

/// Reads one pipe into the job's buffer until EOF or the cap is reached.
async fn pump<R: AsyncRead + Unpin>(reader: Option<R>, job: Arc<Job>, side: StreamSide) {
    let Some(mut reader) = reader else {
        return;
    };
    let mut chunk = vec![0u8; 8192];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let JobPayload::Stream(buffer) = &job.payload else {
                    break;
                };
                let mut buffer = buffer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let stream = match side {
                    StreamSide::Stdout => &mut buffer.stdout,
                    StreamSide::Stderr => &mut buffer.stderr,
                };
                let room = MAX_CAPTURE_BYTES.saturating_sub(stream.bytes.len());
                let take = n.min(room);
                stream.bytes.extend_from_slice(&chunk[..take]);
                if take < n {
                    stream.truncated = true;
                }
            }
        }
    }
}

/// Advances both cursors and returns the new output, stdout before stderr.
///
/// The cursor stops at the last complete UTF-8 character, so a multi-byte
/// character split across two reads is completed by the next one instead of
/// being lost to a lossy conversion.
fn take_delta(buffer: &mut StreamBuffer) -> String {
    let mut out = String::new();
    let stdout = take_stream(&mut buffer.stdout);
    if !stdout.is_empty() {
        out.push_str("--- stdout ---\n");
        out.push_str(&stdout);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    let stderr = take_stream(&mut buffer.stderr);
    if !stderr.is_empty() {
        out.push_str("--- stderr ---\n");
        out.push_str(&stderr);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn take_stream(stream: &mut Stream) -> String {
    if stream.consumed >= stream.bytes.len() {
        return String::new();
    }
    let slice = &stream.bytes[stream.consumed..];
    let valid = match std::str::from_utf8(slice) {
        Ok(text) => text.len(),
        Err(error) => error.valid_up_to(),
    };
    let text = String::from_utf8_lossy(&slice[..valid]).into_owned();
    stream.consumed += valid;
    text
}

fn truncate_label(label: &str) -> String {
    if label.chars().count() <= MAX_LABEL_CHARS {
        return label.to_string();
    }
    let mut truncated: String = label.chars().take(MAX_LABEL_CHARS).collect();
    truncated.push('…');
    truncated
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

fn list_line(snapshot: &JobSnapshot) -> String {
    format!(
        "{} [{}] {} — {}",
        snapshot.id,
        snapshot.kind,
        snapshot.status_line(),
        truncate_label(&snapshot.label)
    )
}

fn schema(properties: Value, required: &[&str]) -> ObjectSchema {
    ObjectSchema {
        schema_type: "object".into(),
        properties: serde_json::from_value(properties)
            .expect("schema properties must be an object"),
        required: required.iter().map(|name| (*name).to_string()).collect(),
    }
}

/// `job_output` — read a job's output, optionally waiting for it.
pub struct JobOutput {
    jobs: Arc<JobRegistry>,
}

impl JobOutput {
    /// Reads against the shared job registry.
    pub fn new(jobs: Arc<JobRegistry>) -> Self {
        Self { jobs }
    }
}

#[async_trait::async_trait]
impl Tool for JobOutput {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "job_output".into(),
            summary: "Read a background job's output".into(),
            description: "Reads what a background job has produced since the last read. A \
                          stream job (a background command) returns only new output; a result \
                          job returns its final text once it has settled. Each read ends with \
                          the job's status. The read is non-blocking unless `wait` is true, in \
                          which case it waits up to `timeoutMs` for the job to finish, leaving \
                          a still-running job alive on timeout."
                .into(),
            guidelines: vec![
                "Track every background job id you start. You are told in-session when a job \
                 finishes, so do not busy-poll. Before your final answer, collect every \
                 still-relevant job with `job_output` — set `wait: true` only when you are \
                 genuinely blocked on it — and `job_kill` the jobs that stopped mattering."
                    .into(),
            ],
            host_validates_arguments: true,
            mutating: false,
            input_schema: schema(
                json!({
                    "jobId": { "type": "string", "description": "The job id to read" },
                    "wait": {
                        "type": "boolean",
                        "default": false,
                        "description": "Block until the job settles, up to `timeoutMs`",
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 600000,
                        "default": 30000,
                    },
                }),
                &["jobId"],
            ),
        }
    }

    async fn execute(&self, arguments: Value, _settings: &ToolSettings) -> Result<ToolOutput> {
        let id = required_str(&arguments, "jobId")?;
        let wait = optional_bool(&arguments, "wait", false);
        let timeout_ms = clamp_u64(
            optional_u64(&arguments, "timeoutMs", DEFAULT_WAIT_MS),
            0,
            MAX_WAIT_MS,
        );

        let read = self
            .jobs
            .read(&id, wait, Duration::from_millis(timeout_ms))
            .await?;

        let mut body = if read.text.trim().is_empty() {
            "(no new output)".to_string()
        } else {
            read.text.trim_end().to_string()
        };
        if read.truncated {
            body.push_str("\n[output truncated]");
        }
        Ok(ToolOutput::text(format!(
            "{body}\n[status: {}]",
            read.snapshot.status_line()
        )))
    }
}

/// `job_list` — every background job, newest last.
pub struct JobList {
    jobs: Arc<JobRegistry>,
}

impl JobList {
    /// Lists against the shared job registry.
    pub fn new(jobs: Arc<JobRegistry>) -> Self {
        Self { jobs }
    }
}

#[async_trait::async_trait]
impl Tool for JobList {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "job_list".into(),
            summary: "List background jobs".into(),
            description: "Lists your background jobs, one per line, with each job's id, kind \
                          and status."
                .into(),
            guidelines: Vec::new(),
            host_validates_arguments: true,
            mutating: false,
            input_schema: schema(json!({}), &[]),
        }
    }

    async fn execute(&self, _arguments: Value, _settings: &ToolSettings) -> Result<ToolOutput> {
        let jobs = self.jobs.list();
        if jobs.is_empty() {
            return Ok(ToolOutput::text("(no background jobs)"));
        }
        let body = jobs.iter().map(list_line).collect::<Vec<_>>().join("\n");
        Ok(ToolOutput::text(body))
    }
}

/// `job_kill` — request cancellation of a job.
pub struct JobKill {
    jobs: Arc<JobRegistry>,
}

impl JobKill {
    /// Cancels against the shared job registry.
    pub fn new(jobs: Arc<JobRegistry>) -> Self {
        Self { jobs }
    }
}

#[async_trait::async_trait]
impl Tool for JobKill {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "job_kill".into(),
            summary: "Stop a background job".into(),
            description: "Requests cancellation of a running background job. The job settles as \
                          killed once its work actually stops."
                .into(),
            guidelines: Vec::new(),
            host_validates_arguments: true,
            mutating: true,
            input_schema: schema(
                json!({
                    "jobId": { "type": "string", "description": "The job id to stop" },
                    "reason": {
                        "type": "string",
                        "description": "Why the job is being stopped; recorded and forwarded",
                    },
                }),
                &["jobId"],
            ),
        }
    }

    async fn execute(&self, arguments: Value, _settings: &ToolSettings) -> Result<ToolOutput> {
        let id = required_str(&arguments, "jobId")?;
        let reason = optional_str(&arguments, "reason");
        let snapshot = self.jobs.kill(&id, reason.as_deref())?;

        let text = if snapshot.status.is_settled() {
            format!(
                "job {} is already finished [{}]",
                snapshot.id,
                snapshot.status_line()
            )
        } else {
            format!(
                "requested cancellation of job {} [{}]",
                snapshot.id,
                snapshot.status_line()
            )
        };
        Ok(ToolOutput::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn registry() -> Arc<JobRegistry> {
        Arc::new(JobRegistry::new())
    }

    /// A job already in a terminal state, for pruning tests that need no task.
    fn settled_job(id: &str) -> Arc<Job> {
        let (status, _) = watch::channel(JobStatus::Completed);
        Arc::new(Job {
            id: id.to_string(),
            kind: "test",
            label: id.to_string(),
            started_ms: 0,
            status,
            detail: Mutex::new(None),
            exit_code: Mutex::new(None),
            cancel: CancellationToken::new(),
            reported: AtomicBool::new(false),
            payload: JobPayload::Result(Mutex::new(None)),
        })
    }

    #[test]
    fn pruning_drops_the_oldest_settled_jobs() {
        let mut jobs: Vec<Arc<Job>> = (0..MAX_JOBS)
            .map(|i| settled_job(&format!("j{i}")))
            .collect();
        jobs.push(settled_job("newest"));

        let dropped = prune_jobs(&mut jobs);

        assert_eq!(dropped, vec!["j0".to_string()], "the oldest job goes first");
        assert_eq!(jobs.len(), MAX_JOBS, "the registry is back at its cap");
        assert!(jobs.iter().any(|job| job.id == "newest"));
    }

    #[test]
    fn pruning_never_drops_a_running_job() {
        let (status, _) = watch::channel(JobStatus::Running);
        let running = Arc::new(Job {
            id: "running".to_string(),
            kind: "test",
            label: "running".to_string(),
            started_ms: 0,
            status,
            detail: Mutex::new(None),
            exit_code: Mutex::new(None),
            cancel: CancellationToken::new(),
            reported: AtomicBool::new(false),
            payload: JobPayload::Result(Mutex::new(None)),
        });
        let mut jobs = vec![running.clone()];
        jobs.extend((0..MAX_JOBS).map(|i| settled_job(&format!("j{i}"))));

        let dropped = prune_jobs(&mut jobs);

        assert_eq!(jobs.len(), MAX_JOBS);
        assert!(
            jobs.iter().any(|job| Arc::ptr_eq(job, &running)),
            "a running job must survive pruning"
        );
        assert!(!dropped.contains(&"running".to_string()));
    }

    /// Runs `script` through the platform shell, the way `exec` does.
    fn spawn(script: &str) -> Child {
        #[cfg(windows)]
        let mut process = {
            let mut process = tokio::process::Command::new("cmd.exe");
            process.args(["/C", script]);
            process
        };
        #[cfg(unix)]
        let mut process = {
            let mut process = tokio::process::Command::new("sh");
            process.args(["-lc", script]);
            process
        };
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        process.spawn().expect("spawn a test process")
    }

    /// A script that keeps running until it is killed.
    fn long_running() -> &'static str {
        #[cfg(windows)]
        {
            "ping -n 60 127.0.0.1 > nul"
        }
        #[cfg(unix)]
        {
            "sleep 60"
        }
    }

    #[tokio::test]
    async fn a_stream_job_reports_its_output_and_exit() {
        let jobs = registry();
        let id = jobs.start_stream("bash", "echo hello", spawn("echo hello"), None);

        let read = jobs
            .read(&id, true, Duration::from_secs(5))
            .await
            .expect("read the job");

        assert!(
            read.snapshot.status.is_settled(),
            "the job should have finished"
        );
        assert_eq!(read.snapshot.exit_code, Some(0));
        assert!(read.text.contains("hello"), "output was: {:?}", read.text);
        assert_eq!(read.snapshot.kind, "bash");
    }

    #[tokio::test]
    async fn a_second_read_returns_only_new_output() {
        let jobs = registry();
        let id = jobs.start_stream("bash", "echo once", spawn("echo once"), None);

        let first = jobs.read(&id, true, Duration::from_secs(5)).await.unwrap();
        assert!(first.text.contains("once"));

        let second = jobs.read(&id, false, Duration::ZERO).await.unwrap();
        assert!(
            second.text.trim().is_empty(),
            "a consumed stream must not repeat: {:?}",
            second.text
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stream_job_writes_its_stdin() {
        let jobs = registry();
        let id = jobs.start_stream("bash", "cat", spawn("cat"), Some("piped\n".into()));

        let read = jobs.read(&id, true, Duration::from_secs(5)).await.unwrap();
        assert!(read.text.contains("piped"), "output was: {:?}", read.text);
    }

    #[tokio::test]
    async fn a_result_job_returns_its_text_once_settled() {
        let jobs = registry();
        let id = jobs.start_result("subagent", "do a thing", |_, _| async {
            Ok("the answer".to_string())
        });

        let read = jobs.read(&id, true, Duration::from_secs(5)).await.unwrap();
        assert_eq!(read.snapshot.status, JobStatus::Completed);
        assert_eq!(read.text, "the answer");
    }

    #[tokio::test]
    async fn a_result_job_is_built_from_its_own_id() {
        // The future is handed the job's id, so a job whose work needs to name
        // itself — a sub-agent tagging the events it forwards — can. The id
        // `make` sees is the one `start_result` returns.
        let jobs = registry();
        let seen = Arc::new(Mutex::new(None));

        let captured = Arc::clone(&seen);
        let id = jobs.start_result("subagent", "self-naming", move |job_id, _| async move {
            *captured.lock().unwrap() = Some(job_id);
            Ok("done".to_string())
        });

        jobs.read(&id, true, Duration::from_secs(5)).await.unwrap();
        assert_eq!(seen.lock().unwrap().as_deref(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn a_failed_result_job_keeps_its_error_as_output() {
        let jobs = registry();
        let id = jobs.start_result("subagent", "fail", |_, _| async {
            Err(AgentError::internal("boom"))
        });

        let read = jobs.read(&id, true, Duration::from_secs(5)).await.unwrap();
        assert_eq!(read.snapshot.status, JobStatus::Failed);
        assert!(read.text.contains("boom"), "output was: {:?}", read.text);
    }

    #[tokio::test]
    async fn killing_a_running_job_settles_it_as_killed() {
        let jobs = registry();
        // A command that would outlive the test unless it is stopped.
        let id = jobs.start_stream("bash", "long", spawn(long_running()), None);

        let requested = jobs.kill(&id, Some("test")).unwrap();
        assert_eq!(requested.status, JobStatus::Stopping);

        let settled = jobs.read(&id, true, Duration::from_secs(5)).await.unwrap();
        assert_eq!(settled.snapshot.status, JobStatus::Killed);
    }

    #[tokio::test]
    async fn completion_is_notified_once() {
        let jobs = registry();
        let id = jobs.start_result("subagent", "quick", |_, _| async { Ok("done".to_string()) });

        jobs.read(&id, true, Duration::from_secs(5)).await.unwrap();

        let first = jobs.drain_notifications();
        assert_eq!(first.len(), 1, "one completion should be queued: {first:?}");
        assert!(
            first[0].contains(&id),
            "the notice names the job: {first:?}"
        );

        // Draining again yields nothing, and a second read does not re-queue.
        assert!(jobs.drain_notifications().is_empty());
        let _ = jobs.read(&id, false, Duration::ZERO).await.unwrap();
        assert!(
            jobs.drain_notifications().is_empty(),
            "a read reports the job"
        );
    }

    #[tokio::test]
    async fn removing_a_job_purges_its_queued_notice() {
        let jobs = registry();
        let id = jobs.start_result("subagent", "quick", |_, _| async { Ok("done".to_string()) });

        jobs.read(&id, true, Duration::from_secs(5)).await.unwrap();
        jobs.remove(&id);

        assert!(jobs.list().is_empty(), "the record is gone");
        assert!(
            jobs.drain_notifications().is_empty(),
            "a foreground wait that removes the job must not leave a notice"
        );
    }

    #[tokio::test]
    async fn listing_reports_each_job_in_registration_order() {
        let jobs = registry();
        let first = jobs.start_result("subagent", "one", |_, _| async { Ok(String::new()) });
        let second = jobs.start_result("subagent", "two", |_, _| async { Ok(String::new()) });

        let listed = jobs.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, first);
        assert_eq!(listed[1].id, second);
        assert_eq!(listed[0].kind, "subagent");
    }

    #[tokio::test]
    async fn an_unknown_job_is_an_error() {
        let jobs = registry();
        assert!(jobs.read("bash-99", false, Duration::ZERO).await.is_err());
        assert!(jobs.kill("bash-99", None).is_err());
    }
}
