//! `task` — delegate a job to a plugin-defined sub-agent.
//!
//! A plugin's `agents/*.md` describe roles — "the Figma Implementation Agent",
//! "the Design Parity Review Agent" — each a system prompt for a worker that
//! does one kind of job. This tool is how the model reaches them: it runs the
//! named role in its own conversation and returns the role's final answer as the
//! tool result. See [`crate::plugins::agents`] for the files.
//!
//! # Why the sub-agent's tools exclude `task`
//!
//! The sub-agent is handed the registry *as it stood before `task` itself was
//! registered* — the built-ins, `read_image`, and the project's MCP tools, but
//! not `task`. That snapshot does two things at once. It bounds delegation to a
//! single level, so a badly written role cannot fan out into an unbounded tree
//! of model calls; and it keeps the types honest, because a `Task` that held a
//! registry containing itself would be a cycle that could never be built.
//!
//! # Why the answer is all that comes back
//!
//! A sub-agent's tool calls and reasoning are its own, and its final message
//! becomes the tool result — which keeps the parent's context small and makes
//! the `prompt` the one channel *into* the sub-agent, hence the guideline
//! telling the model to make it self-contained.
//!
//! # Why its events are forwarded anyway
//!
//! "Not in the parent's context" is not "not visible": the window shows a
//! delegated agent's own transcript, so the user can watch what it is doing
//! without paying for its tokens in the parent's conversation. Every event the
//! sub-agent emits is therefore pushed to the window, tagged with the job that
//! owns it — see [`SubagentSink`]. The events still never reach a *session*:
//! their run id is the sub-agent's own and matches no run the window started,
//! so [`crate::app`] folds them into the sub-agent's transcript instead.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::{
    optional_bool, required_str, ObjectSchema, Tool, ToolDescriptor, ToolOutput, ToolRegistry,
    ToolSettings,
};
use crate::agent::{Agent, EventSink, RunRequest};
use crate::context::ContextSettings;
use crate::error::{AgentError, Result};
use crate::ipc::Event;
use crate::llm::{LlmClient, UserTurn};
use crate::plugins::AgentRole;

pub struct Task {
    /// The model the sub-agent runs on. A clone of the parent's client, which
    /// shares the underlying connection pool.
    llm: LlmClient,
    /// The roles resolved for this project, sorted by name.
    roles: Vec<AgentRole>,
    /// The tools a sub-agent may use: the parent's registry, minus `task`.
    registry: Arc<ToolRegistry>,
    /// Shared with the parent, so a settings change reaches a sub-agent too.
    settings: Arc<RwLock<ToolSettings>>,
    /// The project the sub-agent works in, the same as its parent's.
    working_directory: PathBuf,
    context_settings: ContextSettings,
    /// Where a sub-agent's events go so the window can show them. The same sink
    /// the parent's own run uses, so a delegated agent is observable live
    /// rather than only through its final answer.
    forwarder: Arc<dyn EventSink>,
}

impl Task {
    /// Wires a `task` tool to the model client, the roles it may delegate to,
    /// the sub-agent registry it hands them, and the sink their events are
    /// forwarded through.
    pub fn new(
        llm: LlmClient,
        roles: Vec<AgentRole>,
        registry: Arc<ToolRegistry>,
        settings: Arc<RwLock<ToolSettings>>,
        working_directory: PathBuf,
        context_settings: ContextSettings,
        forwarder: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            llm,
            roles,
            registry,
            settings,
            working_directory,
            context_settings,
            forwarder,
        }
    }

    /// The roles as a bullet list, for the tool's description.
    fn catalogue(&self) -> String {
        self.roles
            .iter()
            .map(|role| match &role.description {
                Some(description) => format!("- `{}`: {description}", role.name),
                None => format!("- `{}`", role.name),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait::async_trait]
impl Tool for Task {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "task".into(),
            summary: "Delegate a job to one of a plugin's sub-agents".into(),
            description: format!(
                "Runs one of the sub-agent roles a plugin contributes. The sub-agent works in \
                 its own conversation with its own tools and returns only its final answer, so \
                 the `prompt` must carry everything it needs. Available agents:\n{}",
                self.catalogue()
            ),
            guidelines: vec![
                "Delegate to `task` when one of the listed agents is written for the job at \
                 hand — they encode a plugin's own workflow. Give the sub-agent a complete, \
                 self-contained brief: it cannot see this conversation and cannot ask you a \
                 question."
                    .into(),
            ],
            host_validates_arguments: true,
            mutating: true,
            input_schema: ObjectSchema {
                schema_type: "object".into(),
                properties: serde_json::from_value(json!({
                    "agent": {
                        "type": "string",
                        "description": "The name of the agent to run",
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task for the sub-agent, complete and self-contained",
                    },
                    "runInBackground": {
                        "type": "boolean",
                        "default": false,
                        "description": "Delegate as a background job and return its id at once, \
                                        instead of waiting for the sub-agent to finish",
                    },
                }))
                .expect("schema must be an object"),
                required: vec!["agent".into(), "prompt".into()],
            },
        }
    }

    async fn execute(&self, arguments: Value, _settings: &ToolSettings) -> Result<ToolOutput> {
        let name = required_str(&arguments, "agent")?;
        let prompt = required_str(&arguments, "prompt")?;

        let role = self
            .roles
            .iter()
            .find(|role| role.name == name)
            .ok_or_else(|| {
                let available: Vec<&str> =
                    self.roles.iter().map(|role| role.name.as_str()).collect();
                AgentError::invalid_params(format!(
                    "Unknown agent `{name}`. Available: {}",
                    available.join(", ")
                ))
            })?
            .clone();

        // A background delegation registers the same run as a `subagent` job:
        // the call returns its id at once, and `job_output` collects the
        // sub-agent's answer once it settles. The id is handed to the future
        // rather than known only afterwards, because the sink tags every
        // forwarded event with it — the window routes on that id.
        if optional_bool(&arguments, "runInBackground", false) {
            let label = format!("{name}: {}", first_line(&prompt));
            let llm = self.llm.clone();
            let registry = self.registry.clone();
            let settings = self.settings.clone();
            let working_directory = self.working_directory.clone();
            let context_settings = self.context_settings;
            let forwarder = self.forwarder.clone();

            let job_id = self
                .registry
                .jobs()
                .start_result("subagent", label, move |job_id| {
                    run_delegation(
                        llm,
                        registry,
                        settings,
                        working_directory,
                        context_settings,
                        role,
                        prompt,
                        SubagentSink::live(job_id, forwarder),
                    )
                });
            return Ok(ToolOutput::text(format!(
                "started background job {job_id}\nThe `{name}` agent is running in the \
                 background. Read its answer with job_output(jobId=\"{job_id}\")."
            )));
        }

        let answer = run_delegation(
            self.llm.clone(),
            self.registry.clone(),
            self.settings.clone(),
            self.working_directory.clone(),
            self.context_settings,
            role,
            prompt,
            SubagentSink::silent(),
        )
        .await;

        match answer {
            Ok(answer) => Ok(ToolOutput::text(answer)),
            Err(error) => Ok(ToolOutput::error(format!(
                "The `{name}` agent failed: {error}"
            ))),
        }
    }
}

/// Runs one delegation and returns the sub-agent's answer.
///
/// A shared helper so a foreground call and a background job run the sub-agent
/// the same way: the background path just hands this future to the job
/// registry instead of awaiting it inline.
#[allow(clippy::too_many_arguments)]
async fn run_delegation(
    llm: LlmClient,
    registry: Arc<ToolRegistry>,
    settings: Arc<RwLock<ToolSettings>>,
    working_directory: PathBuf,
    context_settings: ContextSettings,
    role: AgentRole,
    prompt: String,
    sink: SubagentSink,
) -> Result<String> {
    // Announced before the run starts so the window has the whole brief — the
    // job label carries only its first line — and so it arrives ahead of the
    // work it describes. Same task, so the order holds.
    sink.announce(&role.name, &prompt);

    let agent = Agent::for_role(
        llm,
        registry,
        settings,
        working_directory,
        context_settings,
        &role,
    );

    // Run id `0` is never routed anywhere: the sub-agent's events go to the
    // sink below, which forwards them tagged with the job rather than with a
    // run, so nothing keys off this value.
    let outcome = agent
        .run(
            0,
            UserTurn::from(prompt),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await;

    match outcome {
        // A sub-agent that ran but produced nothing is a failed delegation
        // rather than an empty success: an empty answer tells the model nothing
        // about what went wrong.
        Ok(_) if sink.answer().trim().is_empty() => {
            Err(AgentError::internal("the agent finished without an answer"))
        }
        Ok(_) => Ok(sink.answer()),
        Err(error) => Err(error),
    }
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

/// Keeps the last thing the sub-agent said, and — for a background delegation —
/// forwards everything it emits to the window.
///
/// Two jobs in one sink because they are two readings of the same stream. The
/// answer is what the parent's tool call returns; the forwarded events are what
/// the window shows, tagged with the job so they land in that sub-agent's own
/// transcript instead of a session. A foreground delegation has no job and no
/// row to open, so it forwards nothing — as before.
struct SubagentSink {
    /// The job this delegation belongs to and the sink its events go to, or
    /// `None` for a foreground call, which nothing can watch.
    live: Option<(String, Arc<dyn EventSink>)>,
    answer: Mutex<String>,
}

impl SubagentSink {
    /// A sink whose events reach the window, under the job that owns them.
    fn live(job_id: String, forwarder: Arc<dyn EventSink>) -> Self {
        Self {
            live: Some((job_id, forwarder)),
            answer: Mutex::new(String::new()),
        }
    }

    /// A sink for a foreground delegation: it still collects the answer, but
    /// its events have nowhere to go.
    fn silent() -> Self {
        Self {
            live: None,
            answer: Mutex::new(String::new()),
        }
    }

    /// Tells the window which role is running and what it was asked to do.
    fn announce(&self, agent: &str, prompt: &str) {
        if let Some((job_id, forwarder)) = &self.live {
            forwarder.emit(Event::SubagentStarted {
                job_id: job_id.clone(),
                agent: agent.to_string(),
                prompt: prompt.to_string(),
            });
        }
    }

    /// The sub-agent's final answer: the last non-empty assistant turn.
    ///
    /// The *last*, not the first, because a sub-agent that uses tools emits an
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

impl EventSink for SubagentSink {
    fn emit(&self, event: Event) {
        // Read before forwarding, which consumes the event.
        if let Event::AssistantDone { content, .. } = &event {
            if !content.trim().is_empty() {
                if let Ok(mut answer) = self.answer.lock() {
                    *answer = content.clone();
                }
            }
        }

        if let Some((job_id, forwarder)) = &self.live {
            forwarder.emit(Event::Subagent {
                job_id: job_id.clone(),
                event: Box::new(event),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records everything a sub-agent forwards, standing in for the window.
    #[derive(Default)]
    struct Forwarded(Mutex<Vec<Event>>);

    impl Forwarded {
        fn events(&self) -> Vec<Event> {
            self.0.lock().unwrap().clone()
        }
    }

    impl EventSink for Forwarded {
        fn emit(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn role(name: &str, description: Option<&str>) -> AgentRole {
        AgentRole {
            name: name.to_string(),
            description: description.map(str::to_string),
            instructions: format!("You are {name}."),
            plugin: "figma@openai-curated".into(),
            path: PathBuf::from(format!("/plugins/figma/agents/{name}.md")),
        }
    }

    fn task_with(roles: Vec<AgentRole>) -> Task {
        Task::new(
            LlmClient::new("http://localhost:1", "test-model", "key", None).unwrap(),
            roles,
            Arc::new(ToolRegistry::with_builtins()),
            Arc::new(RwLock::new(ToolSettings::default())),
            PathBuf::from("/work"),
            ContextSettings::default(),
            Arc::new(Forwarded::default()),
        )
    }

    #[test]
    fn the_description_lists_every_agent_the_model_may_choose() {
        // The catalogue lives in the tool's own description rather than a
        // hand-written section of the prompt, so it cannot drift from the roles
        // that actually loaded.
        let task = task_with(vec![
            role("figma-implementation-agent", Some("Write the code")),
            role("design-parity-review-agent", None),
        ]);

        let description = task.descriptor().description;

        assert!(
            description.contains("`figma-implementation-agent`"),
            "{description}"
        );
        assert!(description.contains("Write the code"), "{description}");
        assert!(
            description.contains("`design-parity-review-agent`"),
            "{description}"
        );
    }

    #[tokio::test]
    async fn an_unknown_agent_is_rejected_before_any_model_call() {
        // The guard that keeps a hallucinated name from reaching the LLM: the
        // endpoint here does not exist, so a call would fail rather than return
        // the tidy error this asserts.
        let task = task_with(vec![role("figma-implementation-agent", None)]);

        let error = task
            .execute(
                json!({"agent": "no-such-agent", "prompt": "do it"}),
                &ToolSettings::default(),
            )
            .await
            .unwrap_err();

        assert!(error.message.contains("no-such-agent"), "{}", error.message);
        assert!(
            error.message.contains("figma-implementation-agent"),
            "the error lists what is available: {}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_missing_prompt_is_rejected() {
        let task = task_with(vec![role("figma-implementation-agent", None)]);

        let error = task
            .execute(
                json!({"agent": "figma-implementation-agent"}),
                &ToolSettings::default(),
            )
            .await
            .unwrap_err();

        assert!(error.message.contains("prompt"), "{}", error.message);
    }

    #[test]
    fn the_sink_keeps_the_last_non_empty_answer() {
        // A sub-agent emits a turn per tool round; only the final one is its
        // answer, and an empty turn must not overwrite a real one.
        let sink = SubagentSink::silent();
        sink.emit(Event::AssistantDone {
            run_id: 0,
            content: "thinking out loud".into(),
        });
        sink.emit(Event::AssistantDone {
            run_id: 0,
            content: "   ".into(),
        });
        sink.emit(Event::AssistantDone {
            run_id: 0,
            content: "the answer".into(),
        });
        sink.emit(Event::AssistantDelta {
            run_id: 0,
            text: "streamed but unfinished".into(),
        });

        assert_eq!(sink.answer(), "the answer");
    }

    #[test]
    fn a_live_sink_forwards_its_events_under_the_job_that_owns_them() {
        // What the window lives on: the brief first, then the sub-agent's own
        // events, every one of them tagged with the job the row names.
        let forwarder = Arc::new(Forwarded::default());
        let sink = SubagentSink::live("subagent-1".into(), forwarder.clone());

        sink.announce("figma-implementation-agent", "do the thing");
        sink.emit(Event::AssistantDone {
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
                        && agent == "figma-implementation-agent"
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
        let sink = SubagentSink::silent();
        sink.announce("figma-implementation-agent", "do the thing");
        sink.emit(Event::AssistantDone {
            run_id: 0,
            content: "the answer".into(),
        });

        assert_eq!(sink.answer(), "the answer");
    }

    #[test]
    fn the_sub_agent_registry_is_the_parents_without_task() {
        // The property that bounds delegation: `Task` holds a registry that does
        // not contain `Task`. Built the way `main` builds it — clone before
        // registering — so this fails if that order is ever reversed.
        let mut registry = ToolRegistry::with_builtins();
        let sub = Arc::new(registry.clone());
        registry.register(Arc::new(task_with(vec![role("a", None)])));

        assert!(registry.get("task").is_some(), "the parent can delegate");
        assert!(
            sub.get("task").is_none(),
            "a sub-agent cannot delegate again, or delegation would be unbounded"
        );
        assert!(
            sub.get("apply_patch").is_some(),
            "the sub-agent keeps the real tools"
        );
    }

    #[tokio::test]
    async fn a_background_delegation_returns_a_job_id_at_once() {
        // The call must return the job id without waiting for the sub-agent. Its
        // model endpoint does not exist here, so the job fails on its own — which
        // is beside the point of what this asserts.
        let task = task_with(vec![role("figma-implementation-agent", None)]);

        let output = task
            .execute(
                json!({
                    "agent": "figma-implementation-agent",
                    "prompt": "do it",
                    "runInBackground": true,
                }),
                &ToolSettings::default(),
            )
            .await
            .expect("the call returns");

        assert!(!output.is_error, "{}", output.as_text());
        assert!(
            output
                .as_text()
                .contains("started background job subagent-1"),
            "{}",
            output.as_text()
        );
    }
}
