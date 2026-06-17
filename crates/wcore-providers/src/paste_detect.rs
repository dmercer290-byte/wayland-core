//! Paste-to-detect orchestration: raw paste → fingerprint → live validation.
//!
//! This is the brain of the `/config` paste flow, composing the two foundation
//! pieces ([`fingerprint`](crate::fingerprint) + [`key_validation`]) into a
//! single call the TUI can render without any logic of its own. Given whatever
//! the user actually pasted, it returns one [`DetectionResult`]:
//!
//! - [`DetectionResult::Connected`] — a provider was detected and the key
//!   authenticated (can list models). The headline success path.
//! - [`DetectionResult::NeedsGuidedSetup`] — the shape needs more than the
//!   pasted string (AWS secret + region, GCP project, an Azure endpoint, a JWT
//!   to route). We deliberately do **not** fire a blind network call here — that
//!   would leak the credential to the wrong endpoint and can't succeed anyway.
//! - [`DetectionResult::Unresolved`] — an ambiguous/unknown shape where no
//!   candidate authenticated. Carries the per-candidate failures and a best
//!   guess to prefill the provider picker.
//!
//! Candidates are probed most-specific-first with early return on the first
//! usable one (the fingerprint already ranks the likely provider first, biased
//! by any `export NAME=` hint), so the common ambiguous case (`sk-` → OpenAI
//! vs DeepSeek) resolves in one probe when the hint is present. A latency
//! optimization — probing the ambiguous bucket concurrently — is a documented
//! follow-up; correctness does not depend on it.

use std::sync::Arc;

use wcore_config::config::Config;

use crate::create_provider;
use crate::fingerprint::{CredentialKind, fingerprint_key};
use crate::key_validation::{ValidationOutcome, validate_with};
use crate::{LlmProvider, ModelInfo};

/// The single outcome of detecting and validating a pasted credential.
#[derive(Debug, Clone)]
pub enum DetectionResult {
    /// A provider was detected and the key authenticated.
    Connected {
        /// Provider slug (e.g. `"anthropic"`).
        provider: String,
        /// The live model catalog the probe fetched.
        models: Vec<ModelInfo>,
    },
    /// The credential shape is valid but needs a guided completion flow rather
    /// than a single validating request.
    NeedsGuidedSetup {
        kind: CredentialKind,
        /// The provider the wizard should target, if known (`"bedrock"`/`"vertex"`).
        provider: Option<String>,
    },
    /// Nothing authenticated. `attempts` records what was tried and why each
    /// failed; `best_guess` prefills the provider picker.
    Unresolved {
        attempts: Vec<ValidationOutcome>,
        best_guess: Option<String>,
    },
}

/// Detect and validate whatever the user pasted, using real providers.
///
/// Makes at most one network call per candidate (most-specific first, early
/// return on success). Never writes the key.
pub async fn detect_paste(base: &Config, raw: &str) -> DetectionResult {
    detect_with(base, raw, create_provider).await
}

/// Core of [`detect_paste`], generic over how providers are built so tests can
/// inject fakes. `build` is called once per probed candidate.
pub(crate) async fn detect_with<F>(base: &Config, raw: &str, build: F) -> DetectionResult
where
    F: Fn(&Config) -> Arc<dyn LlmProvider>,
{
    let fp = fingerprint_key(raw);

    // A shape that needs secret+region / project / endpoint / JWT routing must
    // not be blind-probed — branch to the guided wizard instead.
    if fp.needs_completion {
        return DetectionResult::NeedsGuidedSetup {
            kind: fp.kind,
            provider: fp.best().map(|g| g.slug.to_string()),
        };
    }

    // Unknown shape — straight to the picker.
    if fp.candidates.is_empty() {
        return DetectionResult::Unresolved {
            attempts: Vec::new(),
            best_guess: None,
        };
    }

    // Probe candidates in rank order; first usable one wins.
    let mut attempts = Vec::new();
    for guess in &fp.candidates {
        let outcome = validate_with(base, guess.slug, &fp.normalized, |cfg| build(cfg)).await;
        if outcome.is_usable() {
            return DetectionResult::Connected {
                provider: outcome.provider,
                models: outcome.models,
            };
        }
        attempts.push(outcome);
    }

    DetectionResult::Unresolved {
        best_guess: fp.best().map(|g| g.slug.to_string()),
        attempts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use wcore_config::config::{Config, ProviderType, provider_type_slug};
    use wcore_types::llm::{LlmEvent, LlmRequest};

    use crate::{ProviderError, alias_models};

    struct FakeProvider {
        models: Vec<ModelInfo>,
    }

    #[async_trait]
    impl LlmProvider for FakeProvider {
        async fn stream(
            &self,
            _request: &LlmRequest,
        ) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
            unreachable!("detect only calls list_models")
        }
        async fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
            Ok(self.models.clone())
        }
    }

    /// Build a provider factory where exactly `live_provider` returns a real
    /// (non-alias) catalog and every other provider floors to its alias list
    /// (i.e. authentication "failed" for everyone else).
    fn only_live(live_provider: ProviderType) -> impl Fn(&Config) -> Arc<dyn LlmProvider> {
        move |cfg: &Config| {
            let slug = provider_type_slug(cfg.provider);
            let models = if cfg.provider == live_provider {
                vec![ModelInfo {
                    id: format!("{slug}-flagship-live"),
                    display: "Flagship (live)".into(),
                }]
            } else {
                alias_models(slug) // floored == auth failed
            };
            Arc::new(FakeProvider { models })
        }
    }

    /// A factory where no provider authenticates (everyone floors).
    fn all_floored() -> impl Fn(&Config) -> Arc<dyn LlmProvider> {
        move |cfg: &Config| {
            let slug = provider_type_slug(cfg.provider);
            Arc::new(FakeProvider {
                models: alias_models(slug),
            })
        }
    }

    #[tokio::test]
    async fn unique_prefix_connects_directly() {
        let r = detect_with(
            &Config::default(),
            "sk-ant-api03-xyz",
            only_live(ProviderType::Anthropic),
        )
        .await;
        match r {
            DetectionResult::Connected { provider, models } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(models.len(), 1);
            }
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ambiguous_bucket_resolves_to_the_live_one() {
        // Bare `sk-` ranks [openai, deepseek]; only DeepSeek authenticates, so
        // the orchestrator tries openai (floors), then deepseek (connects).
        let r = detect_with(
            &Config::default(),
            "sk-0123456789abcdef0123456789abcdef",
            only_live(ProviderType::Deepseek),
        )
        .await;
        match r {
            DetectionResult::Connected { provider, .. } => assert_eq!(provider, "deepseek"),
            other => panic!("expected Connected deepseek, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn env_hint_orders_the_live_candidate_first() {
        // The hint pulls deepseek to the front; it connects on the first probe.
        let r = detect_with(
            &Config::default(),
            "export DEEPSEEK_API_KEY=sk-abcdef0123456789",
            only_live(ProviderType::Deepseek),
        )
        .await;
        assert!(matches!(r, DetectionResult::Connected { provider, .. } if provider == "deepseek"));
    }

    #[tokio::test]
    async fn aws_access_key_needs_guided_setup_without_probing() {
        // The build closure must never run for a non-bearer shape.
        let r = detect_with(&Config::default(), "AKIAIOSFODNN7EXAMPLE", |_cfg| {
            panic!("must not probe a non-bearer shape")
        })
        .await;
        match r {
            DetectionResult::NeedsGuidedSetup { kind, provider } => {
                assert_eq!(kind, CredentialKind::AwsAccessKeyId);
                assert_eq!(provider.as_deref(), Some("bedrock"));
            }
            other => panic!("expected NeedsGuidedSetup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_shape_is_unresolved_without_probing() {
        let r = detect_with(&Config::default(), "just-random-text", |_cfg| {
            panic!("must not probe an unknown shape")
        })
        .await;
        assert!(matches!(
            r,
            DetectionResult::Unresolved {
                attempts,
                best_guess: None
            } if attempts.is_empty()
        ));
    }

    #[tokio::test]
    async fn all_rejected_is_unresolved_with_attempts_and_guess() {
        let r = detect_with(
            &Config::default(),
            "sk-0123456789abcdef0123456789abcdef",
            all_floored(),
        )
        .await;
        match r {
            DetectionResult::Unresolved {
                attempts,
                best_guess,
            } => {
                assert_eq!(attempts.len(), 2, "both ambiguous candidates were tried");
                assert!(attempts.iter().all(|a| !a.is_usable()));
                assert_eq!(best_guess.as_deref(), Some("openai"));
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }
}
