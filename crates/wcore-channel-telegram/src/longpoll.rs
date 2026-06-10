//! Background long-poll task. Spawned by `TelegramChannel::start()`,
//! signaled to exit by the watch channel in `TelegramChannel`.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, watch};
use wcore_channels::event::{
    Attachment, ChannelEvent, ChatType, IncomingMessage, MediaKind, MentionKind,
};

use crate::api::{Update, get_file, get_updates};

/// Constructor arguments — flatter than a struct, easier to spawn.
pub(crate) struct LongPollArgs {
    pub http: wcore_egress::EgressClient,
    pub api_base: String,
    pub bot_token: String,
    pub timeout_secs: u32,
    pub allowed_chat_ids: HashSet<String>,
    pub inbox: Arc<Mutex<VecDeque<ChannelEvent>>>,
    pub shutdown: watch::Receiver<bool>,
}

/// Drive `getUpdates` in a loop until the shutdown signal flips.
///
/// Backoff on transient failures stays small (2s + jitter-free) — the
/// caller's poll cadence is the load-bearing knob, not this loop's.
pub(crate) async fn longpoll_loop(args: LongPollArgs) {
    let LongPollArgs {
        http,
        api_base,
        bot_token,
        timeout_secs,
        allowed_chat_ids,
        inbox,
        mut shutdown,
    } = args;

    let mut offset: i64 = 0;
    let mut consecutive_failures: u32 = 0;

    loop {
        if *shutdown.borrow() {
            break;
        }

        // Race the next API call against a shutdown signal so we don't
        // get stuck for ~timeout_secs after stop() flips the flag.
        let updates = tokio::select! {
            biased;
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
                continue;
            }
            r = get_updates(&http, &api_base, &bot_token, offset, timeout_secs) => r,
        };

        match updates {
            Ok(updates) => {
                consecutive_failures = 0;
                ingest_updates(
                    updates,
                    &http,
                    &api_base,
                    &bot_token,
                    &allowed_chat_ids,
                    &inbox,
                    &mut offset,
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(
                    target: "wcore_channel_telegram::longpoll",
                    error = %e,
                    "getUpdates failed; backing off"
                );
                consecutive_failures = consecutive_failures.saturating_add(1);
                // Linear cap at 30s — same family as the send retry cap
                // but without the exponential bias (the poll loop is
                // self-correcting; tight failure loops here are usually
                // a transient outage, not a coding error).
                let sleep_secs = (2_u64.saturating_mul(consecutive_failures as u64)).min(30);
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { break; }
                    }
                    _ = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {}
                }
            }
        }
    }
}

/// A media reference pulled off a Telegram message, pre-resolution. The
/// `file_id` still needs a `getFile` round-trip before it points at
/// downloadable bytes; `kind` / `content_type` are known from the field
/// the media arrived in.
struct PendingMedia {
    file_id: String,
    kind: MediaKind,
    content_type: Option<String>,
}

/// Map a `(content_type, MediaKind)` for each media-bearing field on a
/// Telegram message into the pre-resolution `PendingMedia` list. Pure so
/// the field→kind/mime mapping is testable without a network call.
fn pending_media(msg: &crate::api::Message) -> Vec<PendingMedia> {
    let mut out: Vec<PendingMedia> = Vec::new();
    // Photos: take the last (largest) PhotoSize only.
    if let Some(ref sizes) = msg.photo
        && let Some(largest) = sizes.last()
    {
        out.push(PendingMedia {
            file_id: largest.file_id.clone(),
            kind: MediaKind::Image,
            content_type: Some("image/jpeg".to_string()),
        });
    }
    if let Some(ref v) = msg.voice {
        out.push(PendingMedia {
            file_id: v.file_id.clone(),
            kind: MediaKind::Audio,
            // Voice notes are always OGG/Opus; fall back if absent.
            content_type: v
                .mime_type
                .clone()
                .or_else(|| Some("audio/ogg".to_string())),
        });
    }
    if let Some(ref d) = msg.document {
        out.push(PendingMedia {
            file_id: d.file_id.clone(),
            kind: MediaKind::Document,
            content_type: d.mime_type.clone(),
        });
    }
    if let Some(ref vid) = msg.video {
        out.push(PendingMedia {
            file_id: vid.file_id.clone(),
            kind: MediaKind::Video,
            content_type: vid
                .mime_type
                .clone()
                .or_else(|| Some("video/mp4".to_string())),
        });
    }
    out
}

/// Resolve each `PendingMedia` into a typed [`Attachment`]. The download
/// URL comes from a bounded `getFile` call; on failure we log and fall
/// back to leaving the raw `file_id` as the URL so the message is never
/// dropped. The resolved `file_id` is preserved in `path` so downstream
/// consumers can re-resolve if the URL expires.
async fn resolve_attachments(
    pending: Vec<PendingMedia>,
    http: &wcore_egress::EgressClient,
    api_base: &str,
    bot_token: &str,
) -> Vec<Attachment> {
    let mut attachments = Vec::with_capacity(pending.len());
    for m in pending {
        let url = match get_file(http, api_base, bot_token, &m.file_id).await {
            Ok(download_url) => download_url,
            Err(e) => {
                tracing::warn!(
                    target: "wcore_channel_telegram::longpoll",
                    error = %e,
                    file_id = %m.file_id,
                    "getFile failed; falling back to raw file_id as url"
                );
                m.file_id.clone()
            }
        };
        attachments.push(Attachment {
            url,
            path: Some(m.file_id),
            content_type: m.content_type,
            kind: m.kind,
            ..Default::default()
        });
    }
    attachments
}

#[allow(clippy::too_many_arguments)]
async fn ingest_updates(
    updates: Vec<Update>,
    http: &wcore_egress::EgressClient,
    api_base: &str,
    bot_token: &str,
    allowed_chat_ids: &HashSet<String>,
    inbox: &Arc<Mutex<VecDeque<ChannelEvent>>>,
    offset: &mut i64,
) {
    if updates.is_empty() {
        return;
    }
    let mut events = Vec::with_capacity(updates.len());
    for u in updates {
        // Advance offset past every Update we see, even ones we drop —
        // otherwise we'd loop on the same filtered-out message forever.
        *offset = (*offset).max(u.update_id + 1);

        let Some(msg) = u.message else { continue };
        let chat_id_str = msg.chat.id.to_string();
        if !allowed_chat_ids.is_empty() && !allowed_chat_ids.contains(&chat_id_str) {
            continue;
        }

        // ---- Sender identity ----------------------------------------
        let (sender_id, author, sender_display, sender_handle, is_bot) =
            if let Some(ref f) = msg.from {
                let sid = f.id.to_string();
                // author: prefer @username, fall back to first_name, then id
                let display_name = match (f.first_name.as_deref(), f.last_name.as_deref()) {
                    (Some(first), Some(last)) => Some(format!("{first} {last}")),
                    (Some(first), None) => Some(first.to_string()),
                    _ => None,
                };
                let author = f
                    .username
                    .clone()
                    .or_else(|| display_name.clone())
                    .unwrap_or_else(|| sid.clone());
                (sid, author, display_name, f.username.clone(), f.is_bot)
            } else {
                ("unknown".to_string(), "unknown".to_string(), None, None, false)
            };

        // ---- Chat type ----------------------------------------------
        let chat_type = match msg.chat.chat_type.as_str() {
            "private" => ChatType::Direct,
            "group" | "supergroup" => ChatType::Group,
            "channel" => ChatType::Channel,
            // Unrecognised future type — treat as Group (multi-party)
            _ => ChatType::Group,
        };

        // ---- Attachments --------------------------------------------
        // Collect the raw media references, then resolve each file_id to
        // a real download URL via getFile. Only messages that actually
        // carry media make these extra calls; each is bounded by the http
        // client's timeout so a hung getFile can't stall the poll loop.
        let pending = pending_media(&msg);
        let attachments = resolve_attachments(pending, http, api_base, bot_token).await;

        // ---- Mention detection --------------------------------------
        // A `mention` entity in the text signals an @-mention; the bot
        // has no self-identity here so we can only detect the presence of
        // any mention and surface it as Native.
        let has_mention = msg
            .entities
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|e| e.kind == "mention");
        let was_mentioned = has_mention;
        let mention_kind = was_mentioned.then_some(MentionKind::Native);

        // ---- Reply context ------------------------------------------
        let reply_to_message_id = msg
            .reply_to_message
            .as_deref()
            .map(|r| r.message_id.to_string());
        let reply_to_text = msg
            .reply_to_message
            .as_deref()
            .and_then(|r| r.text.clone());

        let text = msg.text.unwrap_or_default();

        events.push(ChannelEvent::MessageReceived {
            msg: IncomingMessage {
                id: msg.message_id.to_string(),
                conversation_id: chat_id_str,
                author,
                text,
                ts_secs: msg.date,
                attachments,
                // Sender identity
                sender_id,
                sender_display,
                sender_handle,
                sender_alt_id: None,
                is_bot,
                is_self: false,
                // Chat context
                chat_type,
                chat_name: msg.chat.title.clone(),
                space_id: None,
                thread_id: msg.message_thread_id.map(|id| id.to_string()),
                parent_chat_id: None,
                // Account / platform routing
                account_id: None,
                platform: Some("telegram".into()),
                // Mention
                was_mentioned,
                mention_kind,
                // Reply
                reply_to_message_id,
                reply_to_text,
            },
        });
    }
    if !events.is_empty() {
        let mut guard = inbox.lock().await;
        for e in events {
            guard.push_back(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Message;

    fn message_from_json(raw: &str) -> Message {
        serde_json::from_str(raw).expect("valid Message JSON")
    }

    #[test]
    fn pending_media_maps_photo_to_image_jpeg() {
        // Photos carry no mime; we synthesize image/jpeg and pick the
        // largest (last) PhotoSize.
        let msg = message_from_json(
            r#"{"message_id":1,"chat":{"id":1},"photo":[{"file_id":"small"},{"file_id":"large"}]}"#,
        );
        let pending = pending_media(&msg);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].file_id, "large");
        assert_eq!(pending[0].kind, MediaKind::Image);
        assert_eq!(pending[0].content_type.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn pending_media_maps_voice_to_audio_ogg_fallback() {
        let msg = message_from_json(r#"{"message_id":1,"chat":{"id":1},"voice":{"file_id":"v"}}"#);
        let pending = pending_media(&msg);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, MediaKind::Audio);
        assert_eq!(pending[0].content_type.as_deref(), Some("audio/ogg"));
    }

    #[test]
    fn pending_media_prefers_reported_mime() {
        // A document with an explicit mime keeps it; a video without one
        // falls back to video/mp4.
        let msg = message_from_json(
            r#"{"message_id":1,"chat":{"id":1},"document":{"file_id":"d","mime_type":"application/pdf"},"video":{"file_id":"vid"}}"#,
        );
        let pending = pending_media(&msg);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].kind, MediaKind::Document);
        assert_eq!(pending[0].content_type.as_deref(), Some("application/pdf"));
        assert_eq!(pending[1].kind, MediaKind::Video);
        assert_eq!(pending[1].content_type.as_deref(), Some("video/mp4"));
    }

    #[test]
    fn pending_media_empty_for_text_only_message() {
        let msg = message_from_json(r#"{"message_id":1,"chat":{"id":1},"text":"hello"}"#);
        assert!(pending_media(&msg).is_empty());
    }
}
