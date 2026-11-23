//! Browser-session authentication for the HTTP frontend carrier.
//!
//! The browser receives one process-local launch URL, exchanges its query token for an
//! authority-bound signed cookie, and then sends only that cookie on index requests. Static assets
//! remain public, while a page shell cannot be fetched by a request that did not complete the
//! exchange.

use std::time::Duration;

use axum::http::{HeaderMap, Method};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use rand::random;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

const COOKIE_PREFIX: &str = "dsh-auth-";
const TOKEN_QUERY: &str = "token";
const COOKIE_VERSION: u8 = 1;
const SECRET_BYTES: usize = 32;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct BrowserAuth {
    launch_token: String,
    secret: [u8; SECRET_BYTES],
    max_age: Duration,
}
#[derive(Debug, Deserialize, Serialize)]
struct CookiePayload {
    version: u8,
    authority: String,
    #[serde(rename = "issuedAt")]
    issued_at: u64,
    #[serde(rename = "expiresAt")]
    expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

/// What a `/connect` request should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectAuth {
    /// The launch token was exchanged for an API operator cookie.
    Redirect {
        /// Relative redirect target.
        location: &'static str,
        /// Signed cookie to set.
        set_cookie: String,
        /// The published operator identity carried by the cookie.
        peer_id: String,
    },
    /// `interactions` was repeated or not one of the two legal values.
    BadRequest {
        /// Plain-text explanation.
        body: String,
    },
    /// The launch token, method, or authority did not authenticate.
    Unauthorized {
        /// The response body; empty for HEAD.
        body: String,
    },
}

/// What an authenticated index request should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexAuth {
    /// The caller may read the index bytes.
    Serve,
    /// The launch token was exchanged and the browser should remove it from the address bar.
    Redirect {
        /// Relative redirect target.
        location: &'static str,
        /// Signed cookie to set.
        set_cookie: String,
    },
    /// The caller has no valid browser session.
    Unauthorized {
        /// The response body; empty for HEAD.
        body: String,
    },
}

impl BrowserAuth {
    /// Creates an authenticator with explicit values, useful for deterministic tests.
    ///
    /// `max_age` must be non-zero. The process launch token and signing secret are independent:
    /// knowing a cookie does not let a caller mint a new launch URL.
    ///
    /// # Panics
    ///
    /// Panics when `max_age` is zero.
    #[must_use]
    pub fn new(
        launch_token: impl Into<String>,
        secret: [u8; SECRET_BYTES],
        max_age: Duration,
    ) -> Self {
        assert!(
            !max_age.is_zero(),
            "browser auth cookie lifetime must be non-zero"
        );
        Self {
            launch_token: launch_token.into(),
            secret,
            max_age,
        }
    }

    /// Creates a process-local authenticator with a cryptographically random launch token and key.
    #[must_use]
    pub fn random(max_age: Duration) -> Self {
        Self::new(
            encode(&random::<[u8; SECRET_BYTES]>()),
            random::<[u8; SECRET_BYTES]>(),
            max_age,
        )
    }

    /// Adds this process's launch token to a clean browser URL.
    ///
    /// Existing query parameters are preserved; only `token` is replaced.
    pub fn authenticated_url(&self, base_url: &str) -> Result<String, url::ParseError> {
        let mut url = Url::parse(base_url)?;
        let query: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(name, _)| name != TOKEN_QUERY)
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in query {
                pairs.append_pair(&name, &value);
            }
            pairs.append_pair(TOKEN_QUERY, &self.launch_token);
        }
        Ok(url.into())
    }

    /// The index admission decision for one HTTP request.
    ///
    /// `target` is the request path plus query, `secure` describes the listener that received the
    /// request, and `now_ms` is supplied by the caller so expiry tests do not depend on sleeping.
    #[must_use]
    pub fn authorize_index(
        &self,
        method: &Method,
        target: &str,
        headers: &HeaderMap,
        secure: bool,
        now_ms: u64,
    ) -> IndexAuth {
        let url = Url::parse(&format!("http://dsh.invalid{target}"))
            .unwrap_or_else(|_| Url::parse("http://dsh.invalid/").expect("static URL"));
        let tokens: Vec<String> = url
            .query_pairs()
            .filter(|(name, _)| name == TOKEN_QUERY)
            .map(|(_, value)| value.into_owned())
            .collect();
        if !tokens.is_empty() {
            let authority = request_audience(headers, secure);
            if *method == Method::GET
                && url.path() == "/"
                && tokens.len() == 1
                && authority.is_some()
                && bool::from(tokens[0].as_bytes().ct_eq(self.launch_token.as_bytes()))
            {
                let authority = authority.expect("checked above");
                let expires_at = now_ms.saturating_add(self.max_age.as_millis() as u64);
                let value = self.encode_cookie(&CookiePayload {
                    version: COOKIE_VERSION,
                    authority: authority.clone(),
                    issued_at: now_ms,
                    expires_at,
                    id: None,
                });
                return IndexAuth::Redirect {
                    location: "./",
                    set_cookie: session_cookie(
                        &cookie_name(&authority),
                        &value,
                        expires_at,
                        self.max_age,
                        secure,
                    ),
                };
            }
            return unauthorized(method);
        }
        if self.is_authenticated(headers, secure, now_ms) {
            IndexAuth::Serve
        } else {
            unauthorized(method)
        }
    }

    /// Exchanges the launch token for an independent API operator.
    ///
    /// Omitted `interactions` means `none`. `publish` runs only after the token, method, path, and
    /// authority have all checked out, and it runs before the cookie is returned.
    pub fn authorize_connect<F>(
        &self,
        method: &Method,
        target: &str,
        headers: &HeaderMap,
        secure: bool,
        now_ms: u64,
        publish: F,
    ) -> ConnectAuth
    where
        F: FnOnce(girder_core::api::gateway::Interactions) -> String,
    {
        let url = Url::parse(&format!("http://dsh.invalid{target}"))
            .unwrap_or_else(|_| Url::parse("http://dsh.invalid/").expect("static URL"));
        let declarations: Vec<String> = url
            .query_pairs()
            .filter(|(name, _)| name == "interactions")
            .map(|(_, value)| value.into_owned())
            .collect();
        let interactions = match declarations.as_slice() {
            [] => girder_core::api::gateway::Interactions::None,
            [only] => match girder_core::api::gateway::Interactions::parse(only) {
                Some(interactions) => interactions,
                None => {
                    return ConnectAuth::BadRequest {
                        body: "interactions must be declared once as answer or none\n".to_owned(),
                    };
                }
            },
            _ => {
                return ConnectAuth::BadRequest {
                    body: "interactions must be declared once as answer or none\n".to_owned(),
                };
            }
        };
        let tokens: Vec<String> = url
            .query_pairs()
            .filter(|(name, _)| name == TOKEN_QUERY)
            .map(|(_, value)| value.into_owned())
            .collect();
        let authority = request_audience(headers, secure);
        if *method != Method::GET
            || url.path() != "/connect"
            || tokens.len() != 1
            || authority.is_none()
            || !bool::from(tokens[0].as_bytes().ct_eq(self.launch_token.as_bytes()))
        {
            return ConnectAuth::Unauthorized {
                body: unauthorized(method).body(),
            };
        }
        let authority = authority.expect("checked above");
        let peer_id = publish(interactions);
        let expires_at = now_ms.saturating_add(self.max_age.as_millis() as u64);
        let value = self.encode_cookie(&CookiePayload {
            version: 2,
            authority: authority.clone(),
            issued_at: now_ms,
            expires_at,
            id: Some(peer_id.clone()),
        });
        ConnectAuth::Redirect {
            location: "./api/remote.mux",
            set_cookie: session_cookie(
                &cookie_name(&authority),
                &value,
                expires_at,
                self.max_age,
                secure,
            ),
            peer_id,
        }
    }

    /// Checks an authority-bound browser cookie without changing any state.
    ///
    /// An API operator cookie is not a browser session: it must not open the index.
    #[must_use]
    pub fn is_authenticated(&self, headers: &HeaderMap, secure: bool, now_ms: u64) -> bool {
        self.payload(headers, secure, now_ms)
            .is_some_and(|payload| payload.version == COOKIE_VERSION)
    }

    /// Returns the operator id from a version-2 cookie, when the cookie is still valid.
    #[must_use]
    pub fn authenticate_api(
        &self,
        headers: &HeaderMap,
        secure: bool,
        now_ms: u64,
    ) -> Option<String> {
        let payload = self.payload(headers, secure, now_ms)?;
        (payload.version == 2).then_some(payload.id).flatten()
    }

    fn payload(&self, headers: &HeaderMap, secure: bool, now_ms: u64) -> Option<CookiePayload> {
        let authority = request_audience(headers, secure)?;
        let raw_cookie = headers.get("cookie")?.to_str().ok()?;
        let value = cookie_value(raw_cookie, &cookie_name(&authority))?;
        let payload = self.decode_cookie(value)?;
        self.fresh(&payload, &authority, now_ms).then_some(payload)
    }

    fn fresh(&self, payload: &CookiePayload, authority: &str, now_ms: u64) -> bool {
        payload.authority == authority
            && payload.issued_at <= now_ms
            && payload.expires_at > now_ms
            && payload.expires_at > payload.issued_at
            && payload.expires_at.saturating_sub(payload.issued_at)
                <= self.max_age.as_millis() as u64
    }

    fn encode_cookie(&self, payload: &CookiePayload) -> String {
        let body = serde_json::to_string(payload).expect("cookie payload is serializable");
        let encoded_body = encode(body.as_bytes());
        let signature = self.sign(&encoded_body);
        format!("v{}.{encoded_body}.{}", payload.version, encode(&signature))
    }

    fn sign(&self, body: &str) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key");
        mac.update(body.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    fn decode_cookie(&self, value: &str) -> Option<CookiePayload> {
        let mut parts = value.split('.');
        let version = parts.next()?;
        let body = parts.next()?;
        let signature = parts.next()?;
        let payload_version = match version {
            "v1" => 1,
            "v2" => 2,
            _ => return None,
        };
        if parts.next().is_some() {
            return None;
        }
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        let mut mac = HmacSha256::new_from_slice(&self.secret).ok()?;
        mac.update(body.as_bytes());
        mac.verify_slice(&signature).ok()?;
        let body = URL_SAFE_NO_PAD.decode(body).ok()?;
        let payload: CookiePayload = serde_json::from_slice(&body).ok()?;
        if payload.version != payload_version {
            return None;
        }
        match payload.version {
            1 if payload.id.is_none() => Some(payload),
            2 if payload.id.as_deref().is_some_and(|id| !id.is_empty()) => Some(payload),
            _ => None,
        }
    }
}

fn encode(value: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(value)
}

fn cookie_name(authority: &str) -> String {
    let digest = Sha256::digest(authority.as_bytes());
    format!("{COOKIE_PREFIX}{}", encode(&digest))
}

fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|segment| {
        let (key, value) = segment.split_once('=')?;
        (key.trim() == name).then_some(value.trim())
    })
}

fn request_audience(headers: &HeaderMap, secure: bool) -> Option<String> {
    let host = headers.get("host")?.to_str().ok()?;
    let url = Url::parse(&format!(
        "{}//{host}",
        if secure { "https:" } else { "http:" }
    ))
    .ok()?;
    let host = url.host_str()?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };
    if secure {
        Some(format!("https://{authority}"))
    } else {
        Some(authority)
    }
}

fn unauthorized(method: &Method) -> IndexAuth {
    IndexAuth::Unauthorized {
        body: unauthorized_body(method),
    }
}

fn unauthorized_body(method: &Method) -> String {
    if *method == Method::HEAD {
        String::new()
    } else {
        "dsh web authentication required; reopen the URL printed by dsh web.\n".to_owned()
    }
}

impl IndexAuth {
    fn body(self) -> String {
        match self {
            Self::Unauthorized { body } => body,
            Self::Serve | Self::Redirect { .. } => String::new(),
        }
    }
}

fn session_cookie(
    name: &str,
    value: &str,
    expires_at: u64,
    max_age: Duration,
    secure: bool,
) -> String {
    format!(
        "{name}={value}; Max-Age={}; Path=/; Expires={}; HttpOnly; SameSite=Strict{}",
        max_age.as_secs(),
        http_date(expires_at),
        if secure { "; Secure" } else { "" },
    )
}

fn http_date(epoch_ms: u64) -> String {
    // The wire only needs a valid Expires value. `httpdate` is already a transitive HTTP dependency,
    // but keeping the conversion here avoids exposing another public type.
    let time = std::time::UNIX_EPOCH + Duration::from_millis(epoch_ms);
    httpdate::fmt_http_date(time)
}

#[cfg(test)]
mod tests {
    use super::{BrowserAuth, IndexAuth};
    use axum::http::{HeaderMap, HeaderValue, Method};
    use std::time::Duration;

    fn headers(cookie: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("127.0.0.1:8787"));
        if let Some(cookie) = cookie {
            headers.insert("cookie", cookie.parse().expect("cookie header"));
        }
        headers
    }

    #[test]
    fn launch_url_exchanges_for_an_authority_bound_cookie() {
        let auth = BrowserAuth::new("launch-token", [7; 32], Duration::from_secs(60));
        let url = auth
            .authenticated_url("http://127.0.0.1:8787/")
            .expect("valid base URL");
        assert!(url.ends_with("?token=launch-token"), "{url}");

        let exchange = auth.authorize_index(
            &Method::GET,
            "/?token=launch-token",
            &headers(None),
            false,
            1_000,
        );
        let IndexAuth::Redirect { set_cookie, .. } = exchange else {
            panic!("the launch token must exchange for a cookie");
        };
        let cookie = set_cookie.split(';').next().expect("cookie pair");
        assert!(auth.is_authenticated(&headers(Some(cookie)), false, 1_001));
        assert!(matches!(
            auth.authorize_index(&Method::GET, "/", &headers(Some(cookie)), false, 1_001),
            IndexAuth::Serve
        ));
    }

    #[test]
    fn cookie_is_bound_to_authority_and_expires() {
        let auth = BrowserAuth::new("launch-token", [7; 32], Duration::from_secs(60));
        let exchange = auth.authorize_index(
            &Method::GET,
            "/?token=launch-token",
            &headers(None),
            false,
            1_000,
        );
        let IndexAuth::Redirect { set_cookie, .. } = exchange else {
            panic!("the launch token must exchange");
        };
        let cookie = set_cookie.split(';').next().expect("cookie pair");

        let mut other = headers(Some(cookie));
        other.insert("host", HeaderValue::from_static("127.0.0.1:8788"));
        assert!(!auth.is_authenticated(&other, false, 1_001));
        assert!(!auth.is_authenticated(&headers(Some(cookie)), false, 61_001));
    }

    #[test]
    fn invalid_or_repeated_launch_tokens_are_unauthorized() {
        let auth = BrowserAuth::new("launch-token", [7; 32], Duration::from_secs(60));
        for target in [
            "/",
            "/?token=wrong",
            "/?token=launch-token&token=launch-token",
        ] {
            assert!(matches!(
                auth.authorize_index(&Method::GET, target, &headers(None), false, 1_000),
                IndexAuth::Unauthorized { .. }
            ));
        }
        assert!(matches!(
            auth.authorize_index(
                &Method::HEAD,
                "/?token=launch-token",
                &headers(None),
                false,
                1_000
            ),
            IndexAuth::Unauthorized { .. }
        ));
    }

    #[test]
    fn connect_defaults_to_none_and_an_api_cookie_does_not_open_the_index() {
        let auth = BrowserAuth::new("launch-token", [7; 32], Duration::from_secs(60));
        let exchange = auth.authorize_connect(
            &Method::GET,
            "/connect?token=launch-token",
            &headers(None),
            false,
            1_000,
            |_| "peer-1".to_owned(),
        );
        let super::ConnectAuth::Redirect {
            location,
            set_cookie,
            peer_id,
        } = exchange
        else {
            panic!("a launch token must exchange for an operator");
        };
        assert_eq!(location, "./api/remote.mux");
        assert_eq!(peer_id, "peer-1");
        let cookie = set_cookie.split(';').next().expect("cookie pair");
        assert_eq!(
            auth.authenticate_api(&headers(Some(cookie)), false, 1_001)
                .as_deref(),
            Some("peer-1")
        );
        assert!(!auth.is_authenticated(&headers(Some(cookie)), false, 1_001));
        assert!(matches!(
            auth.authorize_connect(
                &Method::GET,
                "/connect?token=launch-token&interactions=observe",
                &headers(None),
                false,
                1_000,
                |_| panic!("an illegal declaration must not publish"),
            ),
            super::ConnectAuth::BadRequest { .. }
        ));
    }
}
