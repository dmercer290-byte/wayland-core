//! OpenAI Chat Completions provider.
//!
//! With `Compat::openai_compatible()` and a custom base URL this same
//! implementation drives Ollama, vLLM, LM Studio, and other
//! OpenAI-compatible servers.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{EngineError, Result};
use crate::types::{ContentBlock, LlmRequest, LlmResponse, Role, StopReason, Usage};

use super::{Compat, Provider};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    compat: Compat,
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_config(api_key, DEFAULT_BASE_URL, Compat::openai())
    }

    /// Point at any OpenAI-compatible server with explicit compat settings.
    pub fn with_config(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        compat: Compat,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            compat,
        }
    }

    fn build_body(&self, request: &LlmRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if !self.compat.system_is_top_level {
            if let Some(system) = &request.system {
                messages.push(json!({ "role": "system", "content": system }));
            }
        }
        for message in &request.messages {
            build_messages(message, &mut messages);
        }
        let mut body = json!({
            "model": request.model,
            "messages": messages,
        });
        body[self.compat.max_tokens_field.as_str()] = json!(request.max_tokens);
        if !request.tools.is_empty() {
            body["tools"] = json!(request
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                }))
                .collect::<Vec<_>>());
        }
        body
    }

    fn parse_response(&self, body: &Value) -> Result<LlmResponse> {
        let choice = &body["choices"][0];
        if choice.is_null() {
            return Err(EngineError::BadResponse("missing choices[0]".into()));
        }
        let message = &choice["message"];
        let mut content = Vec::new();
        if let Some(text) = message["content"].as_str() {
            if !text.is_empty() {
                content.push(ContentBlock::Text {
                    text: text.to_string(),
                });
            }
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
                let input: Value = serde_json::from_str(arguments).map_err(|e| {
                    EngineError::BadResponse(format!("unparseable tool arguments: {e}"))
                })?;
                content.push(ContentBlock::ToolUse {
                    id: call["id"].as_str().unwrap_or_default().to_string(),
                    name: call["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    input,
                });
            }
        }
        let stop_reason = match choice["finish_reason"].as_str() {
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };
        let usage = Usage {
            input_tokens: body["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: body["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        };
        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
        })
    }
}

/// Lower one neutral message into Chat Completions messages.
///
/// The shapes diverge: assistant tool calls ride on the assistant message as
/// `tool_calls`, and each tool result becomes its own `role: "tool"` message.
fn build_messages(message: &crate::types::Message, out: &mut Vec<Value>) {
    match message.role {
        Role::Assistant => {
            let text = message.text();
            let tool_calls: Vec<Value> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": input.to_string(),
                        },
                    })),
                    _ => None,
                })
                .collect();
            let mut msg = json!({ "role": "assistant" });
            msg["content"] = if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
            if !tool_calls.is_empty() {
                msg["tool_calls"] = json!(tool_calls);
            }
            out.push(msg);
        }
        Role::User => {
            for block in &message.content {
                match block {
                    ContentBlock::Text { text } => {
                        out.push(json!({ "role": "user", "content": text }));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                    }
                    ContentBlock::ToolUse { .. } => {}
                }
            }
        }
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&self.build_body(request))
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        if !status.is_success() {
            let detail = body["error"]["message"]
                .as_str()
                .unwrap_or("no error detail");
            return Err(EngineError::Provider(format!(
                "openai returned {status}: {detail}"
            )));
        }
        self.parse_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, ToolDef};

    fn request() -> LlmRequest {
        LlmRequest {
            model: "gpt-test".to_string(),
            system: Some("be terse".to_string()),
            messages: vec![
                Message::user_text("hi"),
                Message::assistant(vec![ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({ "path": "a.txt" }),
                }]),
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "file body".to_string(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolDef {
                name: "read_file".to_string(),
                description: "read a file".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            max_tokens: 256,
        }
    }

    #[test]
    fn body_lowers_history_to_chat_completions_shape() {
        let provider = OpenAiProvider::new("k");
        let body = provider.build_body(&request());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        // OpenAI preset uses the newer token-cap field.
        assert_eq!(body["max_completion_tokens"], 256);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["tools"][0]["type"], "function");
    }

    #[test]
    fn compatible_preset_uses_classic_max_tokens() {
        let provider = OpenAiProvider::with_config(
            "k",
            "http://localhost:11434/v1",
            Compat::openai_compatible(),
        );
        let body = provider.build_body(&request());
        assert_eq!(body["max_tokens"], 256);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn parses_tool_call_response() {
        let provider = OpenAiProvider::new("k");
        let body = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"ls\"}",
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 7 },
        });
        let parsed = provider.parse_response(&body).unwrap();
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        let uses = parsed.tool_uses();
        assert_eq!(uses[0].1, "bash");
        assert_eq!(uses[0].2["command"], "ls");
    }
}
