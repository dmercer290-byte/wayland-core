//! The agent loop: prompt → completion → tool runs → repeat until done.

use serde_json::Value;

use crate::error::{EngineError, Result};
use crate::provider::Provider;
use crate::tools::ToolRegistry;
use crate::types::{ContentBlock, LlmRequest, Message, Role, StopReason, Usage};

/// Events surfaced to the host (CLI, desktop app, …) while the agent runs.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The assistant produced text.
    Text(String),
    /// A tool is about to run.
    ToolStart { name: String, input: Value },
    /// A tool finished; `is_error` mirrors what the model will see.
    ToolEnd {
        name: String,
        output: String,
        is_error: bool,
    },
}

pub struct AgentConfig {
    pub model: String,
    pub system: Option<String>,
    pub max_tokens: u32,
    /// Hard cap on provider round-trips per `run()` call.
    pub max_turns: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system: Some(DEFAULT_SYSTEM_PROMPT.to_string()),
            max_tokens: 8192,
            max_turns: 32,
        }
    }
}

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are Genesis, a software engineering agent \
working in the user's workspace. Use the available tools to inspect and modify files \
and run commands. Prefer reading files before editing them. When the task is complete, \
reply with a concise summary of what you did.";

/// One agent session: a provider, a tool set, and the running conversation.
pub struct Agent<P: Provider> {
    provider: P,
    tools: ToolRegistry,
    config: AgentConfig,
    history: Vec<Message>,
    usage: Usage,
}

impl<P: Provider> Agent<P> {
    pub fn new(provider: P, tools: ToolRegistry, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            config,
            history: Vec::new(),
            usage: Usage::default(),
        }
    }

    /// Cumulative token usage across all runs in this session.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The conversation so far.
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Run one user prompt to completion, emitting events along the way.
    /// Returns the assistant's final text.
    pub async fn run(
        &mut self,
        prompt: &str,
        mut on_event: impl FnMut(AgentEvent),
    ) -> Result<String> {
        self.history.push(Message::user_text(prompt));
        for _ in 0..self.config.max_turns {
            let request = LlmRequest {
                model: self.config.model.clone(),
                system: self.config.system.clone(),
                messages: self.history.clone(),
                tools: self.tools.defs(),
                max_tokens: self.config.max_tokens,
            };
            let response = self.provider.complete(&request).await?;
            self.usage.input_tokens += response.usage.input_tokens;
            self.usage.output_tokens += response.usage.output_tokens;

            let text = response.text();
            if !text.is_empty() {
                on_event(AgentEvent::Text(text.clone()));
            }
            self.history
                .push(Message::assistant(response.content.clone()));

            if response.stop_reason != StopReason::ToolUse {
                return Ok(text);
            }

            let mut results = Vec::new();
            for (id, name, input) in response.tool_uses() {
                on_event(AgentEvent::ToolStart {
                    name: name.to_string(),
                    input: input.clone(),
                });
                let (content, is_error) = match self.tools.run(name, input).await {
                    Ok(output) => (output, false),
                    Err(e) => (e.to_string(), true),
                };
                on_event(AgentEvent::ToolEnd {
                    name: name.to_string(),
                    output: content.clone(),
                    is_error,
                });
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content,
                    is_error,
                });
            }
            self.history.push(Message {
                role: Role::User,
                content: results,
            });
        }
        Err(EngineError::MaxTurns(self.config.max_turns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use crate::tools::Tool;
    use crate::types::{LlmResponse, ToolDef};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    /// Replays a fixed script of responses.
    struct ScriptedProvider {
        script: Mutex<Vec<LlmResponse>>,
        seen_requests: Mutex<Vec<LlmRequest>>,
    }

    impl ScriptedProvider {
        fn new(mut responses: Vec<LlmResponse>) -> Self {
            responses.reverse();
            Self {
                script: Mutex::new(responses),
                seen_requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
            self.seen_requests.lock().unwrap().push(request.clone());
            self.script
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| EngineError::Provider("script exhausted".into()))
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "echo".to_string(),
                description: "echo".to_string(),
                input_schema: json!({ "type": "object" }),
            }
        }
        async fn run(&self, input: &Value) -> Result<String> {
            Ok(format!("echo:{}", input["text"].as_str().unwrap_or("")))
        }
    }

    fn tool_use_response(id: &str, name: &str, input: Value) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
            },
        }
    }

    fn text_response(text: &str) -> LlmResponse {
        LlmResponse {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 3,
                output_tokens: 4,
            },
        }
    }

    fn agent(provider: ScriptedProvider) -> Agent<ScriptedProvider> {
        let mut tools = ToolRegistry::default();
        tools.register(Box::new(EchoTool));
        Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "test-model".to_string(),
                ..AgentConfig::default()
            },
        )
    }

    #[tokio::test]
    async fn runs_tools_and_feeds_results_back() {
        let provider = ScriptedProvider::new(vec![
            tool_use_response("tu_1", "echo", json!({ "text": "hi" })),
            text_response("done"),
        ]);
        let mut agent = agent(provider);
        let mut events = Vec::new();
        let answer = agent.run("go", |e| events.push(e)).await.unwrap();
        assert_eq!(answer, "done");

        // Second request must contain the assistant tool_use turn and the
        // tool_result turn built from the echo tool's real output.
        let requests = agent.provider.seen_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let second = &requests[1];
        assert_eq!(second.messages.len(), 3);
        match &second.messages[2].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "tu_1");
                assert_eq!(content, "echo:hi");
                assert!(!is_error);
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
        assert!(matches!(events[0], AgentEvent::ToolStart { .. }));
        assert!(matches!(events[1], AgentEvent::ToolEnd { .. }));
        assert_eq!(agent.usage().output_tokens, 6);
    }

    #[tokio::test]
    async fn unknown_tool_becomes_error_result_not_crash() {
        let provider = ScriptedProvider::new(vec![
            tool_use_response("tu_1", "nope", json!({})),
            text_response("recovered"),
        ]);
        let mut agent = agent(provider);
        let answer = agent.run("go", |_| {}).await.unwrap();
        assert_eq!(answer, "recovered");
        let requests = agent.provider.seen_requests.lock().unwrap();
        match &requests[1].messages[2].content[0] {
            ContentBlock::ToolResult {
                is_error, content, ..
            } => {
                assert!(is_error);
                assert!(content.contains("unknown tool"));
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_turns_is_enforced() {
        let loops: Vec<LlmResponse> = (0..5)
            .map(|i| tool_use_response(&format!("tu_{i}"), "echo", json!({})))
            .collect();
        let provider = ScriptedProvider::new(loops);
        let mut tools = ToolRegistry::default();
        tools.register(Box::new(EchoTool));
        let mut agent = Agent::new(
            provider,
            tools,
            AgentConfig {
                model: "test-model".to_string(),
                max_turns: 3,
                ..AgentConfig::default()
            },
        );
        let err = agent.run("go", |_| {}).await.unwrap_err();
        assert!(matches!(err, EngineError::MaxTurns(3)));
    }
}
