//! Inbound webhook HTTP host.
//!
//! Stands up an `axum` listener that accepts platform webhook deliveries
//! (Slack Events API, WhatsApp Cloud API, Twilio SMS) and routes each one
//! to the matching channel's signature-verifying
//! [`Channel::ingest_webhook`](wcore_channels::Channel::ingest_webhook) via
//! [`ChannelManager::ingest_webhook`]. The host owns NO signature logic of
//! its own: it normalizes the live HTTP request into a
//! [`WebhookRequest`] and lets the connector verify → parse → enqueue, then
//! writes the connector's [`WebhookResponse`] back.
//!
//! Routes:
//!   * `GET  /webhooks/:channel` — Meta (WhatsApp) `hub.challenge` handshake.
//!   * `POST /webhooks/:channel` — runtime delivery for every connector.
//!   * `GET  /healthz`           — liveness probe (`200 "ok"`).
//!
//! Security posture: the listener binds loopback (`127.0.0.1:8787`) by
//! default — operators front it with a TLS-terminating reverse proxy for
//! public exposure. Every request is gated by the connector's platform
//! signature check; an unknown channel is a `404`, a signature/header
//! failure a `401`, any other rejection a `400`. Denials are logged
//! content-free (channel name + error only — never the body or headers).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::{RwLock, watch};
use wcore_channels::{ChannelError, ChannelManager, WebhookRequest, WebhookResponse};
use wcore_config::config::InboundWebhookConfig;

/// Shared handle to the engine's channel registry.
///
/// `RwLock` (not `Mutex`): the read-path router methods (`ingest_webhook`,
/// `send_to`, …) take `&self`, so concurrent webhooks for *different* channels
/// acquire a shared read guard and run in parallel — only the mutating
/// lifecycle ops (`register`/`start_all`/`stop_all`) take a write guard.
/// Same-channel ordering is still serialized by the inner per-slot `Mutex`.
type SharedManager = Arc<RwLock<ChannelManager>>;

/// Host state threaded through every request.
#[derive(Clone)]
struct HostState {
    manager: SharedManager,
    /// Public base URL (scheme + host) the platform calls. Set when the
    /// host sits behind a proxy so signature schemes that sign the URL
    /// (Twilio) verify against the real public URL rather than the local
    /// bind address.
    public_base_url: Option<String>,
}

/// Build the request seam exactly as the live handler does, as a pure
/// function so it can be unit-tested without a socket.
///
/// `path_and_query` is the request target (path plus any `?query`).
/// `host` is the `Host` header value (used only when `public_base_url` is
/// `None`). `headers` and `query` are pre-extracted pairs.
fn build_request(
    method: &Method,
    path_and_query: &str,
    host: Option<&str>,
    public_base_url: Option<&str>,
    headers: Vec<(String, String)>,
    query: Vec<(String, String)>,
    body: String,
) -> WebhookRequest {
    let full_url = match public_base_url {
        Some(base) => format!("{}{}", base.trim_end_matches('/'), path_and_query),
        // No configured public URL: reconstruct from the Host header.
        // Twilio signature verification REQUIRES `public_base_url` to be
        // set to the exact public https URL — behind a proxy the local
        // scheme/host differ from what the platform signed, so this
        // best-effort `http://` reconstruction will not match.
        None => {
            let host = host.unwrap_or("localhost");
            format!("http://{host}{path_and_query}")
        }
    };

    WebhookRequest {
        method: method.as_str().to_uppercase(),
        full_url,
        headers,
        query,
        body,
    }
}

/// Lowercase every header name and keep only UTF-8 values (signature
/// headers are ASCII; a non-UTF-8 value cannot be a valid signature).
fn collect_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_ascii_lowercase(), v.to_string()))
        })
        .collect()
}

/// Parse a URI query string into key/value pairs with percent-decoding.
fn parse_query(query: Option<&str>) -> Vec<(String, String)> {
    match query {
        Some(q) => url::form_urlencoded::parse(q.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
        None => Vec::new(),
    }
}

/// Map a connector result onto an HTTP response.
fn response_for(channel: &str, result: Result<WebhookResponse, ChannelError>) -> Response {
    match result {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, resp.body.unwrap_or_default()).into_response()
        }
        // Unknown channel — the manager surfaces this as `Config`.
        Err(e @ ChannelError::Config(_)) => {
            tracing::warn!(channel, error = %e, "inbound webhook: unknown channel");
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e @ ChannelError::Auth(_)) => {
            tracing::warn!(channel, error = %e, "inbound webhook: auth rejected");
            StatusCode::UNAUTHORIZED.into_response()
        }
        Err(e) => {
            tracing::warn!(channel, error = %e, "inbound webhook: rejected");
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

/// Shared handler for both `GET` and `POST /webhooks/:channel`.
async fn handle_webhook(
    State(state): State<HostState>,
    Path(channel): Path<String>,
    method: Method,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    // Signature verification runs over the exact received bytes; a
    // non-UTF-8 body can never be a valid signed payload, so reject early.
    let body = match String::from_utf8(body.to_vec()) {
        Ok(b) => b,
        Err(_) => {
            tracing::warn!(channel, "inbound webhook: non-UTF-8 body");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());

    let req = build_request(
        &method,
        path_and_query,
        host,
        state.public_base_url.as_deref(),
        collect_headers(&headers),
        parse_query(uri.query()),
        body,
    );

    let result = state
        .manager
        .read()
        .await
        .ingest_webhook(&channel, &req)
        .await;
    response_for(&channel, result)
}

/// Liveness probe.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Build the router. Exposed for tests that drive it via `tower::oneshot`.
fn router(state: HostState) -> Router {
    Router::new()
        .route(
            "/webhooks/:channel",
            get(handle_webhook).post(handle_webhook),
        )
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// Serve the inbound webhook host until `shutdown` flips to `true`.
///
/// Binds `bind`, routes `GET`/`POST /webhooks/:channel` to the matching
/// channel's `ingest_webhook`, and shuts down gracefully when the watch
/// channel reports `true`.
pub async fn serve(
    manager: SharedManager,
    bind: SocketAddr,
    public_base_url: Option<String>,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(HostState {
        manager,
        public_base_url,
    });
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "inbound webhook host listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut sd = shutdown;
            // Wait until the flag is observed `true`; a closed sender
            // (engine teardown) also ends the wait.
            let _ = sd.wait_for(|v| *v).await;
        })
        .await
}

/// Spawn the host from config, returning the join handle + shutdown sender.
///
/// Returns `None` when the host is disabled or `config.bind` does not parse
/// as a socket address (logged, non-fatal — the engine runs without it).
pub fn spawn(
    manager: SharedManager,
    config: &InboundWebhookConfig,
) -> Option<(tokio::task::JoinHandle<()>, watch::Sender<bool>)> {
    if !config.enabled {
        return None;
    }
    let bind: SocketAddr = match config.bind.parse() {
        Ok(addr) => addr,
        Err(e) => {
            tracing::error!(
                bind = %config.bind,
                error = %e,
                "inbound webhook host disabled: invalid bind address"
            );
            return None;
        }
    };

    let (tx, rx) = watch::channel(false);
    let public_base_url = config.public_base_url.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = serve(manager, bind, public_base_url, rx).await {
            tracing::error!(%bind, error = %e, "inbound webhook host exited with error");
        }
    });
    Some((handle, tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_lc() -> Vec<(String, String)> {
        vec![
            ("x-slack-signature".to_string(), "v0=abc".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]
    }

    #[test]
    fn build_request_uppercases_method() {
        let req = build_request(
            &Method::POST,
            "/webhooks/slack",
            Some("example.com"),
            None,
            vec![],
            vec![],
            "{}".to_string(),
        );
        assert_eq!(req.method, "POST");
    }

    #[test]
    fn build_request_uses_public_base_url_and_strips_trailing_slash() {
        let req = build_request(
            &Method::POST,
            "/webhooks/twilio?Foo=Bar",
            Some("ignored.local"),
            Some("https://public.example.com/"),
            vec![],
            vec![],
            String::new(),
        );
        // Trailing slash on the base is trimmed; path+query is appended verbatim.
        assert_eq!(
            req.full_url,
            "https://public.example.com/webhooks/twilio?Foo=Bar"
        );
    }

    #[test]
    fn build_request_reconstructs_from_host_when_no_public_base() {
        let req = build_request(
            &Method::POST,
            "/webhooks/twilio",
            Some("relay.internal:8787"),
            None,
            vec![],
            vec![],
            String::new(),
        );
        assert_eq!(req.full_url, "http://relay.internal:8787/webhooks/twilio");
    }

    #[test]
    fn build_request_defaults_host_to_localhost() {
        let req = build_request(
            &Method::GET,
            "/webhooks/wa",
            None,
            None,
            vec![],
            vec![],
            String::new(),
        );
        assert_eq!(req.full_url, "http://localhost/webhooks/wa");
    }

    #[test]
    fn build_request_preserves_lowercased_headers_and_lookup() {
        let req = build_request(
            &Method::POST,
            "/webhooks/slack",
            Some("h"),
            None,
            headers_lc(),
            vec![],
            "body".to_string(),
        );
        // Headers were already lowercased by the collector; case-insensitive
        // lookup still resolves them.
        assert_eq!(req.header("X-Slack-Signature"), Some("v0=abc"));
        assert_eq!(req.body, "body");
    }

    #[test]
    fn parse_query_percent_decodes_pairs() {
        let q = parse_query(Some("hub.mode=subscribe&hub.challenge=ab%20cd"));
        assert_eq!(q.len(), 2);
        let req = WebhookRequest {
            query: q,
            ..Default::default()
        };
        assert_eq!(req.query_get("hub.mode"), Some("subscribe"));
        // %20 decodes to a space.
        assert_eq!(req.query_get("hub.challenge"), Some("ab cd"));
    }

    #[test]
    fn parse_query_none_is_empty() {
        assert!(parse_query(None).is_empty());
    }

    #[test]
    fn collect_headers_lowercases_names() {
        let mut hm = HeaderMap::new();
        hm.insert("X-Slack-Signature", "v0=abc".parse().unwrap());
        let pairs = collect_headers(&hm);
        assert_eq!(
            pairs,
            vec![("x-slack-signature".to_string(), "v0=abc".to_string())]
        );
    }

    #[test]
    fn response_for_unknown_channel_is_404() {
        let resp = response_for(
            "nope",
            Err(ChannelError::Config("unknown channel: nope".to_string())),
        );
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn response_for_auth_is_401() {
        let resp = response_for("slack", Err(ChannelError::Auth("bad sig".to_string())));
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn response_for_rejected_is_400() {
        let resp = response_for(
            "slack",
            Err(ChannelError::Rejected("malformed".to_string())),
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn response_for_challenge_echoes_body_with_200() {
        let resp = response_for("wa", Ok(WebhookResponse::challenge("12345")));
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn spawn_returns_none_when_disabled() {
        let mgr = Arc::new(RwLock::new(ChannelManager::new()));
        let cfg = InboundWebhookConfig::default(); // enabled = false
        assert!(spawn(mgr, &cfg).is_none());
    }

    #[test]
    fn spawn_returns_none_on_bad_bind() {
        let mgr = Arc::new(RwLock::new(ChannelManager::new()));
        let cfg = InboundWebhookConfig {
            enabled: true,
            bind: "not-an-address".to_string(),
            public_base_url: None,
        };
        assert!(spawn(mgr, &cfg).is_none());
    }
}
