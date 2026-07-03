//! Bot Framework inbound Activity parsing.
//!
//! A Teams bot receives inbound traffic as Bot Framework **Activity** JSON
//! POSTed to its messaging endpoint by the Azure Bot Service. This module
//! turns the slice of that payload we care about into the enriched
//! [`IncomingMessage`] the inbound dispatch kernel consumes.
//!
//! Only `type == "message"` activities produce a message; lifecycle
//! activities (`conversationUpdate`, `typing`, …) parse to `Ok(None)` so the
//! webhook host can ACK them without enqueuing anything.
//!
//! **Round-trip with the send path**: `conversation_id` is encoded as
//! `{serviceUrl}|{conversationId}` so the reply path's `parse_chat_id`
//! recovers the tenant-specific `serviceUrl` the Connector API requires. The
//! `serviceUrl` is taken from the activity itself (Teams stamps it per
//! activity), falling back to the channel's configured service URL.
//!
//! **Attachments are NOT parsed in v1.** Teams delivers files as
//! `attachments[]` entries with `contentType`/`contentUrl`, but fetching them
//! requires a separate auth-gated download against the Graph/Connector API;
//! deferred to a follow-up. `attachments` is left empty here.

use serde::Deserialize;
use wcore_channels::event::{ChatType, IncomingMessage};

use crate::error::MsTeamsError;

/// The slice of a Bot Framework Activity we consume. Every field is
/// `#[serde(default)]` so partial / unfamiliar payloads deserialize rather
/// than 400 — we validate the fields we actually require explicitly.
#[derive(Debug, Deserialize, Default)]
struct Activity {
    #[serde(rename = "type", default)]
    activity_type: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    from: ChannelAccount,
    #[serde(default)]
    recipient: ChannelAccount,
    #[serde(default)]
    conversation: ConversationAccount,
    #[serde(rename = "serviceUrl", default)]
    service_url: String,
    /// RFC3339 timestamp string, e.g. `2026-06-10T12:34:56.789Z`.
    #[serde(default)]
    timestamp: String,
}

/// `from` / `recipient` — a Bot Framework channel account.
#[derive(Debug, Deserialize, Default)]
struct ChannelAccount {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    /// Bot Framework actor role: `"user"` or `"bot"`. Lets the inbound parser
    /// flag bot-authored activities so the dispatch loop guard drops them.
    #[serde(default)]
    role: String,
}

/// `conversation` — the Bot Framework conversation reference.
#[derive(Debug, Deserialize, Default)]
struct ConversationAccount {
    #[serde(default)]
    id: String,
    #[serde(rename = "conversationType", default)]
    conversation_type: String,
    #[serde(rename = "isGroup", default)]
    is_group: bool,
    #[serde(default)]
    name: String,
}

/// Map a Teams conversation descriptor to a [`ChatType`].
///
/// Teams uses `conversationType` of `"personal"` (1:1 DM), `"groupChat"`
/// (ad-hoc group), or `"channel"` (a channel within a team). `isGroup` is a
/// secondary signal for older payloads that omit `conversationType`.
fn chat_type_of(conv: &ConversationAccount) -> ChatType {
    match conv.conversation_type.as_str() {
        "personal" => ChatType::Direct,
        "channel" => ChatType::Channel,
        "groupChat" => ChatType::Group,
        _ if conv.is_group => ChatType::Group,
        _ => ChatType::Direct,
    }
}

/// Parse a Bot Framework Activity JSON body into an [`IncomingMessage`].
///
/// Returns:
/// * `Ok(Some(msg))` for a `type == "message"` activity.
/// * `Ok(None)` for any other activity type (lifecycle events such as
///   `conversationUpdate`, `typing`, message reactions, …).
/// * `Err(MsTeamsError::Parse)` if the JSON is malformed or a `message`
///   activity is missing the required `from.id` (the access-control / dedup
///   key — we refuse to fabricate it).
///
/// `service_url_fallback` is used to build `conversation_id` when the
/// activity omits its own `serviceUrl` (the channel's configured service URL).
pub fn activity_to_incoming(
    raw_body: &str,
    service_url_fallback: &str,
) -> Result<Option<IncomingMessage>, MsTeamsError> {
    let activity: Activity =
        serde_json::from_str(raw_body).map_err(|e| MsTeamsError::Parse(e.to_string()))?;

    // Only message activities carry user text; everything else is a
    // lifecycle/control event the host can ACK without enqueuing.
    if activity.activity_type != "message" {
        return Ok(None);
    }

    if activity.from.id.is_empty() {
        return Err(MsTeamsError::Parse(
            "message activity missing from.id".to_string(),
        ));
    }

    // serviceUrl|conversationId so the reply path (parse_chat_id) recovers
    // the tenant-specific serviceUrl. Strip a trailing slash on the
    // serviceUrl so the encoding is stable regardless of how Teams stamps it.
    let service_url = if activity.service_url.is_empty() {
        service_url_fallback
    } else {
        activity.service_url.as_str()
    };
    let service_url = service_url.strip_suffix('/').unwrap_or(service_url);
    let conversation_id = format!("{service_url}|{}", activity.conversation.id);

    // RFC3339 timestamp → epoch seconds; fall back to now if absent/unparsable.
    let ts_secs = chrono::DateTime::parse_from_rfc3339(&activity.timestamp)
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|_| chrono::Utc::now().timestamp());

    let chat_type = chat_type_of(&activity.conversation);
    let chat_name = if activity.conversation.name.is_empty() {
        None
    } else {
        Some(activity.conversation.name.clone())
    };
    let sender_display = if activity.from.name.is_empty() {
        None
    } else {
        Some(activity.from.name.clone())
    };
    // recipient.id is the bot identity that received this activity.
    let account_id = if activity.recipient.id.is_empty() {
        None
    } else {
        Some(activity.recipient.id.clone())
    };

    // Author label: prefer the display name, fall back to the stable id.
    let author = if activity.from.name.is_empty() {
        activity.from.id.clone()
    } else {
        activity.from.name.clone()
    };

    let msg = IncomingMessage {
        sender_id: activity.from.id.clone(),
        sender_display,
        chat_type,
        chat_name,
        account_id,
        platform: Some("msteams".into()),
        // Bot Framework stamps the sender's role; a "bot" actor is another bot,
        // so flag it and let the dispatch kernel's loop guard drop it instead of
        // engaging in a bot-to-bot loop. Teams does not echo the bot's own
        // outbound back, so is_self has no reliable signal and stays false.
        is_bot: activity.from.role.eq_ignore_ascii_case("bot"),
        ..IncomingMessage::new(activity.id, conversation_id, author, activity.text, ts_secs)
    };

    Ok(Some(msg))
}

/// Extract just the `serviceUrl` from a Bot Framework Activity body, without
/// the full enrichment parse.
///
/// Used by the webhook auth gate to cross-check the JWT's `serviceurl` claim
/// against the Activity body (defense-in-depth against a token replayed
/// alongside a swapped `serviceUrl`). Returns `Ok(None)` when the activity
/// omits `serviceUrl`; `Err(Parse)` only on malformed JSON.
pub fn service_url_of(raw_body: &str) -> Result<Option<String>, MsTeamsError> {
    #[derive(Deserialize, Default)]
    struct ServiceUrlOnly {
        #[serde(rename = "serviceUrl", default)]
        service_url: String,
    }
    let parsed: ServiceUrlOnly =
        serde_json::from_str(raw_body).map_err(|e| MsTeamsError::Parse(e.to_string()))?;
    if parsed.service_url.is_empty() {
        Ok(None)
    } else {
        Ok(Some(parsed.service_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE_FALLBACK: &str = "https://smba.trafficmanager.net/amer/";

    #[test]
    fn message_activity_parses_enriched_fields() {
        let body = r#"{
            "type": "message",
            "id": "1622471234567",
            "text": "hello bot",
            "serviceUrl": "https://smba.trafficmanager.net/emea/",
            "timestamp": "2026-06-10T12:34:56.789Z",
            "from": { "id": "29:user-aad-id", "name": "Ada Lovelace" },
            "recipient": { "id": "28:bot-app-id", "name": "Genesis" },
            "conversation": {
                "id": "19:abc@thread.v2",
                "conversationType": "personal",
                "isGroup": false,
                "name": "Ada / Genesis"
            }
        }"#;

        let msg = activity_to_incoming(body, SERVICE_FALLBACK)
            .expect("parse ok")
            .expect("message activity yields Some");

        assert_eq!(msg.id, "1622471234567");
        assert_eq!(msg.sender_id, "29:user-aad-id");
        assert_eq!(msg.author, "Ada Lovelace");
        assert_eq!(msg.sender_display.as_deref(), Some("Ada Lovelace"));
        assert_eq!(msg.text, "hello bot");
        assert_eq!(msg.account_id.as_deref(), Some("28:bot-app-id"));
        assert_eq!(msg.platform.as_deref(), Some("msteams"));
        assert_eq!(msg.chat_type, ChatType::Direct);
        assert_eq!(msg.chat_name.as_deref(), Some("Ada / Genesis"));
        // conversation_id uses the activity's own serviceUrl (trailing slash
        // stripped) so parse_chat_id round-trips it on the reply path.
        assert_eq!(
            msg.conversation_id,
            "https://smba.trafficmanager.net/emea|19:abc@thread.v2"
        );
        // 2026-06-10T12:34:56Z epoch seconds.
        assert_eq!(msg.ts_secs, 1_781_094_896);
        assert!(!msg.is_self);
        // A normal user activity (no "bot" role) is not flagged as a bot.
        assert!(!msg.is_bot);
        assert!(msg.attachments.is_empty());
    }

    #[test]
    fn bot_role_activity_is_flagged_is_bot() {
        // An activity whose `from.role` is "bot" must set is_bot so the dispatch
        // kernel's loop guard drops it (prevents bot-to-bot loops).
        let body = r#"{
            "type": "message",
            "id": "id-bot",
            "text": "automated",
            "from": { "id": "28:other-bot", "name": "Other Bot", "role": "bot" },
            "conversation": { "id": "19:abc@thread.v2" },
            "serviceUrl": "https://smba.trafficmanager.net/emea/",
            "timestamp": "2026-06-10T12:34:56Z"
        }"#;
        let msg = activity_to_incoming(body, SERVICE_FALLBACK)
            .expect("parses")
            .expect("is a message");
        assert!(msg.is_bot, "from.role=bot must set is_bot");
    }

    #[test]
    fn group_chat_maps_to_group_chat_type() {
        let body = r#"{
            "type": "message",
            "id": "id1",
            "text": "hi all",
            "from": { "id": "29:u", "name": "U" },
            "recipient": { "id": "28:bot" },
            "conversation": { "id": "19:room@thread.v2", "conversationType": "groupChat" }
        }"#;
        let msg = activity_to_incoming(body, SERVICE_FALLBACK)
            .unwrap()
            .unwrap();
        assert_eq!(msg.chat_type, ChatType::Group);
        // No serviceUrl on the activity → fallback is used (slash stripped).
        assert_eq!(
            msg.conversation_id,
            "https://smba.trafficmanager.net/amer|19:room@thread.v2"
        );
    }

    #[test]
    fn conversation_update_yields_none() {
        let body = r#"{
            "type": "conversationUpdate",
            "id": "id2",
            "membersAdded": [{ "id": "29:u" }],
            "recipient": { "id": "28:bot" },
            "conversation": { "id": "19:abc@thread.v2" }
        }"#;
        let out = activity_to_incoming(body, SERVICE_FALLBACK).expect("parse ok");
        assert!(out.is_none(), "non-message activity must yield None");
    }

    #[test]
    fn message_missing_from_id_errors() {
        let body = r#"{
            "type": "message",
            "id": "id3",
            "text": "anon",
            "from": { "name": "No Id" },
            "recipient": { "id": "28:bot" },
            "conversation": { "id": "19:abc@thread.v2", "conversationType": "personal" }
        }"#;
        let err = activity_to_incoming(body, SERVICE_FALLBACK).expect_err("missing from.id errors");
        assert!(matches!(err, MsTeamsError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn malformed_json_errors() {
        let err =
            activity_to_incoming("{ not json", SERVICE_FALLBACK).expect_err("bad json errors");
        assert!(matches!(err, MsTeamsError::Parse(_)), "got {err:?}");
    }
}
