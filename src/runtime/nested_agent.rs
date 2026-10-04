//! Runs one nested agent loop on a Component's behalf.
//!
//! A Component reaches this through the host's `run-agent` capability: it hands
//! over a role's instructions, a prompt, and whether the work should run
//! detached, and gets back either the answer or a job id. The host owns the
//! model client, the tool set, and the event stream, so a delegating Component
//! never needs a registry or a conversation of its own.
//!
//! # Why the tool set is a snapshot
//!
//! The registry here is a clone taken before the delegating Component's own
//! tool was registered, so a nested agent cannot delegate again. That bounds
//! delegation to a single level: a badly written role cannot fan out into an
//! unbounded tree of model calls.
//!
//! # Why its events are forwarded
//!
//! "Not in the parent's context" is not "not visible": the window shows a
//! delegated agent's own transcript, so the user can watch what it is doing
//! without paying for its tokens in the parent's conversation. A detached run
//! therefore forwards every event under the job that owns it; a foreground run
//! has no row to open and forwards nothing.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, RunRequest};
use crate::context::ContextSettings;
use crate::error::{AgentError, Result};
use crate::harness::services::native_services;
use crate::harness::{AgentEvent, AgentEventSink, JobRuntime, NestedAgentRuntime};
use crate::llm::{Message, UserTurn};
use crate::plugins::llm::LlmProvider;
use crate::tools::{ToolRegistry, ToolSettings};

use super::prompt::{build_role_prompt, NativePromptProvider};

/// The request a Component sends through `run-agent`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentRequest {
    /// The role's name, used to label the job and the announcement.
    name: String,
    /// The role's instructions, which become the nested agent's system prompt.
    instructions: String,
    /// The task. The nested agent cannot ask a question, so this must be whole.
    prompt: String,
    /// Run detached and answer with a job id at once, instead of waiting.
    #[serde(default)]
    background: bool,
}

/// The native [`NestedAgentRuntime`]: the parent's model over the parent's
/// tools, minus the delegating tool itself.
pub struct NativeNestedAgent {
    llm: Arc<LlmProvider>,
    registry: Arc<ToolRegistry>,
    settings: Arc<RwLock<ToolSettings>>,
    /// The provider-reported context window, inherited from the parent so a
    /// sub-agent compacts on the same terms.
    context_limit: u64,
    context_settings: ContextSettings,
    project: PathBuf,
    jobs: Arc<dyn JobRuntime>,
    /// Where a detached run's events go so the window can show them.
    sink: Arc<dyn AgentEventSink>,
}

impl NativeNestedAgent {
    /// Wires a nested-agent runner to the parent's model, tools, and jobs.
    // The collaborators all arrive separately because this is assembled once,
    // at project build time; bundling them would only move the list elsewhere.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        llm: Arc<LlmProvider>,
        registry: Arc<ToolRegistry>,
        settings: Arc<RwLock<ToolSettings>>,
        context_limit: u64,
        context_settings: ContextSettings,
        project: PathBuf,
        jobs: Arc<dyn JobRuntime>,
        sink: Arc<dyn AgentEventSink>,
    ) -> Self {
        Self {
            llm,
            registry,
            settings,
            context_limit,
            context_settings,
            project,
            jobs,
            sink,
        }
    }

    /// Registers a detached run as a `subagent` job and returns its id.
    ///
    /// The id is handed to the future rather than known only afterwards,
    /// because the sink tags every forwarded event with it — the window routes
    /// on that id.
    fn start_background(&self, request: AgentRequest) -> String {
        let label = format!("{}: {}", request.name, first_line(&request.prompt));
        let llm = self.llm.clone();
        let registry = self.registry.clone();
        let settings = self.settings.clone();
        let context_limit = self.context_limit;
        let context_settings = self.context_settings;
        let project = self.project.clone();
        let forwarder = self.sink.clone();
        self.jobs.start_result(
            "subagent",
            label,
            Box::new(move |job_id, cancel| {
                Box::pin(run_agent(
                    llm,
                    registry,
                    settings,
                    context_limit,
                    context_settings,
                    project,
                    request,
                    Some((job_id, forwarder)),
                    cancel,
                ))
            }),
        )
    }
}

#[async_trait]
impl NestedAgentRuntime for NativeNestedAgent {
    async fn run(&self, request_json: &str, cancel: &CancellationToken) -> Result<String> {
        let request = parse_request(request_json)?;

        if request.background {
            let job_id = self.start_background(request);
            return Ok(json!({ "jobId": job_id }).to_string());
        }

        let answer = run_agent(
            self.llm.clone(),
            self.registry.clone(),
            self.settings.clone(),
            self.context_limit,
            self.context_settings,
            self.project.clone(),
            request,
            None,
            cancel.clone(),
        )
        .await?;
        Ok(json!({ "answer": answer }).to_string())
    }
}

/// Parses and validates one `run-agent` request.
fn parse_request(request_json: &str) -> Result<AgentRequest> {
    let request: AgentRequest = serde_json::from_str(request_json)
        .map_err(|_| AgentError::invalid_params("Nested agent request must be JSON"))?;
    if request.name.trim().is_empty() {
        return Err(AgentError::invalid_params(
            "Nested agent request needs a name",
        ));
    }
    if request.instructions.trim().is_empty() {
        return Err(AgentError::invalid_params(
            "Nested agent request needs instructions",
        ));
    }
    if request.prompt.trim().is_empty() {
        return Err(AgentError::invalid_params(
            "Nested agent request needs a prompt",
        ));
    }
    Ok(request)
}

/// Runs one nested agent and returns its answer.
///
/// A shared helper so a foreground call and a detached job run the same way:
/// the job path just hands this future to the job registry instead of awaiting
/// it inline.
#[allow(clippy::too_many_arguments)]
async fn run_agent(
    llm: Arc<LlmProvider>,
    registry: Arc<ToolRegistry>,
    settings: Arc<RwLock<ToolSettings>>,
    context_limit: u64,
    context_settings: ContextSettings,
    project: PathBuf,
    request: AgentRequest,
    forward: Option<(String, Arc<dyn AgentEventSink>)>,
    cancel: CancellationToken,
) -> Result<String> {
    let sink = DelegationSink::new(forward);
    // Announced before the run starts so the window has the whole brief — the
    // job label carries only its first line — and so it arrives ahead of the
    // work it describes.
    sink.announce(&request.name, &request.prompt);

    let services = Arc::new(native_services(
        llm,
        registry,
        settings,
        Arc::new(NativePromptProvider::new()),
    ));
    let tools = services.tools.descriptors();
    let system_prompt = build_role_prompt(&request.instructions, &tools);
    let agent = Agent::from_services(
        services,
        project,
        context_limit,
        context_settings,
        system_prompt,
    );
    let history: [Message; 0] = [];
    agent
        .run(
            0,
            UserTurn::from(request.prompt),
            RunRequest {
                history: &history,
                thinking: None,
                carried: None,
            },
            cancel,
            &sink,
        )
        .await?;

    // A nested agent that ran but produced nothing is a failed delegation
    // rather than an empty success: an empty answer tells the model nothing
    // about what went wrong.
    let answer = sink.answer();
    if answer.trim().is_empty() {
        return Err(AgentError::internal("the agent finished without an answer"));
    }
    Ok(answer)
}

/// The first line of a prompt, for a job's one-line label.
fn first_line(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    if line.chars().count() > 80 {
        let truncated: String = line.chars().take(80).collect();
        format!("{truncated}…")
    } else {
        line.to_string()
    }
}

/// Keeps the last thing the nested agent said, and — for a detached run —
/// forwards everything it emits to the window.
///
/// Two jobs in one sink because they are two readings of the same stream. The
/// answer is what the caller's tool call returns; the forwarded events are what
/// the window shows, tagged with the job so they land in that sub-agent's own
/// transcript instead of a session.
struct DelegationSink {
    /// The job this run belongs to and the sink its events go to, or `None` for
    /// a foreground call, which nothing can watch.
    live: Option<(String, Arc<dyn AgentEventSink>)>,
    answer: Arc<Mutex<String>>,
}

impl DelegationSink {
    fn new(live: Option<(String, Arc<dyn AgentEventSink>)>) -> Self {
        Self {
            live,
            answer: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Tells the window which role is running and what it was asked to do.
    fn announce(&self, agent: &str, prompt: &str) {
        if let Some((job_id, forwarder)) = &self.live {
            forwarder.emit(AgentEvent::SubagentStarted {
                job_id: job_id.clone(),
                agent: agent.to_string(),
                prompt: prompt.to_string(),
            });
        }
    }

    /// The nested agent's final answer: the last non-empty assistant turn.
    ///
    /// The *last*, not the first, because an agent that uses tools emits an
    /// assistant turn per round and only the turn that came back without a tool
    /// call is its answer. Empty turns are ignored, so a stray empty one cannot
    /// erase a real answer.
    fn answer(&self) -> String {
        self.answer
            .lock()
            .map(|answer| answer.clone())
            .unwrap_or_default()
    }
}

impl AgentEventSink for DelegationSink {
    fn emit(&self, event: AgentEvent) {
        // Read before forwarding, which consumes the event.
        if let AgentEvent::AssistantDone { content, .. } = &event {
            if !content.trim().is_empty() {
                if let Ok(mut answer) = self.answer.lock() {
                    *answer = content.clone();
                }
            }
        }

        if let Some((job_id, forwarder)) = &self.live {
            forwarder.emit(AgentEvent::Subagent {
                job_id: job_id.clone(),
                event: Box::new(event),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ipc::Event;

    /// Records everything a detached run forwards, standing in for the window.
    #[derive(Default)]
    struct Forwarded(Mutex<Vec<Event>>);

    impl Forwarded {
        fn events(&self) -> Vec<Event> {
            self.0.lock().unwrap().clone()
        }
    }

    impl AgentEventSink for Forwarded {
        fn emit(&self, event: AgentEvent) {
            self.0.lock().unwrap().push(event.into());
        }
    }

    #[test]
    fn a_request_needs_a_name_instructions_and_a_prompt() {
        let parsed = parse_request(r#"{"name":"a","instructions":"i","prompt":"p"}"#)
            .expect("a complete request parses");
        assert!(!parsed.background, "background defaults to false");

        assert!(parse_request("not json").is_err());
        assert!(parse_request(r#"{"name":"","instructions":"i","prompt":"p"}"#).is_err());
        assert!(parse_request(r#"{"name":"a","instructions":"  ","prompt":"p"}"#).is_err());
        assert!(parse_request(r#"{"name":"a","instructions":"i","prompt":""}"#).is_err());

        let background =
            parse_request(r#"{"name":"a","instructions":"i","prompt":"p","background":true}"#)
                .expect("a detached request parses");
        assert!(background.background);
    }

    #[test]
    fn the_sink_keeps_the_last_non_empty_answer() {
        // An agent emits a turn per tool round; only the final one is its
        // answer, and an empty turn must not overwrite a real one.
        let sink = DelegationSink::new(None);
        sink.emit(AgentEvent::AssistantDone {
            run_id: 0,
            content: "thinking out loud".into(),
        });
        sink.emit(AgentEvent::AssistantDone {
            run_id: 0,
            content: "   ".into(),
        });
        sink.emit(AgentEvent::AssistantDone {
            run_id: 0,
            content: "the answer".into(),
        });
        sink.emit(AgentEvent::AssistantDelta {
            run_id: 0,
            text: "streamed but unfinished".into(),
        });

        assert_eq!(sink.answer(), "the answer");
    }

    #[test]
    fn a_live_sink_forwards_its_events_under_the_job_that_owns_them() {
        // What the window lives on: the brief first, then the nested agent's
        // own events, every one of them tagged with the job the row names.
        let forwarder = Arc::new(Forwarded::default());
        let sink = DelegationSink::new(Some(("subagent-1".into(), forwarder.clone())));

        sink.announce("example-agent", "do the thing");
        sink.emit(AgentEvent::AssistantDone {
            run_id: 0,
            content: "the answer".into(),
        });

        let events = forwarder.events();
        assert_eq!(events.len(), 2, "the brief and the turn: {events:?}");
        assert!(
            matches!(
                &events[0],
                Event::SubagentStarted { job_id, agent, prompt }
                    if job_id == "subagent-1"
                        && agent == "example-agent"
                        && prompt == "do the thing"
            ),
            "{:?}",
            events[0]
        );
        assert!(
            matches!(
                &events[1],
                Event::Subagent { job_id, event }
                    if job_id == "subagent-1"
                        && matches!(**event, Event::AssistantDone { .. })
            ),
            "{:?}",
            events[1]
        );
        assert_eq!(
            sink.answer(),
            "the answer",
            "forwarding must not lose the answer"
        );
    }

    #[test]
    fn a_silent_sink_keeps_the_answer_without_forwarding() {
        // A foreground delegation has no job and no row, so nothing is watching
        // it — but its answer is still the tool result.
        let sink = DelegationSink::new(None);
        sink.announce("example-agent", "do the thing");
        sink.emit(AgentEvent::AssistantDone {
            run_id: 0,
            content: "the answer".into(),
        });

        assert_eq!(sink.answer(), "the answer");
    }

    #[test]
    fn the_job_label_is_the_prompts_first_line() {
        assert_eq!(first_line("one\ntwo"), "one");
        let long = "x".repeat(200);
        let label = first_line(&long);
        assert_eq!(label.chars().count(), 81, "capped plus the ellipsis");
        assert!(label.ends_with('…'));
    }
}
