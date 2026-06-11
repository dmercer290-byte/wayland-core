//! Bot Framework inbound JWT validation.
//!
//! Every inbound Bot Framework Activity arrives with an
//! `Authorization: Bearer <jwt>` header. That JWT is signed by Microsoft's
//! Bot Framework token service. Before the webhook host can trust (parse +
//! enqueue) the Activity body, the token MUST be validated:
//!
//! 1. **Signature** — verified against the RSA public key Azure publishes in
//!    its OpenID JWKS metadata, selected by the token's `kid` header.
//! 2. **Audience** — must equal this bot's Microsoft App ID (`self.app_id`).
//! 3. **Issuer** — must equal `https://api.botframework.com`.
//! 4. **Expiry / not-before** — enforced by `jsonwebtoken` (`validate_exp`
//!    defaults to true; we also validate `nbf`).
//!
//! ## Algorithm-confusion / `alg: none` defense
//!
//! The verifier hardcodes `Algorithm::RS256` and NEVER reads the algorithm
//! from the token header. This blocks the classic JWT attacks where an
//! attacker swaps the `alg` to `none` (no signature) or to a symmetric
//! algorithm (`HS256`) and signs with the *public* key as if it were an HMAC
//! secret. `jsonwebtoken` only accepts tokens whose `alg` is in
//! [`Validation::algorithms`], which we pin to `[RS256]`.
//!
//! ## JWKS caching
//!
//! The JWKS is fetched once and cached for 24h. On a `kid` we don't have
//! (key rotation), we force a single refetch before failing — Azure rotates
//! signing keys roughly daily, so a missing `kid` is the expected rotation
//! signal, not necessarily an attack.

use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::error::MsTeamsError;

/// Canonical Bot Framework token issuer.
pub const BF_ISSUER: &str = "https://api.botframework.com";
/// OpenID Connect metadata document — its `jwks_uri` points at the live JWKS.
pub const BF_OPENID_METADATA_URL: &str =
    "https://login.botframework.com/v1/.well-known/openidconfiguration";

/// How long a fetched JWKS is trusted before a refetch.
const JWKS_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Validated JWT claims we care about.
///
/// `jsonwebtoken` already validates `aud`, `iss`, `exp`, and `nbf` against the
/// [`Validation`] config, so we do NOT re-check them here. We only surface the
/// `serviceurl` claim (Bot Framework lowercases it) for the defense-in-depth
/// cross-check against the Activity body's `serviceUrl`.
#[derive(Debug, Deserialize)]
pub struct Claims {
    /// Bot Framework stamps the originating `serviceUrl` into the token,
    /// lowercased as `serviceurl`. Absent on some token shapes, so optional.
    #[serde(default)]
    pub serviceurl: Option<String>,
}

/// One RSA signing key extracted from the JWKS.
#[derive(Debug, Clone)]
struct Jwk {
    kid: String,
    /// Base64url RSA modulus.
    n: String,
    /// Base64url RSA exponent.
    e: String,
}

/// Cached JWKS plus the instant it was fetched (for TTL expiry).
#[derive(Debug)]
struct CachedJwks {
    fetched: Instant,
    keys: Vec<Jwk>,
}

/// OpenID metadata document — we only need the `jwks_uri`.
#[derive(Debug, Deserialize, Default)]
struct OpenIdMeta {
    #[serde(default)]
    jwks_uri: String,
}

/// A JWKS document: `{ "keys": [ ... ] }`.
#[derive(Debug, Deserialize, Default)]
struct JwkSet {
    #[serde(default)]
    keys: Vec<JwkJson>,
}

/// One raw JWK entry. Only RSA (`kty == "RSA"`) keys are retained.
#[derive(Debug, Deserialize, Default)]
struct JwkJson {
    #[serde(default)]
    kty: String,
    #[serde(default)]
    kid: String,
    #[serde(default)]
    n: String,
    #[serde(default)]
    e: String,
}

/// Bot Framework inbound JWT validator.
///
/// Holds the egress client (for JWKS fetches), the expected audience
/// (`app_id`), the OpenID metadata URL + issuer (overrideable for tests), and
/// a TTL-bounded JWKS cache.
pub struct BotFrameworkAuth {
    http: wcore_egress::EgressClient,
    /// Microsoft App ID — the JWT audience.
    app_id: String,
    /// OpenID metadata URL (defaults to [`BF_OPENID_METADATA_URL`]).
    metadata_url: String,
    /// Expected token issuer (defaults to [`BF_ISSUER`]).
    issuer: String,
    jwks: Mutex<Option<CachedJwks>>,
}

impl BotFrameworkAuth {
    /// Construct with production defaults (live Azure metadata + issuer).
    pub fn new(http: wcore_egress::EgressClient, app_id: String) -> Self {
        Self::with_endpoints(
            http,
            app_id,
            BF_OPENID_METADATA_URL.to_string(),
            BF_ISSUER.to_string(),
        )
    }

    /// Construct with explicit metadata URL + issuer — used by tests to point
    /// at a mock OpenID/JWKS server.
    #[doc(hidden)]
    pub fn with_endpoints(
        http: wcore_egress::EgressClient,
        app_id: String,
        metadata_url: String,
        issuer: String,
    ) -> Self {
        Self {
            http,
            app_id,
            metadata_url,
            issuer,
            jwks: Mutex::new(None),
        }
    }

    /// Validate an `Authorization` header value, returning the token claims.
    ///
    /// Steps:
    /// 1. Strip a case-insensitive `Bearer ` prefix (missing → [`MsTeamsError::Auth`]).
    /// 2. Decode the header and require a `kid` (absent → `Auth`).
    /// 3. Resolve the RSA signing key for that `kid` from the (cached) JWKS.
    /// 4. Verify signature + audience + issuer + expiry via RS256-only
    ///    [`validate_with_key`].
    pub async fn validate(&self, auth_header: &str) -> Result<Claims, MsTeamsError> {
        let token = strip_bearer(auth_header)
            .ok_or_else(|| MsTeamsError::Auth("missing or malformed Bearer token".to_string()))?;

        let header = decode_header(token)
            .map_err(|e| MsTeamsError::Auth(format!("malformed JWT header: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| MsTeamsError::Auth("JWT header missing kid".to_string()))?;

        let jwk = self.key_for_kid(&kid).await?;
        let key = DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
            .map_err(|e| MsTeamsError::Auth(format!("invalid RSA JWK for kid {kid}: {e}")))?;

        validate_with_key(token, &key, &self.app_id, &self.issuer)
    }

    /// Resolve the RSA signing key for `kid`.
    ///
    /// Cache hit (fresh AND contains the kid) returns immediately. Otherwise
    /// the JWKS is refetched and the cache replaced. As a key-rotation
    /// accommodation, a *fresh* cache that is merely **missing** the kid still
    /// forces ONE refetch before giving up — Azure rotates signing keys
    /// roughly daily and a brand-new kid is the expected signal.
    async fn key_for_kid(&self, kid: &str) -> Result<Jwk, MsTeamsError> {
        {
            let guard = self.jwks.lock().await;
            if let Some(cached) = guard.as_ref()
                && cached.fetched.elapsed() < JWKS_TTL
                && let Some(jwk) = cached.keys.iter().find(|k| k.kid == kid)
            {
                return Ok(jwk.clone());
            }
        }

        // Either no cache, an expired cache, or a fresh cache missing this kid
        // (rotation): refetch once, replace the cache, then look up the kid.
        let keys = self.fetch_jwks().await?;
        let found = keys.iter().find(|k| k.kid == kid).cloned();
        {
            let mut guard = self.jwks.lock().await;
            *guard = Some(CachedJwks {
                fetched: Instant::now(),
                keys,
            });
        }

        found.ok_or_else(|| MsTeamsError::Auth(format!("no signing key for kid {kid}")))
    }

    /// Fetch + parse the live JWKS: GET metadata → `jwks_uri` → GET JWKS →
    /// retain RSA keys.
    async fn fetch_jwks(&self) -> Result<Vec<Jwk>, MsTeamsError> {
        let meta_resp = self
            .http
            .get(&self.metadata_url)
            .send()
            .await
            .map_err(|e| MsTeamsError::Network(format!("openid metadata: {e}")))?;
        if !meta_resp.status().is_success() {
            return Err(MsTeamsError::Network(format!(
                "openid metadata HTTP {}",
                meta_resp.status().as_u16()
            )));
        }
        let meta: OpenIdMeta = meta_resp
            .json()
            .await
            .map_err(|e| MsTeamsError::Parse(format!("openid metadata: {e}")))?;
        if meta.jwks_uri.is_empty() {
            return Err(MsTeamsError::Parse(
                "openid metadata missing jwks_uri".to_string(),
            ));
        }

        let jwks_resp = self
            .http
            .get(&meta.jwks_uri)
            .send()
            .await
            .map_err(|e| MsTeamsError::Network(format!("jwks: {e}")))?;
        if !jwks_resp.status().is_success() {
            return Err(MsTeamsError::Network(format!(
                "jwks HTTP {}",
                jwks_resp.status().as_u16()
            )));
        }
        let set: JwkSet = jwks_resp
            .json()
            .await
            .map_err(|e| MsTeamsError::Parse(format!("jwks: {e}")))?;

        let keys = set
            .keys
            .into_iter()
            .filter(|k| k.kty == "RSA" && !k.kid.is_empty() && !k.n.is_empty() && !k.e.is_empty())
            .map(|k| Jwk {
                kid: k.kid,
                n: k.n,
                e: k.e,
            })
            .collect();
        Ok(keys)
    }
}

/// Strip a case-insensitive `Bearer ` prefix, returning the bare token.
///
/// Returns `None` if the prefix is absent or the remaining token is empty.
fn strip_bearer(header: &str) -> Option<&str> {
    let header = header.trim();
    // `Bearer ` is 7 ASCII bytes, so `..7` is always a char boundary here.
    header
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("Bearer "))?;
    let token = header[7..].trim();
    if token.is_empty() { None } else { Some(token) }
}

/// Verify a token against an already-resolved decoding key.
///
/// Pins the algorithm to **RS256 only** — the token's `alg` header is never
/// trusted, which blocks `alg: none` and HS256-confusion attacks.
/// `jsonwebtoken` enforces signature + `aud` + `iss` + `exp` + `nbf`; we do
/// not re-implement those checks.
fn validate_with_key(
    token: &str,
    key: &DecodingKey,
    app_id: &str,
    issuer: &str,
) -> Result<Claims, MsTeamsError> {
    // RS256-only: do NOT derive the algorithm from the (attacker-controlled)
    // token header. `Validation::new` sets `algorithms` to exactly [RS256].
    let mut v = Validation::new(Algorithm::RS256);
    v.set_audience(&[app_id]);
    v.set_issuer(&[issuer]);
    // Defaults: validate_exp = true. Enforce nbf too.
    v.validate_nbf = true;

    decode::<Claims>(token, key, &v)
        .map(|data| data.claims)
        .map_err(|e| MsTeamsError::Auth(format!("JWT validation failed: {e}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde::Serialize;

    // 2048-bit RSA test keypair (TEST ONLY — never used in production).
    const TEST_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCxK7iZ13vevXvm
p5s5s0R9epARhp6UlVe3gc2JJ0Gg/5pCrcNPmeprwkIPAsmrsyXb/8SiFNBMV/i9
TdywmkSSgaGIHdFw2H1Dt9xq7FP3QWl7sTZxqd0Rq8JoPCiPlY/Mm4qJOS0Vwiiy
odqorI2BDFJOtz4cZ1+Fyc1zqYt0rT5gLM/9OyqmvJtPc8rZ3af31yJSm/ComIHY
4P7m3Q94Q9Ep5Es1+wAECguGxs6bQSnCUBtHOcCCAo581z1ZJnobnq269OmyzhBL
oz9fNgwtMVcjHje3rOMRbSe36nR85Ahc/zaqngxnt0jdkuFBiiVbEdw2+gG2Qhub
sbbEH4xXAgMBAAECggEAAlPdt/+xu+pnX09iZa6qPq/GhsRq/u67WUjWR3ABl7jj
8O5Re5E9GC9UKNhTh/Lxk2NX1P1LA0XAmdQVCyjrr7UORziFEON3OdWHiswSClSM
qzhXy8R8iAfmpPHtYn2HhxugBU9//SIw4K/prH+f2EsuJaSYp0zgX2SYU2Wt1FmQ
tGxLKWPKF1q51wBsTMKC7XiC+wUZ213vvMAGVFKbFxC9Ldy1Z38QaDtWIIbbA7JB
Wv5GqBEzkcy8wqhn31IPuoeZRHA+VvvoiNFpX6cjmrtri9aO+S5zgE2HZ0IuYo1+
bzixKfhGdfuvomnOpa/V5edLQFiSkYQTISZsrBVu4QKBgQDmBM/8JbdNOK9S7Xdb
p8oD8tO/tFOC8oBZcWDoh5MlMtGuaUXxJ80Or5XayC+o/6IzUraEYTvlqO0p9X1C
4fOvqFg3GwMr3O8jlEKErRaikPfF6R1wtG3sNbjtcLc5JSXNl4RyVJKu1mLqV3dA
fOATBQe/K9Aqd+Rzg4bL9DGC5wKBgQDFLsOHhcOlyWLbC/rbqo0LMqvWHwCC+ta2
6v2JVcshX/V7eZci/Rer9t8fPiZMZNUPoo2Ay0Y2uOF9ryTlb8jtW/x9J6vva/Kx
CNyoiiDSmwc8aT9FwISiMVNixyzL7HGFyGpMnPmx7qWnVv+pPmzyAT5lsujAUrC3
fRg9BWrtEQKBgGNruxZKmxMmqClY+NlGCfxw7fOTlvEnrjB64B9B0mkmsRkI6bFV
ub1aSZR6KJeMfuheHQPVH1WiEXisYksRbQoE4rRW2aUQ5tBjGelNA1abAG2r2AzK
ACU0B02iBaAOnWtizV25jnlBsxmFWscl8phl+TY5Us24aqc/N3lagDgLAoGAR8nC
vjBhDpbHOuCdsCPjvdPw47/du9H/IhFjxQBLOBdrlEysTby/RYhXq1RBNUbwmwSf
Z+iZ44pj7hI56J5OFLyMrDQpUL2IWhPT4jiHwqVWeRQISSjSIQq8RRYmpQesPPy+
Vq4/6hvsi4QNCF0F5QW25efA/WQdmnAcxvqV90ECgYEA4idGNvUuLmxrS64aZBgo
H2favbG0pwxbCPvFEoX3Tviz1dwTTnQ1oA09E5TGqYASOtIrCZepPwJNg44cLpQK
iAqbJ0OTSSXcdHU8aEUGrIomxRhmIXBRGG3CgNgc4oRME1Cqc82WQvUsXkctMsB/
XBeQlzcfRANpSwysQGfZET8=
-----END PRIVATE KEY-----";

    const TEST_PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAsSu4mdd73r175qebObNE
fXqQEYaelJVXt4HNiSdBoP+aQq3DT5nqa8JCDwLJq7Ml2//EohTQTFf4vU3csJpE
koGhiB3RcNh9Q7fcauxT90Fpe7E2candEavCaDwoj5WPzJuKiTktFcIosqHaqKyN
gQxSTrc+HGdfhcnNc6mLdK0+YCzP/TsqprybT3PK2d2n99ciUpvwqJiB2OD+5t0P
eEPRKeRLNfsABAoLhsbOm0EpwlAbRznAggKOfNc9WSZ6G56tuvTpss4QS6M/XzYM
LTFXIx43t6zjEW0nt+p0fOQIXP82qp4MZ7dI3ZLhQYolWxHcNvoBtkIbm7G2xB+M
VwIDAQAB
-----END PUBLIC KEY-----";

    // A DIFFERENT 2048-bit key — used to forge a token the real key rejects.
    const OTHER_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDKHRFoQ5q3jRFS
ecQlH5y+BFT15s7tEkQ/uknX//NED0rq4h8oHMwF8B500kVn3aFkrtfokcWMxNIK
+IMmYmTOnZkR0LUWtD8gxojYP1ntT+lbBxNX3GuPwoK+SrTbWyQds6GrwhWwiWsM
7/tJNay01BqesvP7rOkXUbp6no5hf3+1xk0GzEPHPzWKXjr1WJ5jDWuezbV57gBG
2Q3htDuRYsQYRU1BPTRKSRMQqB6kF2m6X6ckaXLj+AA0ISG9Hq/AnOoVFSYazWBN
N873HquGYviX7Bw7gmFGmxc0bjiHqotC25PZcSShVex1UPuqn5A6UB/WaDWyl1l7
g06yDNflAgMBAAECggEAQsmtPWeNolb+4OK1AtF58b6rtqCBQ4z0OZzdFwAQyq5F
Au4a/p3Ze6LX5aGwZryxvvwaA9Pb1IMbp51sdUwxZKdmdCEkHi8M509D3DW/CTEN
e1OQvEltz9EmdCxqrEvnWNtJsuDNWwtl8R4CSzRt8ElgzI11G3cNhXOv7CImCahK
I5R0Z4Dntj3qsMvOOuowsH9mv/j+AZCXkLf0E6DW/RCGTBPL30OZ6GewCLvzqCI/
/r+NlIrc+g6r+/+teKDwNe9JPcBGUoXGe2N60MZ99Zwa2CSReM3nQz/D9B/CsoxL
yb1zgoyKckSzfrUbwo/unmV3SWJ24pMRWhUvtZgctwKBgQDwTAlmJQtgUJMWsKMB
ulYaLRAfm+mKJAu5j4wFnNOqB/+Oaagz5JsKfoCT20WsZxogsAD30xOnnUn5M+kx
k0LcfzSmar9iiSpstDinQS+hxXl9dpC0MyJpV7Kl5mMyDv3vS8yYPaKp1Y6FK1er
iNM2bUtxcTdJ4W2t2F0zKa1XxwKBgQDXUkEnlxIyPg91YJEcdi+05irjJ7v48rYF
5e8rrXzPbqR8jB//rh8RLcoWaTauN3VTSg33tsDkNpoAj/WIzzqK1ggp+mFY+2En
/G9QzBakL9nIh+SRtv7CHdxCPDbuKfVwn4oYSseF6nVZ7S2oD1x5sJmKQSnxkUVt
R7UkrcVK8wKBgQDm5Vk+sifNQ38ilVX8ag0kF9rfVJRCbcJqall0Zy4nuonAURwT
yP2FRurLqC25rFQ5xoUXnNXNAGE9OLlBLqxXbU+s/POrffuq+j1Z0VQwkKzddpky
3dOZ/2+k48y7JBay4lXUj50GrjLFGVGjfNTe/oQ4nD4xGpCmNDnR2KE8rwKBgGMU
oZCjLqdZ8WkUt5F+PPOkGkYOyauDnAjYxpa1rUISarQ5EpxntjoEdQKdBaFjOaTK
5eR//wDEs1bg5549pXWviXAvm84DVrC8s0hdsWl572AcUCxRJaeTcAA2jxxGyH87
mqMU/fz8Z2WrAyBbeTUx82UwGSnkrCreHVe0cp3LAoGAYmR5NxEcG/4h7MBSrtTc
wzUVBiX7ke9GUU50Xj6JFXx7uOlelL9sKD7EoHHofnYoJDQsFDsqIH8OXglMLLmb
3o+w6ii/Qht+XNxVaAQDJDnM0gJCP+Q5zqjXZkND1irezbnytiDi6jU6qAbrtGn2
ifbvjeUK+8dlmkIBgohnGFo=
-----END PRIVATE KEY-----";

    const TEST_APP_ID: &str = "11111111-2222-3333-4444-555555555555";

    #[derive(Serialize)]
    struct TestClaims {
        aud: String,
        iss: String,
        exp: i64,
        nbf: i64,
        serviceurl: String,
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    /// Sign a token with the given PEM private key + claims.
    fn sign(priv_pem: &str, claims: &TestClaims) -> String {
        let key = EncodingKey::from_rsa_pem(priv_pem.as_bytes()).expect("encode key");
        encode(&Header::new(Algorithm::RS256), claims, &key).expect("encode token")
    }

    fn valid_claims() -> TestClaims {
        TestClaims {
            aud: TEST_APP_ID.to_string(),
            iss: BF_ISSUER.to_string(),
            exp: now() + 3600,
            nbf: now() - 60,
            serviceurl: "https://smba.trafficmanager.net/amer/".to_string(),
        }
    }

    fn pub_key() -> DecodingKey {
        DecodingKey::from_rsa_pem(TEST_PUB_PEM.as_bytes()).expect("decode pub key")
    }

    #[test]
    fn validate_with_key_accepts_valid_token() {
        let token = sign(TEST_PRIV_PEM, &valid_claims());
        let claims =
            validate_with_key(&token, &pub_key(), TEST_APP_ID, BF_ISSUER).expect("valid token");
        assert_eq!(
            claims.serviceurl.as_deref(),
            Some("https://smba.trafficmanager.net/amer/")
        );
    }

    #[test]
    fn validate_with_key_rejects_wrong_audience() {
        let mut c = valid_claims();
        c.aud = "00000000-0000-0000-0000-000000000000".to_string();
        let token = sign(TEST_PRIV_PEM, &c);
        let err = validate_with_key(&token, &pub_key(), TEST_APP_ID, BF_ISSUER)
            .expect_err("wrong aud must fail");
        assert!(matches!(err, MsTeamsError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn validate_with_key_rejects_wrong_issuer() {
        let mut c = valid_claims();
        c.iss = "https://evil.example.com".to_string();
        let token = sign(TEST_PRIV_PEM, &c);
        let err = validate_with_key(&token, &pub_key(), TEST_APP_ID, BF_ISSUER)
            .expect_err("wrong iss must fail");
        assert!(matches!(err, MsTeamsError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn validate_with_key_rejects_expired_token() {
        let mut c = valid_claims();
        c.exp = now() - 3600;
        c.nbf = now() - 7200;
        let token = sign(TEST_PRIV_PEM, &c);
        let err = validate_with_key(&token, &pub_key(), TEST_APP_ID, BF_ISSUER)
            .expect_err("expired must fail");
        assert!(matches!(err, MsTeamsError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn validate_with_key_rejects_wrong_signature() {
        // Signed by a DIFFERENT key → signature check against TEST_PUB fails.
        let token = sign(OTHER_PRIV_PEM, &valid_claims());
        let err = validate_with_key(&token, &pub_key(), TEST_APP_ID, BF_ISSUER)
            .expect_err("bad signature must fail");
        assert!(matches!(err, MsTeamsError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn validate_with_key_rejects_alg_none() {
        // An `alg: none` token (no signature). The RS256-only validator must
        // refuse it regardless of the claims.
        let claims = valid_claims();
        // Hand-build header.payload. with empty signature.
        let header = serde_json::json!({ "alg": "none", "typ": "JWT" });
        let b64 = |v: &serde_json::Value| {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string().as_bytes())
        };
        let claims_json = serde_json::to_value(&claims).unwrap();
        let token = format!("{}.{}.", b64(&header), b64(&claims_json));
        let err = validate_with_key(&token, &pub_key(), TEST_APP_ID, BF_ISSUER)
            .expect_err("alg none must fail");
        assert!(matches!(err, MsTeamsError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn validate_with_key_rejects_hs256_confusion() {
        // Forge an HS256 token using the PUBLIC key bytes as the HMAC secret —
        // the classic algorithm-confusion attack. RS256-only must reject it.
        let key = EncodingKey::from_secret(TEST_PUB_PEM.as_bytes());
        let token =
            encode(&Header::new(Algorithm::HS256), &valid_claims(), &key).expect("hs256 encode");
        let err = validate_with_key(&token, &pub_key(), TEST_APP_ID, BF_ISSUER)
            .expect_err("hs256 confusion must fail");
        assert!(matches!(err, MsTeamsError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn strip_bearer_is_case_insensitive() {
        assert_eq!(strip_bearer("Bearer abc.def.ghi"), Some("abc.def.ghi"));
        assert_eq!(strip_bearer("bearer abc.def.ghi"), Some("abc.def.ghi"));
        assert_eq!(strip_bearer("BEARER abc.def.ghi"), Some("abc.def.ghi"));
        assert_eq!(strip_bearer("  Bearer  tok  "), Some("tok"));
        assert_eq!(strip_bearer("Basic abc"), None);
        assert_eq!(strip_bearer("Bearer "), None);
        assert_eq!(strip_bearer("abc.def.ghi"), None);
    }

    // --- JWKS path (mockito) -------------------------------------------------

    fn auth_for(metadata_url: String) -> BotFrameworkAuth {
        let http = wcore_egress::EgressClient::builder()
            .build()
            .unwrap_or_default();
        BotFrameworkAuth::with_endpoints(
            http,
            TEST_APP_ID.to_string(),
            metadata_url,
            BF_ISSUER.to_string(),
        )
    }

    fn jwks_body(kids: &[&str]) -> String {
        let keys: Vec<_> = kids
            .iter()
            .map(|kid| {
                serde_json::json!({
                    "kty": "RSA",
                    "use": "sig",
                    "kid": kid,
                    "n": "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM",
                    "e": "AQAB"
                })
            })
            .collect();
        serde_json::json!({ "keys": keys }).to_string()
    }

    #[tokio::test]
    async fn key_for_kid_selects_matching_kid_and_caches() {
        let mut server = mockito::Server::new_async().await;
        let keys_url = format!("{}/keys", server.url());

        let meta = server
            .mock("GET", "/openid")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(r#"{{"jwks_uri":"{keys_url}"}}"#))
            .expect(1) // exactly one metadata fetch — second lookup hits cache
            .create_async()
            .await;
        let keys = server
            .mock("GET", "/keys")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(jwks_body(&["kid-A", "kid-B"]))
            .expect(1)
            .create_async()
            .await;

        let auth = auth_for(format!("{}/openid", server.url()));

        let jwk = auth.key_for_kid("kid-B").await.expect("kid-B resolves");
        assert_eq!(jwk.kid, "kid-B");
        assert_eq!(jwk.e, "AQAB");

        // Second lookup of a cached kid must NOT refetch (mocks .expect(1)).
        let jwk_a = auth.key_for_kid("kid-A").await.expect("kid-A cached");
        assert_eq!(jwk_a.kid, "kid-A");

        meta.assert_async().await;
        keys.assert_async().await;
    }

    #[tokio::test]
    async fn key_for_kid_unknown_triggers_single_refetch_then_errors() {
        let mut server = mockito::Server::new_async().await;
        let keys_url = format!("{}/keys", server.url());

        // First call populates the (fresh) cache with kid-A only. The unknown
        // kid then forces exactly ONE more metadata+keys fetch before failing.
        let meta = server
            .mock("GET", "/openid")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(r#"{{"jwks_uri":"{keys_url}"}}"#))
            .expect(2)
            .create_async()
            .await;
        let keys = server
            .mock("GET", "/keys")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(jwks_body(&["kid-A"]))
            .expect(2)
            .create_async()
            .await;

        let auth = auth_for(format!("{}/openid", server.url()));

        // Prime the cache (fresh, contains kid-A).
        auth.key_for_kid("kid-A").await.expect("kid-A resolves");

        // Unknown kid → fresh cache missing it → one refetch → still missing → err.
        let err = auth
            .key_for_kid("kid-ZZZ")
            .await
            .expect_err("unknown kid errors");
        assert!(matches!(err, MsTeamsError::Auth(_)), "got {err:?}");

        meta.assert_async().await;
        keys.assert_async().await;
    }
}
