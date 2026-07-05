//! The provider layer.
//!
//! Every LLM backend implements [`Provider`]. The engine sees only the
//! provider-neutral types in [`crate::types`]; wire-format conversion lives
//! inside each implementation.
//!
//! **No hardcoded provider quirks.** Behavioral differences between backends
//! are expressed as [`Compat`] fields with per-family presets — provider code
//! reads `self.compat.field`, never `if base_url.contains(...)`.

mod anthropic;
mod openai;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiProvider;

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{LlmRequest, LlmResponse};

/// Compatibility knobs that vary between API families or deployments.
///
/// Add a field here (with a preset default) instead of writing a conditional
/// in provider code.
#[derive(Debug, Clone)]
pub struct Compat {
    /// JSON field name carrying the output-token cap.
    pub max_tokens_field: String,
    /// Whether the API accepts a top-level `system` string (Anthropic style)
    /// as opposed to a system-role message (OpenAI style).
    pub system_is_top_level: bool,
}

impl Compat {
    /// Defaults for the Anthropic Messages API.
    pub fn anthropic() -> Self {
        Self {
            max_tokens_field: "max_tokens".to_string(),
            system_is_top_level: true,
        }
    }

    /// Defaults for OpenAI's own endpoint (newer models reject `max_tokens`).
    pub fn openai() -> Self {
        Self {
            max_tokens_field: "max_completion_tokens".to_string(),
            system_is_top_level: false,
        }
    }

    /// Defaults for OpenAI-compatible servers (Ollama, vLLM, LM Studio, …),
    /// which broadly still expect the classic `max_tokens` field.
    pub fn openai_compatible() -> Self {
        Self {
            max_tokens_field: "max_tokens".to_string(),
            system_is_top_level: false,
        }
    }
}

/// A chat-completion backend.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Human-readable backend name (for logs and errors).
    fn name(&self) -> &str;

    /// Run one completion and return the full response.
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse>;
}

#[async_trait]
impl Provider for Box<dyn Provider> {
    fn name(&self) -> &str {
        (**self).name()
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse> {
        (**self).complete(request).await
    }
}
