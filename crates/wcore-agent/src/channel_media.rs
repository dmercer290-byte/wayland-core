//! Inbound media enrichment for channel turns.
//!
//! A channel message can carry typed [`Attachment`]s (an image, a voice
//! note, …). By default the dispatcher only *summarises* them into the
//! prompt — the model is told "an image arrived" but never sees the content.
//! For conversational channels the agent also has no vision / transcription
//! tool to reach for, so the media is effectively invisible.
//!
//! This module closes that gap: before the turn prompt is built, the
//! [`ChannelMediaEnricher`] resolves each attachment to *derived text* — a
//! transcript for audio, a description for images — and writes it into
//! [`Attachment::transcribed`], which `build_turn_prompt` already prefers
//! over the bare URL.
//!
//! ## Tokens stay in the connector (auth-aware fetch)
//!
//! The enricher does NOT hold any channel credentials. It fetches the raw
//! bytes through a [`MediaByteSource`] — in production [`ManagerMediaSource`],
//! which routes to the originating connector via
//! [`ChannelManager::fetch_media_on`](wcore_channels::ChannelManager::fetch_media_on).
//! Each connector downloads its OWN media with its OWN token and platform
//! protocol (Slack bearer on `url_private`, WhatsApp's id→url→bytes, Matrix's
//! `mxc://`→authenticated endpoint, Telegram/Discord plain GET). The bytes
//! then go to the host-wired vision / transcription backend.
//!
//! ## Fail-soft and bounded
//!
//! Every step is best-effort: a connector that can't fetch (default
//! `Rejected`), a fetch error/timeout, an oversize payload, an unsupported
//! format, or a backend error all log and leave the attachment as a bare-URL
//! summary. A media problem never fails the turn. Both the fetch and the
//! analyze step are wall-clock bounded.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use wcore_channels::{Attachment, ChannelManager, MediaKind};
use wcore_tools::transcription_tools::{
    TRANSCRIPTION_MAX_BYTES, TRANSCRIPTION_MIN_BYTES, TranscriptionBackend, TranscriptionOutcome,
    detect_audio_mime,
};
use wcore_tools::vision_tools::{
    VISION_MAX_BYTES, VISION_MIN_BYTES, VisionBackend, VisionOutcome, detect_image_mime,
};

/// Max characters of derived text injected per attachment, to protect the
/// turn's prompt budget. Longer transcripts/descriptions are truncated.
const MAX_DERIVED_CHARS: usize = 2_000;

/// Wall-clock cap on the connector media download.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Wall-clock cap on the vision/transcription model call.
const ANALYZE_TIMEOUT: Duration = Duration::from_secs(45);

/// Vision prompt for eager image enrichment — terse on purpose.
const IMAGE_DESCRIBE_PROMPT: &str =
    "Concisely describe this image for a chat assistant, and quote any visible text verbatim.";

/// Source of inbound media bytes, fetched WITH the originating connector's
/// own credentials. Abstracts [`ChannelManager`] so the enricher is unit
/// testable without a live channel.
#[async_trait]
pub trait MediaByteSource: Send + Sync {
    /// Fetch the bytes of `attachment` as received on `channel`.
    async fn fetch(&self, channel: &str, attachment: &Attachment) -> Result<Vec<u8>, String>;
}

/// Production [`MediaByteSource`]: routes through the [`ChannelManager`] so
/// each connector fetches its own media with its own token. Credentials
/// never leave the connector boundary.
pub struct ManagerMediaSource {
    manager: Arc<tokio::sync::RwLock<ChannelManager>>,
}

impl ManagerMediaSource {
    pub fn new(manager: Arc<tokio::sync::RwLock<ChannelManager>>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl MediaByteSource for ManagerMediaSource {
    async fn fetch(&self, channel: &str, attachment: &Attachment) -> Result<Vec<u8>, String> {
        let guard = self.manager.read().await;
        guard
            .fetch_media_on(channel, attachment)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Resolves inbound attachments to derived text (audio→transcript,
/// image→description) using the host-wired vision / transcription backends
/// and a [`MediaByteSource`] for the auth-aware download.
///
/// Construct via [`ChannelMediaEnricher::new`]. When neither backend is
/// configured the enricher is *inert* ([`Self::is_inert`]) and the caller
/// should skip installing it.
pub struct ChannelMediaEnricher {
    vision: Option<Arc<dyn VisionBackend>>,
    transcription: Option<Arc<dyn TranscriptionBackend>>,
    source: Arc<dyn MediaByteSource>,
    fetch_timeout: Duration,
    analyze_timeout: Duration,
}

impl ChannelMediaEnricher {
    /// Build an enricher from the optional backends (the same the agent's
    /// `vision_analyze` / `transcribe_audio` tools use) and a byte source.
    pub fn new(
        vision: Option<Arc<dyn VisionBackend>>,
        transcription: Option<Arc<dyn TranscriptionBackend>>,
        source: Arc<dyn MediaByteSource>,
    ) -> Self {
        Self {
            vision,
            transcription,
            source,
            fetch_timeout: FETCH_TIMEOUT,
            analyze_timeout: ANALYZE_TIMEOUT,
        }
    }

    /// `true` when no backend is wired — the enricher would do nothing.
    pub fn is_inert(&self) -> bool {
        self.vision.is_none() && self.transcription.is_none()
    }

    /// Enrich each attachment in place. Best-effort and fail-soft.
    pub async fn enrich(&self, attachments: &mut [Attachment], channel: &str) {
        for att in attachments.iter_mut() {
            // Never overwrite a connector-supplied transcript.
            if att.transcribed.is_some() {
                continue;
            }
            // Only kinds we actually have a backend for proceed.
            match att.kind {
                MediaKind::Image if self.vision.is_some() => {}
                MediaKind::Audio if self.transcription.is_some() => {}
                _ => continue,
            }

            // Fetch the bytes via the originating connector (auth lives
            // there), bounded so a slow media host can't stall the turn.
            let att_for_fetch = att.clone();
            let bytes = match tokio::time::timeout(
                self.fetch_timeout,
                self.source.fetch(channel, &att_for_fetch),
            )
            .await
            {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    tracing::debug!(
                        target: "wcore_agent::channel_media",
                        channel,
                        kind = ?att.kind,
                        error = %e,
                        "inbound media fetch failed"
                    );
                    continue;
                }
                Err(_) => {
                    tracing::warn!(
                        target: "wcore_agent::channel_media",
                        channel,
                        kind = ?att.kind,
                        timeout_secs = self.fetch_timeout.as_secs(),
                        "inbound media fetch timed out"
                    );
                    continue;
                }
            };

            let derived = match att.kind {
                MediaKind::Image => self.describe_image(&bytes, channel).await,
                MediaKind::Audio => self.transcribe_audio(&bytes, channel).await,
                _ => None,
            };
            if let Some(text) = derived {
                let (text, truncated) = truncate(text, MAX_DERIVED_CHARS);
                tracing::info!(
                    target: "wcore_agent::channel_media",
                    channel,
                    kind = ?att.kind,
                    chars = text.len(),
                    truncated,
                    "inbound media enriched"
                );
                att.transcribed = Some(text);
            }
        }
    }

    async fn describe_image(&self, bytes: &[u8], channel: &str) -> Option<String> {
        let backend = self.vision.as_ref()?;
        if bytes.len() < VISION_MIN_BYTES || bytes.len() > VISION_MAX_BYTES {
            tracing::debug!(
                target: "wcore_agent::channel_media",
                channel,
                bytes = bytes.len(),
                "image size out of bounds; skipping"
            );
            return None;
        }
        let mime = detect_image_mime(bytes)?;
        match tokio::time::timeout(
            self.analyze_timeout,
            backend.analyze(mime, bytes, IMAGE_DESCRIBE_PROMPT),
        )
        .await
        {
            Ok(VisionOutcome::Ok { analysis }) => non_empty(analysis),
            Ok(VisionOutcome::Err { message }) => {
                tracing::debug!(target: "wcore_agent::channel_media", channel, error = %message, "vision backend error");
                None
            }
            Err(_) => {
                tracing::warn!(target: "wcore_agent::channel_media", channel, "vision analyze timed out");
                None
            }
        }
    }

    async fn transcribe_audio(&self, bytes: &[u8], channel: &str) -> Option<String> {
        let backend = self.transcription.as_ref()?;
        if bytes.len() < TRANSCRIPTION_MIN_BYTES || bytes.len() > TRANSCRIPTION_MAX_BYTES {
            tracing::debug!(
                target: "wcore_agent::channel_media",
                channel,
                bytes = bytes.len(),
                "audio size out of bounds; skipping"
            );
            return None;
        }
        let mime = detect_audio_mime(bytes)?;
        match tokio::time::timeout(self.analyze_timeout, backend.transcribe(mime, bytes, None))
            .await
        {
            Ok(TranscriptionOutcome::Ok { transcript, .. }) => non_empty(transcript),
            Ok(TranscriptionOutcome::Err { message }) => {
                tracing::debug!(target: "wcore_agent::channel_media", channel, error = %message, "transcription backend error");
                None
            }
            Err(_) => {
                tracing::warn!(target: "wcore_agent::channel_media", channel, "transcription timed out");
                None
            }
        }
    }
}

/// `Some(trimmed)` when non-empty, else `None`.
fn non_empty(text: String) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Truncate to at most `max` characters (char-boundary safe), appending a
/// marker when cut. Returns `(text, was_truncated)`.
fn truncate(text: String, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text, false);
    }
    let cut: String = text.chars().take(max).collect();
    (format!("{cut}… [truncated]"), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wcore_tools::transcription_tools::CapturingTranscriptionBackend;
    use wcore_tools::vision_tools::CapturingVisionBackend;

    /// Minimal valid PNG header — passes `detect_image_mime` + min-size.
    fn png_bytes() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0u8; 64]);
        v
    }

    /// Minimal valid OGG header — passes `detect_audio_mime` + min-size.
    fn ogg_bytes() -> Vec<u8> {
        let mut v = b"OggS".to_vec();
        v.extend_from_slice(&[0u8; 64]);
        v
    }

    /// Test byte source returning a fixed result, ignoring channel/attachment.
    struct StaticSource(Result<Vec<u8>, String>);

    #[async_trait]
    impl MediaByteSource for StaticSource {
        async fn fetch(&self, _channel: &str, _att: &Attachment) -> Result<Vec<u8>, String> {
            self.0.clone()
        }
    }

    fn image_att() -> Attachment {
        Attachment {
            url: "mxc://ex.org/abc".into(),
            content_type: Some("image/png".into()),
            kind: MediaKind::Image,
            ..Default::default()
        }
    }

    fn audio_att() -> Attachment {
        Attachment {
            url: "media-id-123".into(),
            content_type: Some("audio/ogg".into()),
            kind: MediaKind::Audio,
            ..Default::default()
        }
    }

    fn vision_enricher(canned: &str, source: StaticSource) -> ChannelMediaEnricher {
        ChannelMediaEnricher::new(
            Some(Arc::new(CapturingVisionBackend::new(canned))),
            None,
            Arc::new(source),
        )
    }

    fn audio_enricher(canned: &str, source: StaticSource) -> ChannelMediaEnricher {
        ChannelMediaEnricher::new(
            None,
            Some(Arc::new(CapturingTranscriptionBackend::new(canned))),
            Arc::new(source),
        )
    }

    #[tokio::test]
    async fn enriches_image_into_description() {
        let enricher = vision_enricher("a red bicycle", StaticSource(Ok(png_bytes())));
        let mut atts = vec![image_att()];
        enricher.enrich(&mut atts, "slack").await;
        assert_eq!(atts[0].transcribed.as_deref(), Some("a red bicycle"));
    }

    #[tokio::test]
    async fn enriches_audio_into_transcript() {
        let enricher = audio_enricher("meet at noon", StaticSource(Ok(ogg_bytes())));
        let mut atts = vec![audio_att()];
        enricher.enrich(&mut atts, "whatsapp").await;
        assert_eq!(atts[0].transcribed.as_deref(), Some("meet at noon"));
    }

    #[tokio::test]
    async fn fetch_error_leaves_attachment_untouched() {
        let enricher = vision_enricher("never", StaticSource(Err("401 unauthorized".into())));
        let mut atts = vec![image_att()];
        enricher.enrich(&mut atts, "slack").await;
        assert!(atts[0].transcribed.is_none());
    }

    #[tokio::test]
    async fn preserves_connector_supplied_transcript() {
        let enricher = audio_enricher("model text", StaticSource(Ok(ogg_bytes())));
        let mut atts = vec![Attachment {
            kind: MediaKind::Audio,
            transcribed: Some("connector transcript".into()),
            ..Default::default()
        }];
        enricher.enrich(&mut atts, "whatsapp").await;
        assert_eq!(atts[0].transcribed.as_deref(), Some("connector transcript"));
    }

    #[tokio::test]
    async fn unsupported_kind_is_skipped_without_fetch() {
        // Document kind + a source that would panic-loudly is never called
        // because the kind is filtered before fetch.
        let enricher = vision_enricher("never", StaticSource(Ok(png_bytes())));
        let mut atts = vec![Attachment {
            url: "x".into(),
            kind: MediaKind::Document,
            ..Default::default()
        }];
        enricher.enrich(&mut atts, "slack").await;
        assert!(atts[0].transcribed.is_none());
    }

    #[tokio::test]
    async fn image_skipped_when_only_transcription_wired() {
        let enricher = audio_enricher("audio only", StaticSource(Ok(png_bytes())));
        let mut atts = vec![image_att()];
        enricher.enrich(&mut atts, "slack").await;
        assert!(atts[0].transcribed.is_none());
    }

    #[tokio::test]
    async fn non_media_bytes_are_rejected_by_mime_sniff() {
        // Source returns an HTML error page; the mime sniff fails → skip.
        let html = b"<!DOCTYPE html><html>nope</html>".to_vec();
        let enricher = vision_enricher("never", StaticSource(Ok(html)));
        let mut atts = vec![image_att()];
        enricher.enrich(&mut atts, "slack").await;
        assert!(atts[0].transcribed.is_none());
    }

    #[tokio::test]
    async fn oversize_payload_is_skipped() {
        let mut huge = b"\x89PNG\r\n\x1a\n".to_vec();
        huge.resize(VISION_MAX_BYTES + 1024, 0u8);
        let enricher = vision_enricher("never", StaticSource(Ok(huge)));
        let mut atts = vec![image_att()];
        enricher.enrich(&mut atts, "slack").await;
        assert!(atts[0].transcribed.is_none());
    }

    #[tokio::test]
    async fn long_derived_text_is_truncated() {
        let huge = "x".repeat(MAX_DERIVED_CHARS + 500);
        let enricher = vision_enricher(&huge, StaticSource(Ok(png_bytes())));
        let mut atts = vec![image_att()];
        enricher.enrich(&mut atts, "slack").await;
        let got = atts[0].transcribed.as_deref().unwrap();
        assert!(got.ends_with("… [truncated]"));
    }

    #[tokio::test]
    async fn inert_enricher_is_a_noop() {
        let enricher =
            ChannelMediaEnricher::new(None, None, Arc::new(StaticSource(Ok(png_bytes()))));
        assert!(enricher.is_inert());
        let mut atts = vec![image_att()];
        enricher.enrich(&mut atts, "slack").await;
        assert!(atts[0].transcribed.is_none());
    }

    #[test]
    fn truncate_below_cap_is_unchanged() {
        let (t, cut) = truncate("short".to_string(), 100);
        assert_eq!(t, "short");
        assert!(!cut);
    }
}
