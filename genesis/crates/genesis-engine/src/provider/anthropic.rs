//! Anthropic Messages API provider.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::{EngineError, Result};
use crate::types::{ContentBlock, LlmRequest, LlmResponse, Message, Role, StopReason, Usage};

use super::{Compat, Provider};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    compat: Compat,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            compat: Compat::anthropic(),
        }
    }

    fn build_body(&self, request: &LlmRequest) -> Value {
        let mut body = json!({
            "model": request.model,
            "messages": request.messages.iter().map(build_message).collect::<Vec<_>>(),
        });
        body[self.compat.max_tokens_field.as_str()] = json!(request.max_tokens);
        if self.compat.system_is_top_level {
            if let Some(system) = &request.system {
                body["system"] = json!(system);
            }
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request
                .tools
                .iter()
                .map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                }))
                .collect::<Vec<_>>());
        }
        body
    }

    fn parse_response(&self, body: &Value) -> Result<LlmResponse> {
        let blocks = body["content"]
            .as_array()
            .ok_or_else(|| EngineError::BadResponse("missing content array".into()))?;
        let mut content = Vec::with_capacity(blocks.len());
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => content.push(ContentBlock::Text {
                    text: block["text"].as_str().unwrap_or_default().to_string(),
                }),
                Some("tool_use") => content.push(ContentBlock::ToolUse {
                    id: block["id"].as_str().unwrap_or_default().to_string(),
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                    input: block["input"].clone(),
                }),
                // Thinking and other block types are not surfaced in v0.
                _ => {}
            }
        }
        let stop_reason = match body["stop_reason"].as_str() {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };
        let usage = Usage {
            input_tokens: body["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: body["usage"]["output_tokens"].as_u64().unwrap_or(0),
        };
        Ok(LlmResponse {
            content,
            stop_reason,
            usage,
        })
    }
}

fn build_message(message: &Message) -> Value {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<Value> = message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
            ContentBlock::ToolUse { id, name, input } => json!({
                "type": "tool_use", "id": id, "name": name, "input": input,
            }),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }),
        })
        .collect();
    json!({ "role": role, "content": content })
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
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
                "anthropic returned {status}: {detail}"
            )));
        }
        self.parse_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolDef;

    fn request() -> LlmRequest {
        LlmRequest {
            model: "claude-sonnet-5".to_string(),
            system: Some("be terse".to_string()),
            messages: vec![Message::user_text("hi")],
            tools: vec![ToolDef {
                name: "read_file".to_string(),
                description: "read a file".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            max_tokens: 512,
        }
    }

    #[test]
    fn body_uses_compat_fields_and_top_level_system() {
        let provider = AnthropicProvider::new("k");
        let body = provider.build_body(&request());
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn tool_result_round_trips_into_user_message() {
        let msg = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".to_string(),
                content: "ok".to_string(),
                is_error: false,
            }],
        };
        let value = build_message(&msg);
        assert_eq!(value["content"][0]["type"], "tool_result");
        assert_eq!(value["content"][0]["tool_use_id"], "tu_1");
        assert_eq!(value["content"][0]["is_error"], false);
    }

    #[test]
    fn parses_tool_use_response() {
        let provider = AnthropicProvider::new("k");
        let body = json!({
            "content": [
                { "type": "text", "text": "reading" },
                { "type": "tool_use", "id": "tu_1", "name": "read_file",
                  "input": { "path": "a.txt" } },
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 10, "output_tokens": 20 },
        });
        let parsed = provider.parse_response(&body).unwrap();
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.tool_uses().len(), 1);
        assert_eq!(parsed.text(), "reading");
        assert_eq!(parsed.usage.output_tokens, 20);
    }
}
