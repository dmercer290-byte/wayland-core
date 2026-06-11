//! Inbound media enrichment for channel turns.
//!
//! A channel message can carry typed [`Attachment`]s (an image, a voice
//! note, …). By default the dispatcher only *summarises* them into the
//! prompt — the model is told "an image arrived at <url>" but never sees
//! the content. For conversational channels the agent also has no vision /
//! transcription tool to reach for (those tools are not in the
//! `Conversational` allowlist), so the media is effectively invisible.
//!
//! This module closes that gap: before the turn prompt is built, the
//! [`ChannelMediaEnricher`] eagerly resolves each attachment to *derived
//! text* — a transcript for audio, a description for images — and writes it
//! into [`Attachment::transcribed`], which `build_turn_prompt` already
//! prefers over the bare URL.
//!
//! ## Reuse, not reimplementation
//!
//! The enricher does **not** re-implement fetching, SSRF defense, size
//! caps, or MIME sniffing. It holds the real [`VisionAnalyzeTool`] /
//! [`TranscribeAudioTool`] (built only when the host wired a backend) and
//! calls their `execute()` surface, parsing the derived text out of the
//! result. Every safety check the agent-facing tool enforces (SSRF-guarded
//! fetch via the host fetcher, the 20/25 MB hard cap, magic-byte MIME
//! validation, the structured-error contract) is therefore enforced here
//! too, for free.
//!
//! ## Fail-soft
//!
//! Enrichment is best-effort: any failure (no backend, fetch error,
//! unsupported format, SSRF block, timeout) logs and leaves the attachment
//! as a plain URL summary. A media problem must never fail the turn — the
//! agent still receives the text and the attachment reference.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use wcore_channels::{Attachment, MediaKind};
use wcore_tools::Tool;
use wcore_tools::transcription_tools::{AudioFetcher, TranscribeAudioTool, TranscriptionBackend};
use wcore_tools::vision_tools::{ImageFetcher, VisionAnalyzeTool, VisionBackend};

/// Max characters of derived text injected per attachment. A long
/// transcript or verbose description must not blow the turn's prompt
/// budget; anything beyond this is truncated with a marker.
const MAX_DERIVED_CHARS: usize = 2_000;

/// Per-attachment wall-clock cap for the whole fetch + model round trip.
/// The underlying fetchers already cap their own connect/read timeouts;
/// this bounds the *combined* fetch-then-analyze so a slow provider cannot
/// stall a channel turn indefinitely.
const ENRICH_TIMEOUT: Duration = Duration::from_secs(45);

/// Vision prompt for eager image enrichment. Kept terse — the goal is a
/// compact, prompt-budget-friendly description plus any visible text, not
/// an essay.
const IMAGE_DESCRIBE_PROMPT: &str =
    "Concisely describe this image for a chat assistant, and quote any visible text verbatim.";

/// Resolves inbound attachments to derived text (audio→transcript,
/// image→description) using the host-wired vision / transcription tools.
///
/// Construct via [`ChannelMediaEnricher::new`]. When neither a vision nor a
/// transcription backend is configured the enricher is *inert*
/// ([`Self::is_inert`]) and the caller should skip installing it.
pub struct ChannelMediaEnricher {
    /// `Some` only when a vision backend was wired. Holds the real tool so
    /// SSRF/cap/sniff are reused.
    vision: Option<VisionAnalyzeTool>,
    /// `Some` only when a transcription backend was wired.
    transcription: Option<TranscribeAudioTool>,
    timeout: Duration,
}

impl ChannelMediaEnricher {
    /// Build an enricher from the optional backends and their fetchers
    /// (the same components `bootstrap` wires into the agent's
    /// `vision_analyze` / `transcribe_audio` tools). A `None` backend means
    /// that media class is left as a plain summary.
    pub fn new(
        vision_backend: Option<Arc<dyn VisionBackend>>,
        vision_fetcher: Arc<dyn ImageFetcher>,
        transcription_backend: Option<Arc<dyn TranscriptionBackend>>,
        audio_fetcher: Arc<dyn AudioFetcher>,
    ) -> Self {
        Self {
            vision: vision_backend.map(|b| VisionAnalyzeTool::new(b, vision_fetcher)),
            transcription: transcription_backend
                .map(|b| TranscribeAudioTool::new(b, audio_fetcher)),
            timeout: ENRICH_TIMEOUT,
        }
    }

    /// `true` when no backend is wired — the enricher would do nothing, so
    /// the caller can avoid installing it (and the per-turn clone of the
    /// message it implies).
    pub fn is_inert(&self) -> bool {
        self.vision.is_none() && self.transcription.is_none()
    }

    /// Enrich each attachment in place. Best-effort and fail-soft: an
    /// attachment that already carries `transcribed`, has a non-HTTP(S)
    /// `url`, is of an unsupported kind, or fails to resolve is left
    /// untouched.
    pub async fn enrich(&self, attachments: &mut [Attachment], channel: &str) {
        for att in attachments.iter_mut() {
            // A connector may have already produced text (e.g. a platform
            // that ships voice-note transcripts). Never overwrite it.
            if att.transcribed.is_some() {
                continue;
            }
            if !is_http_url(&att.url) {
                continue;
            }
            let derived = match att.kind {
                MediaKind::Audio => self.transcribe(&att.url, channel).await,
                MediaKind::Image => self.describe_image(&att.url, channel).await,
                // Video / Document / Other carry no eager backend in v1.
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

    /// Transcribe an audio attachment, or `None` if transcription is not
    /// wired / the attempt failed.
    async fn transcribe(&self, url: &str, channel: &str) -> Option<String> {
        let tool = self.transcription.as_ref()?;
        let content = self
            .execute_bounded(tool, json!({ "audio_url": url }), channel, "audio")
            .await?;
        json_string_field(&content, "transcript")
    }

    /// Describe an image attachment, or `None` if vision is not wired / the
    /// attempt failed.
    async fn describe_image(&self, url: &str, channel: &str) -> Option<String> {
        let tool = self.vision.as_ref()?;
        let content = self
            .execute_bounded(
                tool,
                json!({ "image_url": url, "question": IMAGE_DESCRIBE_PROMPT }),
                channel,
                "image",
            )
            .await?;
        json_string_field(&content, "analysis")
    }

    /// Run a tool's `execute` under the enrichment timeout, returning the
    /// raw JSON `content` string on success. Errors and timeouts log and
    /// return `None` so the caller falls back to the summary.
    async fn execute_bounded(
        &self,
        tool: &dyn Tool,
        input: Value,
        channel: &str,
        media: &'static str,
    ) -> Option<String> {
        match tokio::time::timeout(self.timeout, tool.execute(input)).await {
            Ok(result) if !result.is_error => Some(result.content),
            Ok(result) => {
                tracing::warn!(
                    target: "wcore_agent::channel_media",
                    channel,
                    media,
                    error = %result.content,
                    "inbound media enrichment failed"
                );
                None
            }
            Err(_) => {
                tracing::warn!(
                    target: "wcore_agent::channel_media",
                    channel,
                    media,
                    timeout_secs = self.timeout.as_secs(),
                    "inbound media enrichment timed out"
                );
                None
            }
        }
    }
}

/// Only `http`/`https` URLs are fetchable. A connector that failed to
/// resolve a download URL leaves a raw platform reference (e.g. a Telegram
/// `file_id`) in `url`; that is not fetchable, so skip it.
fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Pull a non-empty string field out of a tool's JSON result body.
fn json_string_field(content: &str, field: &str) -> Option<String> {
    let value: Value = serde_json::from_str(content).ok()?;
    let text = value.get(field)?.as_str()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
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
    use wcore_tools::transcription_tools::{CapturingTranscriptionBackend, StaticAudioFetcher};
    use wcore_tools::vision_tools::{CapturingVisionBackend, StaticImageFetcher};

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

    fn image_att(url: &str) -> Attachment {
        Attachment {
            url: url.into(),
            content_type: Some("image/png".into()),
            kind: MediaKind::Image,
            ..Default::default()
        }
    }

    fn audio_att(url: &str) -> Attachment {
        Attachment {
            url: url.into(),
            content_type: Some("audio/ogg".into()),
            kind: MediaKind::Audio,
            ..Default::default()
        }
    }

    fn vision_only(canned: &str) -> ChannelMediaEnricher {
        ChannelMediaEnricher::new(
            Some(Arc::new(CapturingVisionBackend::new(canned))),
            Arc::new(StaticImageFetcher::new(png_bytes())),
            None,
            Arc::new(StaticAudioFetcher::new(Vec::new())),
        )
    }

    fn audio_only(canned: &str) -> ChannelMediaEnricher {
        ChannelMediaEnricher::new(
            None,
            Arc::new(StaticImageFetcher::new(Vec::new())),
            Some(Arc::new(CapturingTranscriptionBackend::new(canned))),
            Arc::new(StaticAudioFetcher::new(ogg_bytes())),
        )
    }

    #[tokio::test]
    async fn enriches_image_into_description() {
        let enricher = vision_only("a red bicycle leaning on a wall");
        let mut atts = vec![image_att("https://example.com/bike.png")];
        enricher.enrich(&mut atts, "telegram").await;
        assert_eq!(
            atts[0].transcribed.as_deref(),
            Some("a red bicycle leaning on a wall")
        );
    }

    #[tokio::test]
    async fn enriches_audio_into_transcript() {
        let enricher = audio_only("meet me at noon");
        let mut atts = vec![audio_att("https://example.com/voice.ogg")];
        enricher.enrich(&mut atts, "telegram").await;
        assert_eq!(atts[0].transcribed.as_deref(), Some("meet me at noon"));
    }

    #[tokio::test]
    async fn preserves_connector_supplied_transcript() {
        let enricher = audio_only("model transcript that must not be used");
        let mut atts = vec![Attachment {
            url: "https://example.com/voice.ogg".into(),
            kind: MediaKind::Audio,
            transcribed: Some("connector transcript".into()),
            ..Default::default()
        }];
        enricher.enrich(&mut atts, "telegram").await;
        assert_eq!(atts[0].transcribed.as_deref(), Some("connector transcript"));
    }

    #[tokio::test]
    async fn skips_non_http_reference() {
        // A bare Telegram file_id (getFile fell back) is not fetchable.
        let enricher = vision_only("never produced");
        let mut atts = vec![image_att("BAADBAADrwADBREAAYag")];
        enricher.enrich(&mut atts, "telegram").await;
        assert!(atts[0].transcribed.is_none());
    }

    #[tokio::test]
    async fn skips_unsupported_kind() {
        let enricher = vision_only("never produced");
        let mut atts = vec![Attachment {
            url: "https://example.com/report.pdf".into(),
            kind: MediaKind::Document,
            ..Default::default()
        }];
        enricher.enrich(&mut atts, "telegram").await;
        assert!(atts[0].transcribed.is_none());
    }

    #[tokio::test]
    async fn image_skipped_when_only_transcription_wired() {
        let enricher = audio_only("audio backend present, vision absent");
        let mut atts = vec![image_att("https://example.com/x.png")];
        enricher.enrich(&mut atts, "telegram").await;
        assert!(
            atts[0].transcribed.is_none(),
            "image must stay a summary when no vision backend is wired"
        );
    }

    #[tokio::test]
    async fn long_derived_text_is_truncated() {
        let huge = "x".repeat(MAX_DERIVED_CHARS + 500);
        let enricher = vision_only(&huge);
        let mut atts = vec![image_att("https://example.com/big.png")];
        enricher.enrich(&mut atts, "telegram").await;
        let got = atts[0].transcribed.as_deref().unwrap();
        assert!(got.ends_with("… [truncated]"), "expected truncation marker");
        assert!(
            got.chars().count() <= MAX_DERIVED_CHARS + " … [truncated]".chars().count(),
            "truncated text must respect the cap"
        );
    }

    #[tokio::test]
    async fn inert_enricher_is_a_noop() {
        let enricher = ChannelMediaEnricher::new(
            None,
            Arc::new(StaticImageFetcher::new(Vec::new())),
            None,
            Arc::new(StaticAudioFetcher::new(Vec::new())),
        );
        assert!(enricher.is_inert());
        let mut atts = vec![image_att("https://example.com/x.png")];
        enricher.enrich(&mut atts, "telegram").await;
        assert!(atts[0].transcribed.is_none());
    }

    #[test]
    fn truncate_below_cap_is_unchanged() {
        let (text, cut) = truncate("short".to_string(), 100);
        assert_eq!(text, "short");
        assert!(!cut);
    }

    #[test]
    fn is_http_url_matches_only_web_schemes() {
        assert!(is_http_url("https://example.com/a.png"));
        assert!(is_http_url("http://example.com/a.png"));
        assert!(!is_http_url("file:///etc/passwd"));
        assert!(!is_http_url("BAADBAADrwADBREAAYag"));
    }
}
