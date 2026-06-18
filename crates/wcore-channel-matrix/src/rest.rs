//! Matrix CS API REST helpers.
//!
//! Implements the send path: `PUT /_matrix/client/v3/rooms/{roomId}/send/m.room.message/{txnId}`.
//! Transaction IDs use a process-local counter (monotonic u64) to make retries idempotent.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::MatrixError;

static TXN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Hard cap on a single media download. The `mxc://` URI is attacker-controlled
/// (it arrives on an inbound message), so the body is streamed with a byte cap
/// to prevent an OOM-DoS from a homeserver that omits/lies about
/// `Content-Length`. Matches the 100 MiB cap used by the Discord/Slack/Telegram
/// media paths.
const MAX_MEDIA_BYTES: usize = 100 * 1024 * 1024;

/// Wall-clock timeout for a media download request (including the body read).
const MEDIA_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Serialize)]
struct TextMessageBody<'a> {
    msgtype: &'a str,
    body: &'a str,
}

#[derive(Deserialize)]
struct SendEventResponse {
    event_id: String,
}

/// Send a plain-text `m.room.message` to `room_id` and return the server-assigned `event_id`.
pub async fn send_text_message(
    http: &wcore_egress::EgressClient,
    api_base: &str,
    access_token: &str,
    room_id: &str,
    body: &str,
) -> Result<String, MatrixError> {
    let txn_id = TXN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let encoded_room = urlencoding::encode(room_id);
    let url =
        format!("{api_base}/_matrix/client/v3/rooms/{encoded_room}/send/m.room.message/{txn_id}");

    let payload = TextMessageBody {
        msgtype: "m.text",
        body,
    };

    let resp = http
        .put(&url)
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|e| MatrixError::Network(e.to_string()))?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(MatrixError::Http { status, body: text });
    }

    let result: SendEventResponse = resp
        .json()
        .await
        .map_err(|e| MatrixError::Parse(e.to_string()))?;

    Ok(result.event_id)
}

#[derive(Serialize)]
struct TypingBody {
    typing: bool,
    timeout: u64,
}

/// Send a typing notification: `PUT /_matrix/client/v3/rooms/{room}/typing/{userId}`.
/// `timeout_ms` is how long the server should show the indicator before
/// auto-clearing it; the subscriber re-sends on a shorter cadence.
pub async fn send_typing(
    http: &wcore_egress::EgressClient,
    api_base: &str,
    access_token: &str,
    room_id: &str,
    user_id: &str,
    timeout_ms: u64,
) -> Result<(), MatrixError> {
    let encoded_room = urlencoding::encode(room_id);
    let encoded_user = urlencoding::encode(user_id);
    let url = format!("{api_base}/_matrix/client/v3/rooms/{encoded_room}/typing/{encoded_user}");
    let payload = TypingBody {
        typing: true,
        timeout: timeout_ms,
    };
    let resp = http
        .put(&url)
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|e| MatrixError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if resp.status().is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(MatrixError::Http { status, body })
    }
}

#[derive(Serialize)]
struct ReactionBody<'a> {
    #[serde(rename = "m.relates_to")]
    relates_to: RelatesTo<'a>,
}

#[derive(Serialize)]
struct RelatesTo<'a> {
    rel_type: &'a str,
    event_id: &'a str,
    key: &'a str,
}

/// Send an `m.reaction` annotation relating to `event_id` with `emoji` as
/// the key: `PUT /_matrix/client/v3/rooms/{room}/send/m.reaction/{txnId}`.
pub async fn send_reaction(
    http: &wcore_egress::EgressClient,
    api_base: &str,
    access_token: &str,
    room_id: &str,
    event_id: &str,
    emoji: &str,
) -> Result<(), MatrixError> {
    let txn_id = TXN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let encoded_room = urlencoding::encode(room_id);
    let url = format!("{api_base}/_matrix/client/v3/rooms/{encoded_room}/send/m.reaction/{txn_id}");
    let payload = ReactionBody {
        relates_to: RelatesTo {
            rel_type: "m.annotation",
            event_id,
            key: emoji,
        },
    };
    let resp = http
        .put(&url)
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|e| MatrixError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if resp.status().is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(MatrixError::Http { status, body })
    }
}

/// Split an `mxc://server/mediaId` URI into `(server, mediaId)`.
fn parse_mxc(mxc: &str) -> Result<(&str, &str), MatrixError> {
    let rest = mxc
        .strip_prefix("mxc://")
        .ok_or_else(|| MatrixError::Parse(format!("not an mxc URI: {mxc}")))?;
    rest.split_once('/')
        .filter(|(s, m)| !s.is_empty() && !m.is_empty())
        .ok_or_else(|| MatrixError::Parse(format!("malformed mxc URI: {mxc}")))
}

/// Download unencrypted Matrix media by its `mxc://server/id` URI via the
/// authenticated media endpoint (Matrix v1.11+ / MSC3916):
/// `GET /_matrix/client/v1/media/download/{server}/{mediaId}` with the access
/// token. Replaces the deprecated unauthenticated `/_matrix/media/v3/download`.
pub async fn download_media(
    http: &wcore_egress::EgressClient,
    api_base: &str,
    access_token: &str,
    mxc: &str,
) -> Result<Vec<u8>, MatrixError> {
    let (server, media_id) = parse_mxc(mxc)?;
    let url = format!(
        "{api_base}/_matrix/client/v1/media/download/{}/{}",
        urlencoding::encode(server),
        urlencoding::encode(media_id),
    );
    let resp = http
        .get(&url)
        .bearer_auth(access_token)
        .timeout(MEDIA_DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|e| MatrixError::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(MatrixError::Http { status, body });
    }
    // Stream the body with a hard cap so a homeserver that omits/lies about
    // Content-Length on an attacker-supplied mxc:// URI cannot OOM the host.
    let bytes = wcore_egress::read_body_capped(resp, MAX_MEDIA_BYTES)
        .await
        .map_err(|e| MatrixError::Network(format!("media body read: {e}")))?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Minimal urlencoding without adding a dep (percent-encode room IDs).
// ---------------------------------------------------------------------------
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 4);
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                _ => {
                    out.push('%');
                    out.push(
                        char::from_digit((byte >> 4) as u32, 16)
                            .unwrap()
                            .to_ascii_uppercase(),
                    );
                    out.push(
                        char::from_digit((byte & 0xf) as u32, 16)
                            .unwrap()
                            .to_ascii_uppercase(),
                    );
                }
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn encodes_exclamation_and_colon() {
            let encoded = encode("!room:example.org");
            assert_eq!(encoded, "%21room%3Aexample.org");
        }
    }
}

#[cfg(test)]
mod ack_tests {
    use super::*;

    const TOKEN: &str = "syt_test";
    const ROOM: &str = "!room123:example.org";

    #[tokio::test]
    async fn send_typing_puts_to_typing_endpoint() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock(
                "PUT",
                "/_matrix/client/v3/rooms/%21room123%3Aexample.org/typing/%40bot%3Aexample.org",
            )
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        let http = wcore_egress::EgressClient::new();
        send_typing(
            &http,
            &server.url(),
            TOKEN,
            ROOM,
            "@bot:example.org",
            30_000,
        )
        .await
        .expect("typing should succeed on 200");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn send_reaction_puts_annotation_event() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock(
                "PUT",
                mockito::Matcher::Regex(
                    r"/_matrix/client/v3/rooms/[^/]+/send/m\.reaction/\d+".to_string(),
                ),
            )
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex(r#""rel_type":"m\.annotation""#.to_string()),
                mockito::Matcher::Regex(r#""event_id":"\$evt1""#.to_string()),
                mockito::Matcher::Regex(r#""key":"👀""#.to_string()),
            ]))
            .with_status(200)
            .with_body(r#"{"event_id":"$react1"}"#)
            .create_async()
            .await;
        let http = wcore_egress::EgressClient::new();
        send_reaction(&http, &server.url(), TOKEN, ROOM, "$evt1", "👀")
            .await
            .expect("reaction should succeed on 200");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn reaction_http_error_surfaces() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("PUT", mockito::Matcher::Any)
            .with_status(403)
            .with_body(r#"{"errcode":"M_FORBIDDEN"}"#)
            .create_async()
            .await;
        let http = wcore_egress::EgressClient::new();
        let err = send_reaction(&http, &server.url(), TOKEN, ROOM, "$evt1", "👀")
            .await
            .expect_err("403 should error");
        assert!(
            matches!(err, MatrixError::Http { status: 403, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_mxc_splits_server_and_id() {
        assert_eq!(
            parse_mxc("mxc://ex.org/abc123").unwrap(),
            ("ex.org", "abc123")
        );
        assert!(parse_mxc("https://ex.org/x").is_err());
        assert!(parse_mxc("mxc://ex.org/").is_err());
        assert!(parse_mxc("mxc://").is_err());
    }

    #[tokio::test]
    async fn download_media_uses_authenticated_endpoint() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/_matrix/client/v1/media/download/ex.org/abc123")
            .match_header("authorization", format!("Bearer {TOKEN}").as_str())
            .with_status(200)
            .with_body(b"\x89PNG\r\n\x1a\nmatrixpng".as_slice())
            .create_async()
            .await;
        let http = wcore_egress::EgressClient::new();
        let bytes = download_media(&http, &server.url(), TOKEN, "mxc://ex.org/abc123")
            .await
            .expect("download should succeed");
        assert_eq!(&bytes[..4], b"\x89PNG");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn download_media_http_error_surfaces() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(404)
            .create_async()
            .await;
        let http = wcore_egress::EgressClient::new();
        let err = download_media(&http, &server.url(), TOKEN, "mxc://ex.org/x")
            .await
            .expect_err("404 should error");
        assert!(
            matches!(err, MatrixError::Http { status: 404, .. }),
            "got {err:?}"
        );
    }
}
