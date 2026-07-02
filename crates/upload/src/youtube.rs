//! YouTube upload client (docs/adr/0011): OAuth 2.0 loopback login (PKCE) plus the Data API v3
//! resumable upload for `videos.insert`.
//!
//! Like the ganked.tv client, this is transport-only: the recorder decides what to upload and
//! when. Errors are mapped to user-actionable variants (quota, upload limit, expired login)
//! rather than raw Google error JSON.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use crate::Visibility;

/// Compile-time default OAuth client id (a desktop-app client id is not a secret; Google's
/// installed-app model embeds it). Empty when the build didn't provide one.
pub const DEFAULT_CLIENT_ID: &str = match option_env!("REWYND_YT_CLIENT_ID") {
    Some(v) => v,
    None => "",
};
/// Compile-time default OAuth client secret. For desktop apps Google treats this as
/// non-confidential; it still stays out of logs.
pub const DEFAULT_CLIENT_SECRET: &str = match option_env!("REWYND_YT_CLIENT_SECRET") {
    Some(v) => v,
    None => "",
};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const UPLOAD_URL: &str = "https://www.googleapis.com/upload/youtube/v3/videos";
/// The minimal scope for `videos.insert`.
const SCOPE: &str = "https://www.googleapis.com/auth/youtube.upload";

// Timeout discipline is shared with the ganked.tv client so the destinations can't drift.
use crate::{API_TIMEOUT, CONNECT_TIMEOUT, PUT_BASE_TIMEOUT, PUT_MIN_RATE_BYTES_PER_SEC};
/// How long the loopback listener waits for the browser redirect before giving up.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh the access token when it has less than this left, so it can't expire mid-upload
/// initiation.
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(60);
/// YouTube's documented per-file cap (256 GB); pre-checked so an oversized file fails fast.
const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024 * 1024;

/// Errors from the YouTube login and upload flows.
#[derive(Debug, Error)]
pub enum YouTubeError {
    #[error("could not read the clip: {0}")]
    Io(#[from] std::io::Error),
    #[error("clip is {} MB; YouTube accepts at most {} GB", size.div_ceil(1_000_000), max >> 30)]
    TooLarge { size: u64, max: u64 },
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("YouTube rejected the request (HTTP {status}, {reason}): {message}")]
    Api {
        status: u16,
        reason: String,
        message: String,
    },
    #[error("the YouTube API quota for today is used up; try again after midnight Pacific time")]
    QuotaExceeded,
    #[error("this channel has reached its YouTube upload limit for now; try again later")]
    UploadLimitExceeded,
    #[error("the YouTube login has expired; log in with YouTube again in the settings")]
    NeedsReauth,
    #[error("the login attempt timed out; start again")]
    LoginExpired,
    #[error("the login attempt failed: {0}")]
    LoginFailed(String),
    #[error("YouTube did not return an upload session URL")]
    NoUploadUrl,
}

/// An uploaded video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedVideo {
    pub id: String,
}

impl UploadedVideo {
    /// The public watch URL, or `None` if the server-issued id contains anything outside the
    /// YouTube id alphabet (it is headed for a URL and a notification).
    #[must_use]
    pub fn watch_url(&self) -> Option<String> {
        (!self.id.is_empty()
            && self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .then(|| format!("https://youtu.be/{}", self.id))
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Deserialize)]
struct InsertedVideo {
    id: String,
}

/// A cached access token with its (margined) expiry, on tokio's clock so tests can pause time.
struct CachedToken {
    token: String,
    expires_at: tokio::time::Instant,
}

/// Client for uploading finished clips to the user's YouTube channel.
#[derive(Clone)]
pub struct YouTubeClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_url: String,
    upload_url: String,
    // Shared across clones so a clone doesn't re-refresh a token the original just minted.
    access: Arc<tokio::sync::Mutex<Option<CachedToken>>>,
}

// Manual Debug: the refresh token mints access tokens and the client secret signs the exchange;
// neither may reach logs through an innocent `{:?}`.
impl std::fmt::Debug for YouTubeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YouTubeClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &"***")
            .field("refresh_token", &"***")
            .finish_non_exhaustive()
    }
}

impl YouTubeClient {
    /// Build a client that authenticates with `refresh_token`, minted for the given OAuth client.
    pub fn new(
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<Self, YouTubeError> {
        Ok(Self {
            http: http_client()?,
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            refresh_token: refresh_token.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            upload_url: UPLOAD_URL.to_owned(),
            access: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// Upload the MP4 at `path` with `title` and return the video id. Visibility maps 1:1 onto
    /// YouTube's `privacyStatus` (the config strings are Google's exact values).
    pub async fn upload(
        &self,
        path: &Path,
        title: &str,
        visibility: Visibility,
    ) -> Result<UploadedVideo, YouTubeError> {
        let size = tokio::fs::metadata(path).await?.len();
        if size > MAX_UPLOAD_BYTES {
            return Err(YouTubeError::TooLarge {
                size,
                max: MAX_UPLOAD_BYTES,
            });
        }
        let token = self.access_token().await?;

        // Resumable init: metadata first, the session URL comes back in Location.
        let init = api_request(
            self.http
                .post(format!(
                    "{}?uploadType=resumable&part=snippet,status",
                    self.upload_url
                ))
                .bearer_auth(&token)
                .header("X-Upload-Content-Length", size)
                .header("X-Upload-Content-Type", "video/mp4")
                .json(&serde_json::json!({
                    "snippet": { "title": title },
                    "status": {
                        "privacyStatus": visibility.as_str(),
                        "selfDeclaredMadeForKids": false,
                    },
                })),
        )
        .await?;
        let session_url = init
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or(YouTubeError::NoUploadUrl)?;

        // The bytes go to the session URL WITH the bearer (unlike a presigned PUT, the session
        // lives on googleapis.com and Google's protocol authenticates every request). The body
        // streams from disk; the deadline scales with the file size.
        let file = tokio::fs::File::open(path).await?;
        let put = self
            .http
            .put(&session_url)
            .bearer_auth(&token)
            .header(reqwest::header::CONTENT_TYPE, "video/mp4")
            .header(reqwest::header::CONTENT_LENGTH, size)
            .timeout(put_timeout(size))
            .body(reqwest::Body::wrap_stream(
                tokio_util::io::ReaderStream::new(file),
            ))
            .send()
            .await?;
        let status = put.status();
        if !status.is_success() {
            return Err(map_api_error(
                status.as_u16(),
                &crate::bounded_text(put).await,
            ));
        }
        let video: InsertedVideo = put.json().await?;
        Ok(UploadedVideo { id: video.id })
    }

    /// A valid access token: the cached one while it has margin left, else a fresh one from the
    /// refresh grant. `invalid_grant` (revoked/expired refresh token) maps to [`YouTubeError::NeedsReauth`].
    async fn access_token(&self) -> Result<String, YouTubeError> {
        let mut cached = self.access.lock().await;
        if let Some(c) = cached.as_ref()
            && tokio::time::Instant::now() < c.expires_at
        {
            return Ok(c.token.clone());
        }
        let resp = self
            .http
            .post(&self.token_url)
            .timeout(API_TIMEOUT)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", self.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(map_token_error(
                status.as_u16(),
                &crate::bounded_text(resp).await,
            ));
        }
        let token: TokenResponse = resp.json().await?;
        let ttl = Duration::from_secs(token.expires_in).saturating_sub(TOKEN_EXPIRY_MARGIN);
        *cached = Some(CachedToken {
            token: token.access_token.clone(),
            expires_at: tokio::time::Instant::now() + ttl,
        });
        Ok(token.access_token)
    }
}

/// A started YouTube loopback login: the caller opens [`auth_url`](Self::auth_url) in the
/// browser and awaits [`youtube_login_wait`], which serves the single redirect on the bound
/// loopback port and exchanges the code.
pub struct YouTubeLogin {
    /// The Google consent URL to open in the browser.
    pub auth_url: String,
    listener: tokio::net::TcpListener,
    redirect_uri: String,
    state: String,
    verifier: String,
    client_id: String,
    client_secret: String,
    token_url: String,
    timeout: Duration,
}

// Manual Debug: the PKCE verifier stands in for a client secret during the exchange.
impl std::fmt::Debug for YouTubeLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("YouTubeLogin")
            .field("redirect_uri", &self.redirect_uri)
            .field("client_id", &self.client_id)
            .field("verifier", &"***")
            .finish_non_exhaustive()
    }
}

/// Refresh token plus whether Google also sent an access token (unused; the client refreshes on
/// first use so there is exactly one token path).
#[derive(Deserialize)]
struct ExchangeResponse {
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Start a loopback OAuth login (RFC 8252 §7.3): bind an ephemeral 127.0.0.1 port and build the
/// consent URL with a fresh PKCE verifier (S256) and state. `access_type=offline` +
/// `prompt=consent` force a refresh token even on repeat logins.
pub async fn youtube_login_start(
    client_id: &str,
    client_secret: &str,
) -> Result<YouTubeLogin, YouTubeError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let verifier = random_urlsafe(64);
    let state = random_urlsafe(32);
    let challenge = {
        use base64::Engine;
        use sha2::Digest;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()))
    };
    let auth_url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}\
         &code_challenge={challenge}&code_challenge_method=S256&state={state}\
         &access_type=offline&prompt=consent",
        urlencode(client_id),
        urlencode(&redirect_uri),
        urlencode(SCOPE),
    );
    Ok(YouTubeLogin {
        auth_url,
        listener,
        redirect_uri,
        state,
        verifier,
        client_id: client_id.to_owned(),
        client_secret: client_secret.to_owned(),
        token_url: TOKEN_URL.to_owned(),
        timeout: LOGIN_TIMEOUT,
    })
}

/// Await the browser redirect, validate the state, answer with a small "you're logged in" page,
/// and exchange the code (with the PKCE verifier) for tokens. Returns the refresh token.
/// Abortable: dropping the future closes the listener. A local deadline maps to
/// [`YouTubeError::LoginExpired`].
pub async fn youtube_login_wait(login: YouTubeLogin) -> Result<String, YouTubeError> {
    let code = tokio::time::timeout(login.timeout, await_redirect(&login))
        .await
        .map_err(|_| YouTubeError::LoginExpired)??;

    let http = http_client()?;
    let resp = http
        .post(&login.token_url)
        .timeout(API_TIMEOUT)
        .form(&[
            ("client_id", login.client_id.as_str()),
            ("client_secret", login.client_secret.as_str()),
            ("code", code.as_str()),
            ("code_verifier", login.verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", login.redirect_uri.as_str()),
        ])
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(map_token_error(
            status.as_u16(),
            &crate::bounded_text(resp).await,
        ));
    }
    let tokens: ExchangeResponse = resp.json().await?;
    tokens.refresh_token.ok_or_else(|| {
        YouTubeError::LoginFailed(
            "Google did not issue a refresh token; try logging in again".to_owned(),
        )
    })
}

/// Serve loopback connections until one carries a redirect with our state; return its code.
/// Connections that aren't the redirect — favicon probes, other local traffic, and
/// query-bearing GETs whose state doesn't match (any process can poke a loopback port; a
/// stray hit must not kill the pending login) — get a 404 and the loop continues. Only a
/// state-matching redirect ends the flow.
async fn await_redirect(login: &YouTubeLogin) -> Result<String, YouTubeError> {
    loop {
        let (mut stream, _) = login.listener.accept().await?;
        match read_redirect(&mut stream).await {
            Ok(Some(params)) => match redirect_outcome(&params, &login.state) {
                Some(outcome) => {
                    let (status, page) = match &outcome {
                        Ok(_) => ("200 OK", pending_page()),
                        Err(e) => ("400 Bad Request", failure_page(&e.to_string())),
                    };
                    let _ = respond(&mut stream, status, &page).await;
                    return outcome;
                }
                None => {
                    let _ = respond(&mut stream, "404 Not Found", "Not found").await;
                }
            },
            Ok(None) => {
                let _ = respond(&mut stream, "404 Not Found", "Not found").await;
            }
            Err(e) => tracing::debug!(error = %e, "ignoring a bad loopback connection"),
        }
    }
}

/// Decide what a parsed redirect query means: `None` for a stray request (state missing or
/// mismatched — not ours to act on), `Some` for this login's terminal outcome. Split from the
/// IO for testability.
fn redirect_outcome(
    params: &[(String, String)],
    expected_state: &str,
) -> Option<Result<String, YouTubeError>> {
    let get = |k: &str| {
        params
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    // Constant-time comparison is unnecessary: the state is single-use and the listener only
    // accepts loopback connections, but it must match to bind this request (including an OAuth
    // error response, which Google sends with our state) to OUR login attempt.
    if get("state") != Some(expected_state) {
        return None;
    }
    if let Some(err) = get("error") {
        return Some(Err(YouTubeError::LoginFailed(match err {
            "access_denied" => "you declined the request in the browser".to_owned(),
            other => format!("Google reported {other:?}"),
        })));
    }
    Some(
        get("code")
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| YouTubeError::LoginFailed("the redirect carried no code".to_owned())),
    )
}

/// Read one HTTP request line from `stream`; `Some(query pairs)` for `GET /?...`, `None` for
/// anything else worth a 404. Bounded read: the request line is tiny.
async fn read_redirect(
    stream: &mut tokio::net::TcpStream,
) -> std::io::Result<Option<Vec<(String, String)>>> {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 8192];
    let mut len = 0;
    // Read until the request line is complete (CRLF) or the buffer fills.
    while len < buf.len() {
        let n = stream.read(&mut buf[len..]).await?;
        if n == 0 {
            break;
        }
        len += n;
        if buf[..len].windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf[..len]);
    let line = text.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let query = match target.split_once('?') {
        Some(("/", q)) => q,
        _ => return Ok(None),
    };
    Ok(Some(parse_query(query)))
}

/// Percent-decode a query string into key/value pairs.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((urldecode(k), urldecode(v)))
        })
        .collect()
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// The Arena-toned "you're logged in" page: self-contained, no external assets.
// The code exchange still runs after this page is served, so it must not claim success yet;
// the settings window shows the real outcome.
fn pending_page() -> String {
    login_page(
        "Approval received",
        "You can close this tab. rewynd is finishing the connection; \
         the settings window confirms when it is done.",
    )
}

fn failure_page(reason: &str) -> String {
    login_page("Login failed", &escape_html(reason))
}

fn login_page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>rewynd</title></head>\
         <body style=\"margin:0;display:grid;place-items:center;min-height:100vh;\
         background:#0b0b0f;color:#f0f0f4;font-family:system-ui,sans-serif\">\
         <div style=\"text-align:center;padding:32px;background:#111116;\
         border:1px solid rgba(255,255,255,.07);border-radius:8px\">\
         <div style=\"color:#00e5a0;font-size:22px;font-weight:800;\
         text-transform:uppercase\">{title}</div>\
         <p style=\"color:rgba(255,255,255,.5)\">{body}</p></div></body></html>"
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// An HTTP client with the shared connect timeout; redirects refused (a redirect on the token
/// endpoint could re-send credentials elsewhere).
fn http_client() -> Result<reqwest::Client, YouTubeError> {
    Ok(reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Send an API request; return the response on 2xx or the mapped Google error.
async fn api_request(req: reqwest::RequestBuilder) -> Result<reqwest::Response, YouTubeError> {
    let resp = req.timeout(API_TIMEOUT).send().await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    Err(map_api_error(
        status.as_u16(),
        &crate::bounded_text(resp).await,
    ))
}

/// Map Google's error JSON (`{"error": {"code", "message", "errors": [{"reason"}]}}`) onto the
/// user-actionable variants; anything unrecognized keeps the reason/message for diagnostics.
fn map_api_error(status: u16, body: &str) -> YouTubeError {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let error = json.get("error");
    let reason = error
        .and_then(|e| e.pointer("/errors/0/reason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    match reason {
        "quotaExceeded" | "dailyLimitExceeded" | "rateLimitExceeded" => YouTubeError::QuotaExceeded,
        "uploadLimitExceeded" => YouTubeError::UploadLimitExceeded,
        "authError" | "unauthorized" => YouTubeError::NeedsReauth,
        _ if status == 401 => YouTubeError::NeedsReauth,
        _ => YouTubeError::Api {
            status,
            reason: reason.to_owned(),
            message: error
                .and_then(|e| e.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| crate::snippet(body))
                .to_owned(),
        },
    }
}

/// Map an OAuth *token endpoint* failure. Unlike the Data API's nested error shape, these are
/// flat (`{"error": "invalid_grant", "error_description": "..."}`), and a 401 here means the
/// OAuth *client* was rejected, not that the user's login expired — so this must not reuse
/// [`map_api_error`], whose 401 arm would tell the user to log in again forever.
fn map_token_error(status: u16, body: &str) -> YouTubeError {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let code = json
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let description = json
        .get("error_description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match code {
        // The refresh token is revoked/expired: a fresh login fixes it.
        "invalid_grant" => YouTubeError::NeedsReauth,
        // The OAuth client itself was rejected: logging in again cannot help.
        "invalid_client" | "unauthorized_client" => YouTubeError::LoginFailed(format!(
            "Google rejected the OAuth client ({code}); check the client id and secret \
             under Advanced options"
        )),
        _ => YouTubeError::LoginFailed(format!(
            "Google reported {code} (HTTP {status}){}{}",
            if description.is_empty() { "" } else { ": " },
            description
        )),
    }
}

/// Total-request deadline for the video PUT, scaled to the file size.
fn put_timeout(size: u64) -> Duration {
    PUT_BASE_TIMEOUT + Duration::from_secs(size / PUT_MIN_RATE_BYTES_PER_SEC)
}

/// `len` random characters from the RFC 7636 unreserved alphabet (CSPRNG-backed).
fn random_urlsafe(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).expect("OS randomness available");
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

/// Percent-encode for a query component (RFC 3986 unreserved set kept).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-decode a query component (`+` is a space in query strings). A malformed escape is
/// kept verbatim rather than erroring: the value it garbles will simply fail the state check.
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len()
                && bytes[i + 1].is_ascii_hexdigit()
                && bytes[i + 2].is_ascii_hexdigit() =>
            {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn clip_file(contents: &[u8]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rewynd-yt-{}-{n}.mp4", std::process::id()));
        std::fs::write(&p, contents).expect("write clip fixture");
        p
    }

    /// A client whose token + upload endpoints point at the mock server.
    fn test_client(server: &MockServer) -> YouTubeClient {
        let mut c = YouTubeClient::new("cid", "csecret", "rt_secret").expect("client");
        c.token_url = format!("{}/token", server.uri());
        c.upload_url = format!("{}/upload/youtube/v3/videos", server.uri());
        c
    }

    fn token_ok(server_calls: u64) -> Mock {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rt_secret"))
            .and(body_string_contains("client_id=cid"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at_fresh",
                "expires_in": 3599,
                "token_type": "Bearer",
            })))
            .expect(server_calls)
    }

    #[test]
    fn watch_url_validates_the_id() {
        assert_eq!(
            UploadedVideo {
                id: "dQw4w9WgXcQ".into()
            }
            .watch_url(),
            Some("https://youtu.be/dQw4w9WgXcQ".to_owned())
        );
        assert_eq!(UploadedVideo { id: String::new() }.watch_url(), None);
        assert_eq!(
            UploadedVideo {
                id: "a/../b".into()
            }
            .watch_url(),
            None
        );
    }

    #[test]
    fn put_timeout_scales_with_size() {
        assert_eq!(put_timeout(0), PUT_BASE_TIMEOUT);
        assert!(put_timeout(1_000_000_000) > Duration::from_secs(3600));
    }

    #[test]
    fn url_encoding_round_trips() {
        let raw = "a b+c%d&e=f";
        assert_eq!(urldecode(&urlencode(raw)), raw);
        assert_eq!(urlencode("https://x/y"), "https%3A%2F%2Fx%2Fy");
        assert_eq!(urldecode("a%2Bb+c"), "a+b c");
        assert_eq!(urldecode("bad%zzescape"), "bad%zzescape");
    }

    #[test]
    fn random_urlsafe_is_unique_and_sized() {
        let a = random_urlsafe(64);
        let b = random_urlsafe(64);
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn redirect_outcome_checks_state_and_errors() {
        let pairs = |s: &[(&str, &str)]| -> Vec<(String, String)> {
            s.iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        assert_eq!(
            redirect_outcome(&pairs(&[("state", "s1"), ("code", "c1")]), "s1")
                .expect("ours")
                .expect("ok"),
            "c1"
        );
        // A stray query-bearing GET (wrong or missing state) must not end the login.
        assert!(redirect_outcome(&pairs(&[("state", "WRONG"), ("code", "c1")]), "s1").is_none());
        assert!(redirect_outcome(&pairs(&[("error", "access_denied")]), "s1").is_none());
        // Google's own error redirect carries our state and is terminal.
        assert!(matches!(
            redirect_outcome(&pairs(&[("state", "s1"), ("error", "access_denied")]), "s1"),
            Some(Err(YouTubeError::LoginFailed(_)))
        ));
        assert!(matches!(
            redirect_outcome(&pairs(&[("state", "s1")]), "s1"),
            Some(Err(YouTubeError::LoginFailed(_)))
        ));
    }

    #[test]
    fn debug_redacts_secrets() {
        let client = YouTubeClient::new("cid", "topsecret", "rt_secret").expect("client");
        let s = format!("{client:?}");
        assert!(!s.contains("topsecret") && !s.contains("rt_secret"), "{s}");
    }

    #[tokio::test]
    async fn login_start_builds_a_pkce_consent_url() {
        let login = youtube_login_start("my client", "sec")
            .await
            .expect("start");
        assert!(login.auth_url.starts_with(AUTH_URL));
        assert!(login.auth_url.contains("client_id=my%20client"));
        assert!(login.auth_url.contains("code_challenge_method=S256"));
        assert!(login.auth_url.contains("access_type=offline"));
        assert!(login.auth_url.contains("prompt=consent"));
        assert!(login.auth_url.contains(&format!("state={}", login.state)));
        // The verifier itself must never appear in the URL, only its S256 challenge.
        assert!(!login.auth_url.contains(&login.verifier));
        assert!(login.redirect_uri.starts_with("http://127.0.0.1:"));
    }

    #[tokio::test]
    async fn login_wait_serves_the_redirect_and_exchanges_the_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=auth_code_1"))
            .and(body_string_contains("code_verifier="))
            .and(body_string_contains("client_id=cid"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at",
                "expires_in": 3599,
                "refresh_token": "rt_minted",
                "token_type": "Bearer",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let mut login = youtube_login_start("cid", "sec").await.expect("start");
        login.token_url = format!("{}/token", server.uri());
        let redirect = format!(
            "{}/?state={}&code=auth_code_1",
            login.redirect_uri, login.state
        );
        let wait = tokio::spawn(youtube_login_wait(login));
        // The browser side: hit the loopback redirect like Google would.
        let page = reqwest::get(&redirect).await.expect("redirect served");
        assert_eq!(page.status(), 200);
        let html = page.text().await.expect("page body");
        assert!(html.contains("logged in"), "{html}");

        let token = wait.await.expect("join").expect("exchange");
        assert_eq!(token, "rt_minted");
    }

    #[tokio::test]
    async fn login_wait_rejects_a_denied_or_forged_redirect() {
        let login = youtube_login_start("cid", "sec").await.expect("start");
        let redirect = format!("{}/?error=access_denied", login.redirect_uri);
        let wait = tokio::spawn(youtube_login_wait(login));
        let page = reqwest::get(&redirect).await.expect("redirect served");
        assert!(page.text().await.expect("body").contains("Login failed"));
        assert!(matches!(
            wait.await.expect("join"),
            Err(YouTubeError::LoginFailed(_))
        ));

        // A state that isn't ours terminates the login without exchanging anything.
        let login = youtube_login_start("cid", "sec").await.expect("start");
        let redirect = format!("{}/?state=FORGED&code=x", login.redirect_uri);
        let wait = tokio::spawn(youtube_login_wait(login));
        let _ = reqwest::get(&redirect).await.expect("redirect served");
        assert!(matches!(
            wait.await.expect("join"),
            Err(YouTubeError::LoginFailed(_))
        ));
    }

    #[tokio::test]
    async fn login_wait_ignores_stray_requests_and_times_out_locally() {
        let mut login = youtube_login_start("cid", "sec").await.expect("start");
        login.timeout = Duration::from_millis(300);
        let favicon = format!("{}/favicon.ico", login.redirect_uri);
        let wait = tokio::spawn(youtube_login_wait(login));
        // A stray probe gets a 404 and must not consume the login.
        let resp = reqwest::get(&favicon).await.expect("stray served");
        assert_eq!(resp.status(), 404);
        assert!(matches!(
            wait.await.expect("join"),
            Err(YouTubeError::LoginExpired)
        ));
    }

    #[tokio::test]
    async fn missing_refresh_token_in_exchange_is_a_login_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at",
                "expires_in": 3599,
                "token_type": "Bearer",
            })))
            .mount(&server)
            .await;
        let mut login = youtube_login_start("cid", "sec").await.expect("start");
        login.token_url = format!("{}/token", server.uri());
        let redirect = format!("{}/?state={}&code=c", login.redirect_uri, login.state);
        let wait = tokio::spawn(youtube_login_wait(login));
        let _ = reqwest::get(&redirect).await.expect("redirect served");
        assert!(matches!(
            wait.await.expect("join"),
            Err(YouTubeError::LoginFailed(_))
        ));
    }

    #[tokio::test]
    async fn happy_path_refreshes_inits_and_puts_with_the_bearer() {
        let server = MockServer::start().await;
        token_ok(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/upload/youtube/v3/videos"))
            .and(query_param("uploadType", "resumable"))
            .and(query_param("part", "snippet,status"))
            .and(header("authorization", "Bearer at_fresh"))
            .and(header("x-upload-content-length", "4"))
            .and(header("x-upload-content-type", "video/mp4"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "snippet": { "title": "my clip" },
                "status": { "privacyStatus": "unlisted", "selfDeclaredMadeForKids": false },
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("location", format!("{}/session/abc", server.uri()).as_str()),
            )
            .expect(1)
            .mount(&server)
            .await;
        // Unlike a presigned storage PUT, the session PUT stays on googleapis and MUST carry
        // the bearer — that is the contract this test encodes.
        Mock::given(method("PUT"))
            .and(path("/session/abc"))
            .and(header("authorization", "Bearer at_fresh"))
            .and(header("content-type", "video/mp4"))
            .and(wiremock::matchers::body_bytes(b"mp4!".to_vec()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "vid123",
                "kind": "youtube#video",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client = test_client(&server);
        let video = client
            .upload(&file, "my clip", Visibility::Unlisted)
            .await
            .expect("upload succeeds");
        assert_eq!(video.id, "vid123");
        assert_eq!(
            video.watch_url(),
            Some("https://youtu.be/vid123".to_owned())
        );
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn access_token_is_cached_across_uploads() {
        let server = MockServer::start().await;
        token_ok(1).mount(&server).await; // expect(1): the second upload reuses the token
        Mock::given(method("POST"))
            .and(path("/upload/youtube/v3/videos"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("location", format!("{}/session/s", server.uri()).as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/session/s"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "v"})))
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client = test_client(&server);
        for _ in 0..2 {
            client
                .upload(&file, "t", Visibility::Public)
                .await
                .expect("upload succeeds");
        }
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn invalid_grant_on_refresh_needs_reauth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Token has been expired or revoked.",
            })))
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client = test_client(&server);
        assert!(matches!(
            client.upload(&file, "t", Visibility::Public).await,
            Err(YouTubeError::NeedsReauth)
        ));
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn quota_and_upload_limit_errors_map_to_their_variants() {
        let server = MockServer::start().await;
        token_ok(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/upload/youtube/v3/videos"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {
                    "code": 403,
                    "message": "The request cannot be completed...",
                    "errors": [{ "domain": "youtube.quota", "reason": "quotaExceeded" }],
                },
            })))
            .mount(&server)
            .await;
        let file = clip_file(b"mp4!");
        let client = test_client(&server);
        assert!(matches!(
            client.upload(&file, "t", Visibility::Public).await,
            Err(YouTubeError::QuotaExceeded)
        ));

        server.reset().await;
        token_ok(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/upload/youtube/v3/videos"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "code": 400,
                    "message": "The user has exceeded the number of videos they may upload.",
                    "errors": [{ "domain": "youtube.video", "reason": "uploadLimitExceeded" }],
                },
            })))
            .mount(&server)
            .await;
        let client = test_client(&server);
        assert!(matches!(
            client.upload(&file, "t", Visibility::Public).await,
            Err(YouTubeError::UploadLimitExceeded)
        ));
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn unknown_api_error_keeps_reason_and_message() {
        let server = MockServer::start().await;
        token_ok(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/upload/youtube/v3/videos"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "code": 400,
                    "message": "Bad title.",
                    "errors": [{ "reason": "invalidTitle" }],
                },
            })))
            .mount(&server)
            .await;
        let file = clip_file(b"mp4!");
        let client = test_client(&server);
        match client.upload(&file, "t", Visibility::Public).await {
            Err(YouTubeError::Api {
                status,
                reason,
                message,
            }) => {
                assert_eq!(status, 400);
                assert_eq!(reason, "invalidTitle");
                assert_eq!(message, "Bad title.");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn missing_location_header_is_its_own_error() {
        let server = MockServer::start().await;
        token_ok(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/upload/youtube/v3/videos"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let file = clip_file(b"mp4!");
        let client = test_client(&server);
        assert!(matches!(
            client.upload(&file, "t", Visibility::Public).await,
            Err(YouTubeError::NoUploadUrl)
        ));
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn missing_file_is_an_io_error() {
        let client = YouTubeClient::new("cid", "s", "rt").expect("client");
        assert!(matches!(
            client
                .upload(Path::new("/nonexistent/clip.mp4"), "t", Visibility::Public)
                .await,
            Err(YouTubeError::Io(_))
        ));
    }
}
