//! Key validation — drive a live `list_models` probe against a just-pasted key.
//!
//! Step 2 of the `/config` paste-to-detect flow. Given a provider slug (from
//! [`fingerprint`](crate::fingerprint)) and the pasted key, build a one-off
//! provider bound to that key and ask it to list models. The result places the
//! key on a small confidence ladder:
//!
//! - [`Rung::Detected`]      — shape recognised; the probe did not authenticate.
//! - [`Rung::CanListModels`] — the live model endpoint returned a real catalog:
//!   the key authenticated and the account can enumerate models.
//!
//! A higher [`Rung::Ready`] (a billed one-token test request that proves the
//! account can actually *run* a model — catching credit-empty / access-blocked
//! keys) is modelled here but driven by the flow layer, which decides when to
//! spend. `validate_key` itself never spends tokens.
//!
//! Every provider's `list_models` floors to its static alias catalog on any
//! failure, so "the returned list equals the alias catalog" is read as "the live
//! fetch did not succeed" — the same Live-vs-BuiltIn discrimination
//! [`model_catalog`](crate::model_catalog) uses. Caveat: a provider whose
//! `/models` endpoint is unauthenticated (e.g. OpenRouter) returns a live list
//! even for a bad key; such providers need a key-specific validation endpoint,
//! a follow-up refinement tracked in the config-cockpit build plan.

use std::sync::Arc;

use wcore_config::config::{Config, provider_type_from_slug};

use crate::{LlmProvider, ModelInfo, alias_models, create_provider};

/// How far up the validation ladder a pasted key reached. Each rung implies the
/// ones below it, so the variants are ordered for comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    /// Shape recognised and a provider guessed, but the live probe did not
    /// authenticate (or the provider has no live catalog).
    Detected,
    /// The live model endpoint returned a real (non-fallback) catalog: the key
    /// authenticated and can list models.
    CanListModels,
    /// A minimal test request succeeded — the account can actually run a model.
    /// Set by the flow layer after an opt-in billed probe; never reached by
    /// [`validate_key`] alone.
    Ready,
}

/// The outcome of probing a pasted key against a candidate provider.
#[derive(Debug, Clone)]
pub struct ValidationOutcome {
    /// The provider slug that was probed.
    pub provider: String,
    /// The highest rung reached.
    pub reached: Rung,
    /// The live model catalog, when [`Rung::CanListModels`] was reached
    /// (otherwise empty).
    pub models: Vec<ModelInfo>,
    /// A human-readable reason the key did not authenticate, when it didn't.
    pub failure: Option<String>,
}

impl ValidationOutcome {
    /// `true` when the key authenticated and can list models.
    pub fn is_usable(&self) -> bool {
        self.reached >= Rung::CanListModels
    }

    /// Number of models the live catalog returned (0 when not usable).
    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    fn detected(slug: &str, failure: impl Into<String>) -> Self {
        Self {
            provider: slug.to_string(),
            reached: Rung::Detected,
            models: Vec::new(),
            failure: Some(failure.into()),
        }
    }
}

/// Probe a pasted `api_key` against the provider named by `slug`, returning how
/// far up the validation ladder it got. Reuses the provider's live
/// `list_models` (timeout-bounded, alias-floored) — the same path that warms the
/// model picker — so a successful probe doubles as a warm catalog fetch.
///
/// Makes one network call (the model-list request). Never writes the key.
pub async fn validate_key(base: &Config, slug: &str, api_key: &str) -> ValidationOutcome {
    validate_with(base, slug, api_key, create_provider).await
}

/// Core of [`validate_key`], generic over how the provider is built so the
/// paste-detect orchestrator and tests can inject a fake provider without a
/// network call.
pub(crate) async fn validate_with<F>(
    base: &Config,
    slug: &str,
    api_key: &str,
    build: F,
) -> ValidationOutcome
where
    F: FnOnce(&Config) -> Arc<dyn LlmProvider>,
{
    let Some(provider_type) = provider_type_from_slug(slug) else {
        return ValidationOutcome::detected(slug, format!("unknown provider '{slug}'"));
    };

    let cfg = base.for_key_validation(provider_type, api_key);
    let provider = build(&cfg);

    match provider.list_models().await {
        Ok(models) if is_live_catalog(&models, slug) => ValidationOutcome {
            provider: slug.to_string(),
            reached: Rung::CanListModels,
            models,
            failure: None,
        },
        Ok(_) => ValidationOutcome::detected(
            slug,
            "key not accepted, or this provider has no live model catalog",
        ),
        Err(e) => ValidationOutcome::detected(slug, e.to_string()),
    }
}

/// A `list_models` result is a *live* catalog when it is non-empty and differs
/// from the provider's static alias fallback. An equal result means the live
/// fetch floored (auth/HTTP/parse failure) and is not proof the key works.
fn is_live_catalog(models: &[ModelInfo], slug: &str) -> bool {
    !models.is_empty() && models != alias_models(slug).as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use wcore_config::config::Config;
    use wcore_types::llm::{LlmEvent, LlmRequest};

    use crate::ProviderError;

    enum Behavior {
        Models(Vec<ModelInfo>),
        Error(String),
    }

    struct FakeProvider {
        behavior: Behavior,
    }

    #[async_trait]
    impl LlmProvider for FakeProvider {
        async fn stream(
            &self,
            _request: &LlmRequest,
        ) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
            unreachable!("validate_key only calls list_models")
        }
        async fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
            match &self.behavior {
                Behavior::Models(m) => Ok(m.clone()),
                Behavior::Error(e) => Err(anyhow::anyhow!(e.clone())),
            }
        }
    }

    fn fake(behavior: Behavior) -> impl FnOnce(&Config) -> Arc<dyn LlmProvider> {
        move |_cfg| Arc::new(FakeProvider { behavior })
    }

    #[tokio::test]
    async fn live_catalog_reaches_can_list_models() {
        let live = vec![ModelInfo {
            id: "claude-opus-4-8".into(),
            display: "Opus 4.8".into(),
        }];
        let out = validate_with(
            &Config::default(),
            "anthropic",
            "sk-ant-xxx",
            fake(Behavior::Models(live)),
        )
        .await;
        assert_eq!(out.reached, Rung::CanListModels);
        assert!(out.is_usable());
        assert_eq!(out.model_count(), 1);
        assert!(out.failure.is_none());
    }

    #[tokio::test]
    async fn floored_to_alias_catalog_is_only_detected() {
        // A result equal to the alias catalog means the live fetch failed (the
        // provider floored), so the key is NOT proven to work.
        let aliases = alias_models("anthropic");
        assert!(!aliases.is_empty(), "anthropic needs an alias catalog here");
        let out = validate_with(
            &Config::default(),
            "anthropic",
            "sk-ant-bad",
            fake(Behavior::Models(aliases)),
        )
        .await;
        assert_eq!(out.reached, Rung::Detected);
        assert!(!out.is_usable());
        assert!(out.failure.is_some());
    }

    #[tokio::test]
    async fn empty_catalog_is_only_detected() {
        let out = validate_with(
            &Config::default(),
            "anthropic",
            "x",
            fake(Behavior::Models(Vec::new())),
        )
        .await;
        assert_eq!(out.reached, Rung::Detected);
        assert!(!out.is_usable());
    }

    #[tokio::test]
    async fn error_surfaces_as_detected_with_failure_message() {
        let out = validate_with(
            &Config::default(),
            "openai",
            "x",
            fake(Behavior::Error("401 Unauthorized".into())),
        )
        .await;
        assert_eq!(out.reached, Rung::Detected);
        assert_eq!(out.failure.as_deref(), Some("401 Unauthorized"));
    }

    #[tokio::test]
    async fn unknown_slug_fails_fast_without_building() {
        // The build closure must never run for an unknown slug.
        let out = validate_with(&Config::default(), "not-a-provider", "x", |_cfg| {
            panic!("must not build a provider for an unknown slug")
        })
        .await;
        assert_eq!(out.reached, Rung::Detected);
        assert!(out.failure.unwrap().contains("unknown provider"));
    }
}
