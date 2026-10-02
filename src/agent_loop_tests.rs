//! End-to-end tests for the agent loop, driven by a scripted SSE server.
//!
//! The unit tests in `llm` and `tools` cover the pieces in isolation. What is
//! only testable here is the wiring: that a streamed tool call is reassembled,
//! executed against the real filesystem, fed back as a `tool` message, and that
//! the run stops when the model answers without calling a tool.
//!
//! The server is a raw `TcpListener` rather than a mock of `LlmClient`, because
//! the interesting failures — a fragmented `arguments` string, a body that
//! arrives in several reads — only exist on the wire.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, RunRequest};
use crate::attachments::ImageRef;
use crate::context::ContextSettings;
use crate::error::AgentError;
use crate::harness::services::native_services;
use crate::harness::{AgentEvent, AgentEventSink, HookRuntime, PromptContext};
use crate::ipc::{AuditOutcome, Event};
use crate::llm::{LlmClient, Message};
use crate::plugins::capabilities::CapabilityHub;
use crate::plugins::providers::WasmHookRuntime;
use crate::plugins::wasm_runtime::ComponentActor;
use crate::tools::{to_openai_tools_from_descriptors, ToolRegistry, ToolSettings};

/// Collects everything the agent emits, so a test can assert on the sequence.
#[derive(Default)]
struct CollectingSink {
    events: Mutex<Vec<Event>>,
}

impl CollectingSink {
    fn events(&self) -> Vec<Event> {
        self.events
            .lock()
            .expect("the sink lock is not poisoned")
            .clone()
    }

    /// Every tool outcome, in the order the calls finished.
    fn outcomes(&self) -> Vec<AuditOutcome> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::ToolFinished { outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect()
    }

    /// The concatenation of every streamed text fragment.
    fn transcript(&self) -> String {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::AssistantDelta { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    /// The outcome and result text of the first finished call with this name.
    fn tool_output(&self, name: &str) -> Option<(AuditOutcome, String)> {
        let call_id = self.events().into_iter().find_map(|event| match event {
            Event::ToolStarted {
                call_id,
                name: started,
                ..
            } if started == name => Some(call_id),
            _ => None,
        })?;

        self.events().into_iter().find_map(|event| match event {
            Event::ToolFinished {
                call_id: finished,
                outcome,
                output,
                ..
            } if finished == call_id => Some((outcome, output)),
            _ => None,
        })
    }

    /// Every image any finished call carried, in the order the calls finished.
    fn images(&self) -> Vec<ImageRef> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::ToolFinished { images, .. } => Some(images),
                _ => None,
            })
            .flatten()
            .collect()
    }
}

impl AgentEventSink for CollectingSink {
    fn emit(&self, event: AgentEvent) {
        self.events
            .lock()
            .expect("the sink lock is not poisoned")
            .push(event.into());
    }
}

/// A fake OpenAI-compatible endpoint that replays one scripted body per request.
///
/// Requests are answered in order, so a test scripts the whole conversation up
/// front: turn one, turn two, and so on.
struct FakeServer {
    base_url: String,
    /// The body of every request received, in order. Tests assert on these to
    /// check what the agent actually sent, not merely that something was sent.
    requests: Arc<Mutex<Vec<String>>>,
    /// The indices whose request is answered with a bare 500 instead of its
    /// scripted body. Shared with the accept loop, so a test can arm it after
    /// `start` returns.
    failing: Arc<Mutex<Vec<usize>>>,
}

impl FakeServer {
    /// Starts the server and returns it alongside a task that must stay alive
    /// for the duration of the test.
    async fn start(bodies: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port must be available");
        let port = listener
            .local_addr()
            .expect("the listener has an address")
            .port();

        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = requests.clone();
        let failing: Arc<Mutex<Vec<usize>>> = Arc::default();
        let failure_gate = failing.clone();

        tokio::spawn(async move {
            for body in bodies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                // The whole request must be drained before the socket closes:
                // closing with unread bytes pending resets the connection on
                // Windows, and the client would never see the response.
                let request_body = read_request(&mut socket).await;
                let index = {
                    let mut requests = sink.lock().expect("the request lock is not poisoned");
                    requests.push(request_body);
                    requests.len() - 1
                };
                let failing = failure_gate
                    .lock()
                    .expect("the failure lock is not poisoned")
                    .contains(&index);
                let response = format!(
                    "HTTP/1.1 {}\r\n\
                     Content-Type: text/event-stream\r\n\
                     Cache-Control: no-cache\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    if failing {
                        "500 Internal Server Error"
                    } else {
                        "200 OK"
                    },
                    if failing { 0 } else { body.len() },
                    if failing { "" } else { body.as_str() }
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        Self {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requests,
            failing,
        }
    }

    /// Answers the request at each of `indices` with a 500, and every other
    /// request with its scripted body.
    fn fail(&self, indices: &[usize]) {
        *self
            .failing
            .lock()
            .expect("the failure lock is not poisoned") = indices.to_vec();
    }

    fn request_count(&self) -> usize {
        self.requests
            .lock()
            .expect("the request lock is not poisoned")
            .len()
    }

    /// The body of the `index`-th request (0-based), or `""` if fewer requests
    /// arrived than the test expected — which is itself what the assertion is
    /// about.
    fn request_body(&self, index: usize) -> String {
        self.requests
            .lock()
            .expect("the request lock is not poisoned")
            .get(index)
            .cloned()
            .unwrap_or_default()
    }
}

/// Reads a full HTTP request: the head, then `Content-Length` bytes of body.
///
/// Returns the body: the head is boilerplate, and what the tests want to see is
/// the JSON conversation the agent sent.
async fn read_request(socket: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    let head_end = loop {
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return String::new();
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let declared = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    let mut remaining = declared.saturating_sub(buffer.len() - head_end);
    while remaining > 0 {
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        let take = read.min(remaining);
        buffer.extend_from_slice(&chunk[..take]);
        remaining -= take;
    }

    String::from_utf8_lossy(&buffer[head_end..]).to_string()
}

/// Renders events as an SSE body, terminated the way a provider terminates one.
fn sse(events: &[Value]) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&serde_json::to_string(event).expect("events are serialisable"));
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// A turn that calls one tool, then a turn that answers and stops.
fn two_turns(call: Value, answer: &str) -> Vec<String> {
    vec![
        sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [call] } }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            json!({
                "choices": [],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 },
            }),
        ]),
        sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "content": answer } }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
            json!({
                "choices": [],
                "usage": { "prompt_tokens": 20, "completion_tokens": 3, "total_tokens": 23 },
            }),
        ]),
    ]
}

/// Builds an agent pointed at `server`, with tools rooted at `directory`.
///
/// `context` enables the context-window manager; the tests that do not care
/// pass the default, which is compaction off.
fn agent_for_with_context(
    server: &FakeServer,
    directory: &std::path::Path,
    context: ContextSettings,
) -> Agent {
    agent_with_registry(ToolRegistry::with_builtins(), server, directory, context)
}

/// Builds an agent over an explicit registry, so a test can add `read_image`
/// the way the worker does for a model that declares image input.
fn agent_with_registry(
    registry: ToolRegistry,
    server: &FakeServer,
    directory: &std::path::Path,
    context: ContextSettings,
) -> Agent {
    agent_with_settings(
        registry,
        server,
        directory,
        context,
        ToolSettings {
            working_directory: directory.to_path_buf(),
            ..ToolSettings::default()
        },
    )
}

fn agent_with_settings(
    registry: ToolRegistry,
    server: &FakeServer,
    directory: &std::path::Path,
    context: ContextSettings,
    settings: ToolSettings,
) -> Agent {
    agent_with_settings_and_hooks(
        registry,
        server,
        directory,
        context,
        settings,
        Arc::new(WasmHookRuntime::empty()),
    )
}

fn agent_with_settings_and_hooks(
    registry: ToolRegistry,
    server: &FakeServer,
    directory: &std::path::Path,
    context: ContextSettings,
    settings: ToolSettings,
    hooks: Arc<dyn HookRuntime>,
) -> Agent {
    let client = LlmClient::new(&server.base_url, "test-model", "test-key", None, Some(0))
        .expect("the client builds");
    let mut native = native_services(
        client,
        Arc::new(registry),
        Arc::new(tokio::sync::RwLock::new(settings)),
        Arc::new(crate::runtime::prompt::NativePromptProvider::new()),
    );
    native.hooks = hooks;
    let services = Arc::new(native);
    let prompt_context = PromptContext {
        tools: services.tools.descriptors(),
        skills: Vec::new(),
        agents: Vec::new(),
        project_instructions: crate::runtime::prompt::read_project_instructions(directory),
    };
    let system_prompt = services
        .prompts
        .build_system_prompt(&prompt_context)
        .expect("the native prompt provider renders");
    Agent::from_services(services, directory.to_path_buf(), context, system_prompt)
}

fn agent_for(server: &FakeServer, directory: &std::path::Path) -> Agent {
    agent_for_with_context(server, directory, ContextSettings::default())
}

async fn fixture_hook_runtime(
    project: &std::path::Path,
    settings: &ToolSettings,
) -> Arc<dyn HookRuntime> {
    let manifest = serde_json::from_str::<crate::plugins::PluginManifest>(include_str!(
        "../plugin-fixtures/hooks-provider/plugin.json"
    ))
    .expect("the checked-in fixture manifest is valid")
    .wasm_runtime()
    .expect("the Hooks fixture declares a Wasm runtime");
    std::fs::write(
        project.join(".hooks.json"),
        r#"{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Read",
        "hooks": [
          {
            "type": "command",
            "command": "echo wasm-hook-ran"
          }
        ]
      }
    ]
  }
}"#,
    )
    .expect("the scoped Hooks configuration is writable");
    let host_tools = Arc::new(crate::harness::services::RegistryToolRuntime::new(
        Arc::new(ToolRegistry::with_builtins()),
        Arc::new(tokio::sync::RwLock::new(settings.clone())),
    ));
    let hub = CapabilityHub::new(
        project.to_path_buf(),
        project.to_path_buf(),
        manifest.permissions.clone(),
        host_tools,
    )
    .expect("the fixture host capabilities are valid");
    let actor = ComponentActor::load_bytes(
        include_bytes!("../plugin-fixtures/hooks-provider/plugin.wasm"),
        hub,
    )
    .await
    .expect("the checked-in hook component loads");
    Arc::new(
        WasmHookRuntime::load([("hooky@test".to_string(), actor)])
            .await
            .expect("the fixture hook declaration is valid"),
    )
}

async fn agent_with_fixture_hooks(
    registry: ToolRegistry,
    server: &FakeServer,
    directory: &std::path::Path,
    settings: ToolSettings,
) -> Agent {
    let hooks = fixture_hook_runtime(directory, &settings).await;
    agent_with_settings_and_hooks(
        registry,
        server,
        directory,
        ContextSettings::default(),
        settings,
        hooks,
    )
}

#[tokio::test]
async fn a_run_with_tool_calls_reports_each_turns_usage_as_it_lands() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    std::fs::write(directory.path().join("note.txt"), "hello\n").expect("the file is written");

    // Two requests: the first calls a tool, the second answers. Their usage
    // figures are 10 and 20 prompt tokens respectively.
    let server = FakeServer::start(two_turns(
        json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": { "name": "read_file", "arguments": "{\"path\":\"note.txt\"}" },
        }),
        "done",
    ))
    .await;

    // The window has to be active: `record_usage` deliberately ignores a
    // measurement when no context limit is configured — see
    // `context::tests::a_zero_limit_disables_everything` — so the default
    // settings would sample nothing. The limit is high enough that no
    // compaction fires over these two tiny turns.
    let agent = agent_for_with_context(
        &server,
        directory.path(),
        ContextSettings {
            context_limit: 100_000,
            threshold_percent: 60,
            ..ContextSettings::default()
        },
    );
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "read it".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    // The first turn's sample must arrive *before* the tool ran — that is the
    // whole point: the gauge moves while the run is still working, not after
    // it. The final turn has no tool calls, so the run's own RunFinished is
    // what carries its figure; no sample follows it.
    let events = sink.events();
    let sample_position = events.iter().position(|event| {
        matches!(
            event,
            Event::UsageSampled {
                measurement: Some((10, _)),
                ..
            }
        )
    });
    let tool_position = events
        .iter()
        .position(|event| matches!(event, Event::ToolFinished { .. }));
    let finish_position = events
        .iter()
        .position(|event| matches!(event, Event::RunFinished { .. }));

    let sample_position = sample_position.expect("the tool turn sampled its usage");
    let tool_position = tool_position.expect("the tool ran");
    assert!(
        sample_position < tool_position,
        "the sample must precede the tool call it priced, got: {events:?}"
    );
    assert!(
        finish_position.expect("the run finished") > sample_position,
        "the run finishes after its sample"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::UsageSampled {
                measurement: Some((20, _)),
                ..
            }
        )),
        "the final turn has no tool calls, so it must not sample again, got: {events:?}"
    );
}

#[tokio::test]
async fn a_post_tool_use_hook_runs_and_its_output_joins_the_tool_result() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    std::fs::write(directory.path().join("note.txt"), "hello\n").expect("the file is written");

    let server = FakeServer::start(two_turns(
        json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": { "name": "read_file", "arguments": "{\"path\":\"note.txt\"}" },
        }),
        "done",
    ))
    .await;

    let agent = agent_with_fixture_hooks(
        ToolRegistry::with_builtins(),
        &server,
        directory.path(),
        ToolSettings {
            working_directory: directory.path().to_path_buf(),
            ..ToolSettings::default()
        },
    )
    .await;
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "read it".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    let (outcome, output) = sink.tool_output("read_file").expect("the call finished");
    assert_eq!(outcome, AuditOutcome::Executed);
    assert!(
        output.contains("hello"),
        "the tool's own output survives: {output}"
    );
    assert!(
        output.contains("PostToolUse hook"),
        "the hook is named in the result the model reads: {output}"
    );
    assert!(
        output.contains("wasm-hook-ran"),
        "the Wasm provider's output joins the result: {output}"
    );
}

#[tokio::test]
async fn a_hook_whose_matcher_does_not_match_the_tool_stays_out() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    std::fs::write(directory.path().join("note.txt"), "hello\n").expect("the file is written");

    let server = FakeServer::start(two_turns(
        json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": { "name": "list_dir", "arguments": "{\"path\":\".\"}" },
        }),
        "done",
    ))
    .await;

    let agent = agent_with_fixture_hooks(
        ToolRegistry::with_builtins(),
        &server,
        directory.path(),
        ToolSettings {
            working_directory: directory.path().to_path_buf(),
            ..ToolSettings::default()
        },
    )
    .await;
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "read it".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    let (_, output) = sink.tool_output("list_dir").expect("the call finished");
    assert!(
        !output.contains("wasm-hook-ran"),
        "a Wasm hook must not run for a tool it does not match: {output}"
    );
}

#[tokio::test]
async fn a_refused_call_does_not_fire_its_hook() {
    let directory = tempfile::tempdir().expect("a temp directory is available");

    let server = FakeServer::start(two_turns(
        json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": { "name": "exec", "arguments": "{\"command\":\"rm -rf /\"}" },
        }),
        "stopping there",
    ))
    .await;

    let agent = agent_with_fixture_hooks(
        ToolRegistry::with_builtins(),
        &server,
        directory.path(),
        ToolSettings {
            working_directory: directory.path().to_path_buf(),
            ..ToolSettings::default()
        },
    )
    .await;
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "delete everything".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    let (outcome, output) = sink.tool_output("exec").expect("the call finished");
    assert_eq!(
        outcome,
        AuditOutcome::Denied,
        "the guard must have refused it"
    );
    assert!(
        !output.contains("wasm-hook-ran"),
        "a refused call is not a tool use, so its Wasm hook must not describe one: {output}"
    );
}

/// A small valid PNG, built with the same encoder `read_image` re-encodes with.
fn png_fixture(width: u32, height: u32) -> Vec<u8> {
    crate::image_ops::encode_png(&crate::image_ops::Raster {
        width,
        height,
        rgba: vec![200u8; (width * height * 4) as usize],
    })
    .expect("the fixture encodes")
}

#[tokio::test]
async fn the_system_prompt_carries_the_project_context() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "Always answer in Chinese.\n",
    )
    .expect("the fixture is written");

    let server = FakeServer::start(vec![sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "content": "好的" } }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
    ])])
    .await;

    let agent = agent_for(&server, directory.path());
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "hi".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    let body = server.request_body(0);
    assert!(
        // The body arrives as raw JSON, so the quotes around the path are
        // escaped on the wire; asserting on the tag name, the file name, and
        // the content side-steps the escaping without weakening the check.
        body.contains("project_instructions")
            && body.contains("AGENTS.md")
            && body.contains("Always answer in Chinese."),
        "the project context must reach the model, got: {body}"
    );
}

#[tokio::test]
async fn the_runtime_context_rides_as_a_message_and_is_not_repeated() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    let server = FakeServer::start(vec![answered(10, "ok"), answered(10, "again")]).await;
    let agent = agent_for(&server, directory.path());

    // First run: the history is empty, so the environment block is appended
    // after the prompt and recorded as a host message.
    let sink = CollectingSink::default();
    let _ = agent
        .run(
            1,
            "hi".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the first run completes");

    let first = server.request_body(0);
    assert!(
        first.contains(crate::runtime_context::TAG) && first.contains("working_directory:"),
        "the environment block must reach the model, got: {first}"
    );
    assert!(
        sink.events().iter().any(|event| matches!(
            event,
            Event::Notice { text, .. } if text.starts_with(crate::runtime_context::TAG)
        )),
        "the block must be recorded as a host message, got: {:?}",
        sink.events()
    );

    // Second run carrying that same block in its history: it must not be
    // appended again, or the conversation would grow for no new information.
    let block = crate::runtime_context::render(directory.path());
    let history = vec![
        Message::user("hi"),
        Message::user(block),
        Message::assistant("ok".into(), Vec::new()),
    ];
    let sink = CollectingSink::default();
    let _ = agent
        .run(
            2,
            "more".into(),
            RunRequest {
                history: &history,
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the second run completes");

    assert!(
        !sink
            .events()
            .iter()
            .any(|event| matches!(event, Event::Notice { .. })),
        "an unchanged block must not be re-sent, got: {:?}",
        sink.events()
    );
    assert_eq!(
        server
            .request_body(1)
            .matches(crate::runtime_context::TAG)
            .count(),
        1,
        "the block must appear exactly once in the second request"
    );
}

#[tokio::test]
async fn a_chosen_thinking_level_reaches_the_request() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    let server = FakeServer::start(vec![answered(10, "ok")]).await;

    let agent = agent_for(&server, directory.path());
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "hi".into(),
            RunRequest {
                history: &[],
                thinking: Some(crate::llm::ThinkingLevel::High),
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    let body = server.request_body(0);
    assert!(
        body.contains(r#""reasoning_effort":"high""#),
        "the chosen level must go out on the wire, got: {body}"
    );
}

#[tokio::test]
async fn no_thinking_level_sends_no_reasoning_effort() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    let server = FakeServer::start(vec![answered(10, "ok")]).await;

    let agent = agent_for(&server, directory.path());
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "hi".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    let body = server.request_body(0);
    assert!(
        !body.contains("reasoning_effort"),
        "an unset level must leave the parameter out, got: {body}"
    );
}

/// A turn that answers and reports the prompt size the provider measured.
fn answered(prompt_tokens: u64, answer: &str) -> String {
    sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "content": answer } }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
        json!({
            "choices": [],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": 3,
                "total_tokens": prompt_tokens + 3,
            },
        }),
    ])
}

#[tokio::test]
async fn a_completion_retries_provider_errors_and_returns_the_successful_answer() {
    let provider_error = json!({ "error": { "message": "temporary failure" } }).to_string();
    let answer = json!({
        "choices": [{
            "message": { "content": "recovered" },
            "finish_reason": "stop",
        }],
    })
    .to_string();
    let server = FakeServer::start(vec![provider_error, answer]).await;
    let client = LlmClient::new(&server.base_url, "test-model", "test-key", None, Some(1))
        .expect("the client builds");

    let turn = client
        .complete_turn(
            &[Message::user("summarize")],
            &json!([]),
            &CancellationToken::new(),
        )
        .await
        .expect("the retry returns the successful answer");

    assert_eq!(turn.content, "recovered");
    assert_eq!(server.request_count(), 2, "one retry means two attempts");
}

#[tokio::test]
async fn a_completion_stops_after_the_configured_number_of_retries() {
    let server = FakeServer::start(vec![String::new(), String::new(), String::new()]).await;
    server.fail(&[0, 1, 2]);
    let client = LlmClient::new(&server.base_url, "test-model", "test-key", None, Some(2))
        .expect("the client builds");

    let result = client
        .complete_turn(
            &[Message::user("summarize")],
            &json!([]),
            &CancellationToken::new(),
        )
        .await;

    assert!(result.is_err(), "every scripted HTTP response is an error");
    assert_eq!(
        server.request_count(),
        3,
        "two retries allow the initial attempt plus two retries"
    );
}

#[tokio::test]
async fn a_stream_retry_discards_partial_output_before_emitting_the_answer() {
    let partial = format!(
        "data: {}\n\n",
        json!({ "choices": [{ "delta": { "content": "partial" } }] })
    );
    let server = FakeServer::start(vec![partial, answered(3, "complete")]).await;
    let client = LlmClient::new(&server.base_url, "test-model", "test-key", None, Some(1))
        .expect("the client builds");
    let mut fragments = Vec::new();

    let turn = client
        .stream_turn(
            &[Message::user("answer")],
            &json!([]),
            None,
            &CancellationToken::new(),
            |fragment| match fragment {
                crate::llm::StreamFragment::Reset => fragments.push("reset".to_string()),
                crate::llm::StreamFragment::Reasoning(text) => {
                    fragments.push(format!("reasoning:{text}"));
                }
                crate::llm::StreamFragment::Content(text) => {
                    fragments.push(format!("content:{text}"));
                }
            },
        )
        .await
        .expect("the second stream completes");

    assert_eq!(turn.content, "complete");
    assert_eq!(
        fragments,
        vec!["content:partial", "reset", "content:complete"],
        "failed-attempt output is cleared before the retry's output"
    );
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn an_unlimited_completion_keeps_retrying_past_the_default_limit() {
    let failures = (0..4).map(|_| json!({ "error": { "message": "maintenance" } }).to_string());
    let answer = json!({
        "choices": [{
            "message": { "content": "service restored" },
            "finish_reason": "stop",
        }],
    })
    .to_string();
    let server = FakeServer::start(failures.chain([answer]).collect()).await;
    let client = LlmClient::new(&server.base_url, "test-model", "test-key", None, None)
        .expect("the client builds");

    let turn = client
        .complete_turn(
            &[Message::user("summarize")],
            &json!([]),
            &CancellationToken::new(),
        )
        .await
        .expect("unlimited retries continue until the service recovers");

    assert_eq!(turn.content, "service restored");
    assert_eq!(
        server.request_count(),
        5,
        "four failures do not exhaust retries"
    );
}

#[tokio::test]
async fn an_unlimited_completion_stops_retrying_when_cancelled() {
    let server = FakeServer::start(vec![String::new()]).await;
    server.fail(&[0]);
    let client = LlmClient::new(&server.base_url, "test-model", "test-key", None, None)
        .expect("the client builds");
    let cancel = CancellationToken::new();
    let request_cancel = cancel.clone();
    let request = tokio::spawn(async move {
        client
            .complete_turn(&[Message::user("summarize")], &json!([]), &request_cancel)
            .await
    });

    while server.request_count() == 0 {
        tokio::task::yield_now().await;
    }
    cancel.cancel();

    let error = tokio::time::timeout(std::time::Duration::from_secs(2), request)
        .await
        .expect("cancellation ends an unlimited retry loop")
        .expect("the request task does not panic")
        .expect_err("the cancelled request does not return a completion");
    assert_eq!(error.code, crate::error::code::CANCELLED);
    assert_eq!(
        server.request_count(),
        1,
        "no retry starts after cancellation"
    );
}

#[tokio::test]
async fn a_stream_waiting_for_response_headers_stops_when_cancelled() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port must be available");
    let port = listener
        .local_addr()
        .expect("the listener has an address")
        .port();
    let (accepted_tx, accepted_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("the client connects");
        read_request(&mut socket).await;
        let _ = accepted_tx.send(());
        let _ = release_rx.await;
    });
    let client = LlmClient::new(
        format!("http://127.0.0.1:{port}/v1"),
        "test-model",
        "test-key",
        None,
        None,
    )
    .expect("the client builds");
    let cancel = CancellationToken::new();
    let request_cancel = cancel.clone();
    let request = tokio::spawn(async move {
        client
            .stream_turn(
                &[Message::user("answer")],
                &json!([]),
                None,
                &request_cancel,
                |_| {},
            )
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), accepted_rx)
        .await
        .expect("the server receives the request")
        .expect("the server signals the accepted request");
    cancel.cancel();

    let error = tokio::time::timeout(std::time::Duration::from_secs(2), request)
        .await
        .expect("cancellation ends the request before response headers arrive")
        .expect("the request task does not panic")
        .expect_err("the cancelled stream does not return a turn");
    assert_eq!(error.code, crate::error::code::CANCELLED);
    let _ = release_tx.send(());
    server.await.expect("the local server task exits");
}

#[tokio::test]
async fn crossing_the_threshold_compacts_the_history_before_the_next_turn() {
    let directory = tempfile::tempdir().expect("a temp directory is available");

    // Run one measures 90 tokens against a limit of 100 with a trigger of 60%,
    // so the first request of run two must go out compacted: a summary request
    // over the folded prefix, then the real turn carrying the brief plus the
    // recent tail verbatim.
    let server = FakeServer::start(vec![
        answered(90, "first answer"),
        // The summariser is a non-streaming call, so its reply is one plain
        // JSON body rather than an SSE stream.
        serde_json::to_string(&json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "the user asked about files" },
                "finish_reason": "stop",
            }],
        }))
        .expect("the summary reply is serialisable"),
        answered(70, "second answer"),
    ])
    .await;

    let agent = agent_for_with_context(
        &server,
        directory.path(),
        ContextSettings {
            context_limit: 100,
            threshold_percent: 60,
            ..ContextSettings::default()
        },
    );

    // Run one: a bare prompt. It hands back the measurement run two seeds its
    // window with — the window itself lives on the frame, so a fresh run
    // starts blank and would never notice it is over the limit.
    let sink = CollectingSink::default();
    let carried = agent
        .run(
            1,
            "list files".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the first run completes");

    // Run two: the follow-up, continuing the conversation run one started.
    let history = vec![
        Message::user("list files"),
        Message::assistant("first answer".into(), Vec::new()),
        Message::user("and the second thing"),
        Message::assistant("second answer".into(), Vec::new()),
        Message::user("and a third"),
        Message::assistant("third answer".into(), Vec::new()),
    ];
    let sink = CollectingSink::default();
    let _ = agent
        .run(
            2,
            "hello again".into(),
            RunRequest {
                history: &history,
                thinking: None,
                carried,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the follow-up completes");

    let events = sink.events();
    // Four older messages fold — everything up to "and a third" — while the
    // last user turn, its answer, this run's prompt, and the runtime-context
    // block are kept.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::CompactionStarted { dropping: 4, .. })),
        "compaction must be announced, got: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Compacted { summary, .. } if summary.contains("the user asked about files")
        )),
        "the summary must be surfaced to the user, got: {events:?}"
    );

    // Request 0 is run one's turn, request 1 the summariser, request 2 the
    // compacted turn. The summariser gets only the folded prefix: the kept tail
    // is not paid for twice, and the prefix is byte-identical to the start of
    // the original request, so the provider still serves it from cache.
    let summary_request = server.request_body(1);
    assert!(
        summary_request.contains("list files") && summary_request.contains("and the second thing"),
        "the summariser must be given the folded prefix, got: {summary_request}"
    );
    assert!(
        !summary_request.contains("and a third") && !summary_request.contains("hello again"),
        "the kept tail must not be folded, got: {summary_request}"
    );

    let second = server.request_body(2);
    assert!(
        second.contains("Here is a summary of our conversation so far")
            && second.contains("the user asked about files")
            && second.contains("Continue from where it left off"),
        "the compacted request must carry the brief and a turn to answer, got: {second}"
    );
    assert!(
        second.contains("and a third")
            && second.contains("third answer")
            && second.contains("hello again"),
        "the kept tail must ride along verbatim, got: {second}"
    );
    assert!(
        !second.contains("list files") && !second.contains("first answer"),
        "the folded turns must not survive verbatim, got: {second}"
    );
}

#[tokio::test]
async fn a_failed_summary_request_still_lets_the_run_finish() {
    let directory = tempfile::tempdir().expect("a temp directory is available");

    // Request 1 — the summariser — is answered with a bare 500; the run must
    // shrug it off and send the follow-up with its history intact.
    let server = FakeServer::start(vec![
        answered(95, "first answer"),
        String::new(),
        answered(95, "second answer"),
    ])
    .await;
    server.fail(&[1]);

    let agent = agent_for_with_context(
        &server,
        directory.path(),
        ContextSettings {
            context_limit: 100,
            threshold_percent: 60,
            ..ContextSettings::default()
        },
    );

    let sink = CollectingSink::default();
    let carried = agent
        .run(
            1,
            "list files".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the first run completes");

    // Six messages: more than the recent window keeps, so there is something
    // to summarise.
    let history = vec![
        Message::user("list files"),
        Message::assistant("first answer".into(), Vec::new()),
        Message::user("and the second thing"),
        Message::assistant("second answer".into(), Vec::new()),
        Message::user("and a third"),
        Message::assistant("third answer".into(), Vec::new()),
    ];
    let sink = CollectingSink::default();
    let _ = agent
        .run(
            2,
            "hello again".into(),
            RunRequest {
                history: &history,
                thinking: None,
                carried,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes despite the failed summary");

    assert!(
        sink.events()
            .iter()
            .any(|event| matches!(event, Event::Compacted { summary, .. } if summary.is_empty())),
        "the failure must be surfaced as an empty summary, got: {:?}",
        sink.events()
    );
    // The follow-up went out full-length: nothing was dropped.
    assert!(
        server.request_body(2).contains("list files"),
        "the original history must survive a failed compaction, got: {}",
        server.request_body(2)
    );
}

#[tokio::test]
async fn without_a_limit_the_history_is_never_touched() {
    let directory = tempfile::tempdir().expect("a temp directory is available");

    let server = FakeServer::start(vec![
        answered(999_999, "first answer"),
        answered(999_999, "second answer"),
    ])
    .await;

    let agent = agent_for(&server, directory.path());

    let sink = CollectingSink::default();
    let carried = agent
        .run(
            1,
            "list files".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the first run completes");

    // Six messages: more than the recent window keeps, so there is something
    // to summarise.
    let history = vec![
        Message::user("list files"),
        Message::assistant("first answer".into(), Vec::new()),
        Message::user("and the second thing"),
        Message::assistant("second answer".into(), Vec::new()),
        Message::user("and a third"),
        Message::assistant("third answer".into(), Vec::new()),
    ];
    let sink = CollectingSink::default();
    let _ = agent
        .run(
            2,
            "hello again".into(),
            RunRequest {
                history: &history,
                thinking: None,
                carried,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the follow-up completes");

    assert!(
        !sink
            .events()
            .iter()
            .any(|event| matches!(event, Event::CompactionStarted { .. })),
        "no limit means no compaction"
    );
    assert!(
        server.request_body(1).contains("list files"),
        "the follow-up must carry the whole history, got: {}",
        server.request_body(1)
    );
}

#[tokio::test]
async fn a_streamed_tool_call_is_executed_and_fed_back() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    std::fs::write(directory.path().join("note.txt"), "42\n").expect("the fixture is written");

    // `arguments` is split across two deltas on purpose: that is the shape that
    // breaks an implementation which parses each chunk as it arrives, because
    // the string is not valid JSON until the second fragment lands.
    let server = FakeServer::start(vec![
        sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                "index": 0,
                "id": "call_1",
                "type": "function",
                "function": { "name": "read_file", "arguments": "{\"path\":\"no" },
            }] } }] }),
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{
                "index": 0,
                "function": { "arguments": "te.txt\"}" },
            }] } }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
        ]),
        sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "content": "the file says 42" } }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
            json!({
                "choices": [],
                "usage": { "prompt_tokens": 20, "completion_tokens": 3, "total_tokens": 23 },
            }),
        ]),
    ])
    .await;

    let agent = agent_for(&server, directory.path());
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "read note.txt".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    let (outcome, output) = sink
        .tool_output("read_file")
        .expect("read_file was called and finished");
    assert_eq!(outcome, AuditOutcome::Executed, "got: {output}");
    assert!(
        output.contains("42"),
        "the file body must reach the model: {output}"
    );
    assert_eq!(sink.outcomes(), vec![AuditOutcome::Executed]);
    assert_eq!(sink.transcript(), "the file says 42");
}

#[tokio::test]
async fn a_refused_command_is_reported_as_a_refusal() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    let server = FakeServer::start(two_turns(
        json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": { "name": "exec", "arguments": "{\"command\":\"rm -rf /\"}" },
        }),
        "stopping there",
    ))
    .await;

    let agent = agent_for(&server, directory.path());
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "wipe everything".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    let (outcome, output) = sink.tool_output("exec").expect("exec was called");
    assert_eq!(
        outcome,
        AuditOutcome::Denied,
        "a refused command must be recorded as a refusal, not a failure"
    );
    assert!(
        output.starts_with("Refused:"),
        "the model must be told this was a refusal, not a failure: {output}"
    );
    assert_eq!(sink.outcomes(), vec![AuditOutcome::Denied]);
}

#[tokio::test]
async fn turning_the_guard_off_lets_a_destructive_command_through() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    std::fs::write(directory.path().join("doomed.txt"), "bye\n").expect("the fixture is written");

    let server = FakeServer::start(two_turns(
        json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": { "name": "exec", "arguments": "{\"command\":\"rm doomed.txt\"}" },
        }),
        "deleted",
    ))
    .await;

    let settings = ToolSettings {
        working_directory: directory.path().to_path_buf(),
        block_destructive_commands: false,
        ..ToolSettings::default()
    };
    let agent = agent_with_settings(
        ToolRegistry::with_builtins(),
        &server,
        directory.path(),
        ContextSettings::default(),
        settings,
    );
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "delete it".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    assert_eq!(sink.outcomes(), vec![AuditOutcome::Executed]);
    assert!(
        !directory.path().join("doomed.txt").exists(),
        "the command must actually have run"
    );
}

#[tokio::test]
async fn an_unknown_tool_is_a_failure_not_a_crash() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    let server = FakeServer::start(two_turns(
        json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": { "name": "launch_missiles", "arguments": "{}" },
        }),
        "understood",
    ))
    .await;

    let agent = agent_for(&server, directory.path());
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "do something".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    let (outcome, output) = sink
        .tool_output("launch_missiles")
        .expect("it was attempted");
    assert_eq!(outcome, AuditOutcome::Failed);
    assert!(output.contains("Unknown tool"), "got: {output}");
    assert_eq!(sink.outcomes(), vec![AuditOutcome::Failed]);
}

#[tokio::test]
async fn malformed_arguments_are_reported_back_to_the_model() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    let server = FakeServer::start(two_turns(
        json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": { "name": "read_file", "arguments": "{\"path\":" },
        }),
        "let me retry",
    ))
    .await;

    let agent = agent_for(&server, directory.path());
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "read something".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes despite the bad call");

    let (outcome, output) = sink.tool_output("read_file").expect("it was attempted");
    assert_eq!(outcome, AuditOutcome::Failed);
    assert!(
        output.contains("Could not parse"),
        "the model needs to know why, got: {output}"
    );
    assert_eq!(sink.outcomes(), vec![AuditOutcome::Failed]);
}

#[tokio::test]
async fn a_cancelled_run_stops_without_finishing() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    // Only one turn is scripted, and it is never consumed: the run is cancelled
    // before the request can be answered.
    let server = FakeServer::start(vec![sse(&[json!({
        "choices": [{ "index": 0, "delta": { "content": "too late" } }],
    })])])
    .await;

    let agent = agent_for(&server, directory.path());
    let sink = CollectingSink::default();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = agent
        .run(
            1,
            "anything".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            cancel,
            &sink,
        )
        .await
        .expect_err("a cancelled run must not report success");

    assert_eq!(error.code, AgentError::cancelled().code);
    assert!(
        !sink
            .events()
            .iter()
            .any(|event| matches!(event, Event::RunFinished { .. })),
        "a cancelled run must not be marked finished"
    );
}

#[tokio::test]
async fn a_follow_up_run_replays_the_conversation_it_continues() {
    let directory = tempfile::tempdir().expect("a temp directory is available");
    // The same scripted answer for both runs: the assertions read the *requests*,
    // so what the server says does not matter, and a scripted third turn would
    // be dead weight.
    let answer = sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "content": "it is empty" } }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
    ]);
    let server = FakeServer::start(vec![answer.clone(), answer]).await;

    let agent = agent_for(&server, directory.path());

    // Run one: a bare prompt, nothing to replay.
    let sink = CollectingSink::default();
    let carried = agent
        .run(
            1,
            "list the directory".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the first run completes");

    // Run two: the follow-up, carrying the transcript of run one. This is the
    // shape the GUI builds for a second prompt in the same session.
    let history = vec![
        Message::user("list the directory"),
        Message::assistant("it is empty".into(), Vec::new()),
    ];
    let sink = CollectingSink::default();
    let _ = agent
        .run(
            2,
            "now what did it contain?".into(),
            RunRequest {
                history: &history,
                thinking: None,
                carried,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the follow-up completes");

    let first = server.request_body(0);
    let second = server.request_body(1);
    assert!(
        first.contains("list the directory") && !first.contains("it is empty"),
        "the first run has nothing behind it to replay, got: {first}"
    );
    assert!(
        second.contains("list the directory")
            && second.contains("it is empty")
            && second.contains("now what did it contain?"),
        "the follow-up must carry the earlier turns plus the new prompt, got: {second}"
    );
}

#[test]
fn read_image_is_offered_only_to_a_model_that_declares_image_input() {
    // The gate the worker applies: a text-only model must never see the tool,
    // because a picture it cannot read is a wasted call and a provider error.
    let text_only =
        to_openai_tools_from_descriptors(&ToolRegistry::with_builtins().descriptors()).to_string();
    assert!(
        !text_only.contains("read_image"),
        "a text-only model must not be offered read_image: {text_only}"
    );

    let multimodal =
        to_openai_tools_from_descriptors(&ToolRegistry::with_image_input().descriptors())
            .to_string();
    assert!(
        multimodal.contains("read_image"),
        "a model that declares image input must be offered read_image: {multimodal}"
    );
}

#[tokio::test]
async fn a_read_image_call_hands_the_picture_to_the_model() {
    // The copy `read_image` stores must not land in the user's real attachment
    // store, so the config directory is redirected for the length of the test.
    let config_dir = tempfile::tempdir().expect("a temp directory is available");
    std::env::set_var(crate::config::CONFIG_DIR_ENV, config_dir.path());

    let directory = tempfile::tempdir().expect("a temp directory is available");
    std::fs::write(directory.path().join("shot.png"), png_fixture(4, 4))
        .expect("the fixture is written");

    let server = FakeServer::start(two_turns(
        json!({
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": { "name": "read_image", "arguments": "{\"path\":\"shot.png\"}" },
        }),
        "a grey square",
    ))
    .await;

    let agent = agent_with_registry(
        ToolRegistry::with_image_input(),
        &server,
        directory.path(),
        ContextSettings::default(),
    );
    let sink = CollectingSink::default();

    let _ = agent
        .run(
            1,
            "what is in shot.png?".into(),
            RunRequest {
                history: &[],
                thinking: None,
                carried: None,
            },
            CancellationToken::new(),
            &sink,
        )
        .await
        .expect("the run completes");

    // The tool handed a picture back, not just a description of one.
    let (outcome, output) = sink
        .tool_output("read_image")
        .expect("read_image was called and finished");
    assert_eq!(outcome, AuditOutcome::Executed, "got: {output}");
    assert!(
        output.contains("image/png"),
        "the envelope names the media type: {output}"
    );

    let images = sink.images();
    assert_eq!(images.len(), 1, "the finished call must carry one image");
    assert_eq!(images[0].media_type, "image/png");
    assert_eq!((images[0].width, images[0].height), (4, 4));

    // The picture reaches the model in the *follow-up* request: the tool result
    // is a content array whose second part is an `image_url` data URL.
    let follow_up = server.request_body(1);
    assert!(
        follow_up.contains("\"type\":\"image_url\"")
            && follow_up.contains("data:image/png;base64,"),
        "the picture must ride in the follow-up request, got: {follow_up}"
    );
    // …and it must not have been inlined into a text field, where it would eat
    // the context window on every later turn.
    assert!(
        !sink.transcript().contains("base64"),
        "base64 must never reach the transcript"
    );
}
