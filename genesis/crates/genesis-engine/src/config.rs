//! Configuration loading.
//!
//! Precedence (highest wins): CLI flags (applied by the caller) → environment
//! variables → `~/.genesis/config.toml` → built-in defaults.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{EngineError, Result};

/// Which provider family to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    Openai,
    /// Any OpenAI-compatible server (Ollama, vLLM, LM Studio, …).
    OpenaiCompatible,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct FileConfig {
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    max_tokens: Option<u32>,
    max_turns: Option<usize>,
}

/// Resolved engine configuration.
#[derive(Debug, Clone)]
pub struct GenesisConfig {
    pub provider: ProviderKind,
    pub model: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub max_tokens: u32,
    pub max_turns: usize,
}

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5";
const DEFAULT_OPENAI_MODEL: &str = "gpt-5.2";

impl GenesisConfig {
    /// Standard config file location: `~/.genesis/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".genesis").join("config.toml"))
    }

    /// Load configuration, merging the optional config file with environment
    /// variables. `provider_override` / `model_override` come from CLI flags.
    pub fn load(
        path: Option<&Path>,
        provider_override: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Self> {
        let file = match path {
            Some(p) if p.exists() => {
                let raw = std::fs::read_to_string(p)?;
                toml::from_str::<FileConfig>(&raw)
                    .map_err(|e| EngineError::Config(format!("{}: {e}", p.display())))?
            }
            _ => FileConfig::default(),
        };

        let provider_name = provider_override
            .map(str::to_string)
            .or_else(|| std::env::var("GENESIS_PROVIDER").ok())
            .or(file.provider)
            .unwrap_or_else(|| "anthropic".to_string());
        let provider = parse_provider(&provider_name)?;

        let model = model_override
            .map(str::to_string)
            .or_else(|| std::env::var("GENESIS_MODEL").ok())
            .or(file.model)
            .unwrap_or_else(|| default_model(provider).to_string());

        let api_key = resolve_api_key(provider)?;

        let base_url = std::env::var("GENESIS_BASE_URL").ok().or(file.base_url);

        Ok(Self {
            provider,
            model,
            api_key,
            base_url,
            max_tokens: file.max_tokens.unwrap_or(8192),
            max_turns: file.max_turns.unwrap_or(32),
        })
    }
}

fn parse_provider(name: &str) -> Result<ProviderKind> {
    match name {
        "anthropic" => Ok(ProviderKind::Anthropic),
        "openai" => Ok(ProviderKind::Openai),
        "openai_compatible" | "openai-compatible" => Ok(ProviderKind::OpenaiCompatible),
        other => Err(EngineError::Config(format!(
            "unknown provider '{other}' (expected anthropic, openai, or openai_compatible)"
        ))),
    }
}

fn default_model(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Anthropic => DEFAULT_ANTHROPIC_MODEL,
        ProviderKind::Openai | ProviderKind::OpenaiCompatible => DEFAULT_OPENAI_MODEL,
    }
}

fn resolve_api_key(provider: ProviderKind) -> Result<String> {
    // GENESIS_API_KEY always wins; otherwise fall back to the provider
    // family's conventional variable. Compatible servers often need no key.
    if let Ok(key) = std::env::var("GENESIS_API_KEY") {
        return Ok(key);
    }
    let (var, optional) = match provider {
        ProviderKind::Anthropic => ("ANTHROPIC_API_KEY", false),
        ProviderKind::Openai => ("OPENAI_API_KEY", false),
        ProviderKind::OpenaiCompatible => ("OPENAI_API_KEY", true),
    };
    match std::env::var(var) {
        Ok(key) => Ok(key),
        Err(_) if optional => Ok(String::new()),
        Err(_) => Err(EngineError::Config(format!(
            "no API key: set GENESIS_API_KEY or {var}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_names() {
        assert_eq!(
            parse_provider("anthropic").unwrap(),
            ProviderKind::Anthropic
        );
        assert_eq!(
            parse_provider("openai-compatible").unwrap(),
            ProviderKind::OpenaiCompatible
        );
        assert!(parse_provider("gemini").is_err());
    }

    #[test]
    fn file_config_parses_and_overrides_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "provider = \"openai_compatible\"\nmodel = \"llama3\"\nbase_url = \"http://localhost:11434/v1\"\nmax_turns = 5\n",
        )
        .unwrap();
        // openai_compatible needs no key, so this loads without env setup.
        let config = GenesisConfig::load(Some(&path), None, None).unwrap();
        assert_eq!(config.provider, ProviderKind::OpenaiCompatible);
        assert_eq!(config.model, "llama3");
        assert_eq!(
            config.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(config.max_turns, 5);

        // CLI override beats the file.
        let config = GenesisConfig::load(Some(&path), None, Some("mistral")).unwrap();
        assert_eq!(config.model, "mistral");
    }

    #[test]
    fn bad_toml_is_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "provider = [broken").unwrap();
        assert!(matches!(
            GenesisConfig::load(Some(&path), None, None),
            Err(EngineError::Config(_))
        ));
    }
}
