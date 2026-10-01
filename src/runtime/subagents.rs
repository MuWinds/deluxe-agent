//! Native sub-agent runner backed by the existing `Agent` loop.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, RunRequest};
use crate::error::{AgentError, Result};
use crate::harness::services::native_services;
use crate::harness::{AgentEvent, AgentEventSink, AgentRole, SubagentContext, SubagentRunner};
use crate::llm::{LlmClient, Message, UserTurn};
use crate::tools::{ToolRegistry, ToolSettings};

pub struct NativeSubagentRunner {
    llm: LlmClient,
    registry: Arc<ToolRegistry>,
    settings: Arc<RwLock<ToolSettings>>,
}

impl NativeSubagentRunner {
    /// Builds a runner over the parent's model and the non-delegating registry.
    pub fn new(
        llm: LlmClient,
        registry: Arc<ToolRegistry>,
        settings: Arc<RwLock<ToolSettings>>,
    ) -> Self {
        Self {
            llm,
            registry,
            settings,
        }
    }
}

#[async_trait]
impl SubagentRunner for NativeSubagentRunner {
    async fn run_role(
        &self,
        role: &AgentRole,
        prompt: String,
        context: SubagentContext,
        sink: Arc<dyn AgentEventSink>,
        cancel: CancellationToken,
    ) -> Result<String> {
        let services = Arc::new(native_services(
            self.llm.clone(),
            self.registry.clone(),
            self.settings.clone(),
            Arc::new(crate::runtime::prompt::NativePromptProvider::new()),
        ));
        let tools = services.tools.descriptors();
        let system_prompt = services.prompts.build_role_prompt(role, &tools)?;
        let agent = Agent::from_services(
            services,
            context.project,
            context.context_settings,
            system_prompt,
        );
        let answer = Arc::new(Mutex::new(String::new()));
        let answer_sink = AnswerSink {
            downstream: sink,
            answer: answer.clone(),
        };
        let history: [Message; 0] = [];
        agent
            .run(
                0,
                UserTurn::from(prompt),
                RunRequest {
                    history: &history,
                    thinking: None,
                    carried: None,
                },
                cancel,
                &answer_sink,
            )
            .await?;
        let answer = answer
            .lock()
            .map(|answer| answer.clone())
            .unwrap_or_default();
        if answer.trim().is_empty() {
            return Err(AgentError::internal("the agent finished without an answer"));
        }
        Ok(answer)
    }
}

struct AnswerSink {
    downstream: Arc<dyn AgentEventSink>,
    answer: Arc<Mutex<String>>,
}

impl AgentEventSink for AnswerSink {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::AssistantDone { content, .. } = &event {
            if !content.trim().is_empty() {
                if let Ok(mut answer) = self.answer.lock() {
                    *answer = content.clone();
                }
            }
        }
        self.downstream.emit(event);
    }
}
