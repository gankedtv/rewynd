//! ganked.tv upload client (docs/adr/0009): API-key auth over the 3-step presigned flow —
//! create the clip record, PUT the MP4 to presigned storage, complete, then read the share code.
//!
//! The client is transport-only: what to upload and when is the caller's business (the recorder
//! triggers it from the tray). Errors carry the server's RFC 7807 `code`/`detail` so the caller
//! can show something actionable.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// Server-side upload cap (500 MiB by default); pre-checked here so an oversized clip fails fast
/// instead of after the whole PUT.
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 524_288_000;
/// Server-side title length cap (characters).
const MAX_TITLE_CHARS: usize = 255;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// Base allowance for the storage PUT, extended per byte below.
const PUT_BASE_TIMEOUT: Duration = Duration::from_secs(60);
/// Slowest tolerated sustained upload rate (~1 Mbit/s): the PUT deadline scales with the file
/// size, so a slow-but-progressing upload isn't cut off by a flat total-request timeout.
const PUT_MIN_RATE_BYTES_PER_SEC: u64 = 125_000;

/// Clip visibility for uploads: `Public` is in feeds, `Unlisted` is reachable by link only,
/// `Private` is owner-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    Public,
    #[default]
    Unlisted,
    Private,
}

impl Visibility {
    pub const ALL: [Visibility; 3] = [
        Visibility::Public,
        Visibility::Unlisted,
        Visibility::Private,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Unlisted => "unlisted",
            Self::Private => "private",
        }
    }

    /// Parse a config value. Fails closed: only an explicit, recognized level is honored;
    /// anything unrecognized (a typo, say) becomes private rather than widening visibility.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if s.eq_ignore_ascii_case("public") {
            Self::Public
        } else if s.eq_ignore_ascii_case("unlisted") {
            Self::Unlisted
        } else {
            Self::Private
        }
    }
}

// Rendered directly in the settings pick_list.
impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Public => "Public",
            Self::Unlisted => "Unlisted",
            Self::Private => "Private",
        })
    }
}

/// Errors from talking to ganked.tv.
#[derive(Debug, Error)]
pub enum UploadError {
    #[error("could not read the clip: {0}")]
    Io(#[from] std::io::Error),
    #[error("clip is {} MB; ganked.tv accepts at most {} MB", size.div_ceil(1_000_000), max / 1_000_000)]
    TooLarge { size: u64, max: u64 },
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ganked.tv rejected the request (HTTP {status}, {code}): {detail}")]
    Api {
        status: u16,
        code: String,
        detail: String,
    },
    #[error("storage rejected the upload (HTTP {status})")]
    Storage { status: u16 },
    #[error("the login request expired; start again")]
    LoginExpired,
    #[error("invalid API URL {url:?}: {reason}")]
    InvalidUrl { url: String, reason: String },
}

/// An uploaded clip. `status` is the server's processing state right after completion — usually
/// `processing`/`transcoding`; `failed` means the server rejected the clip after upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedClip {
    pub id: String,
    pub share_code: Option<String>,
    pub status: String,
}

impl UploadedClip {
    /// Public watch URL under `share_base` (e.g. `https://ganked.tv`), if a share code was
    /// issued. The code is server-provided text headed for a URL and a notification, so anything
    /// outside the code alphabet yields `None` rather than an injectable string.
    #[must_use]
    pub fn share_url(&self, share_base: &str) -> Option<String> {
        self.share_code
            .as_ref()
            .filter(|c| {
                !c.is_empty()
                    && c.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            .map(|code| format!("{}/c/{code}", share_base.trim_end_matches('/')))
    }

    /// Whether the server already marked the clip failed (nothing shareable will come of it).
    #[must_use]
    pub fn failed(&self) -> bool {
        self.status == "failed"
    }
}

#[derive(Deserialize)]
struct CreatedClip {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadTarget {
    url: String,
    content_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClipStatus {
    share_code: Option<String>,
    #[serde(default)]
    status: String,
}

/// Client for uploading finished clips to ganked.tv.
#[derive(Clone)]
pub struct GankedClient {
    http: reqwest::Client,
    api_base: String,
    api_key: String,
    max_upload_bytes: u64,
}

// Manual Debug: the API key must never reach logs through an innocent `{:?}`.
impl std::fmt::Debug for GankedClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GankedClient")
            .field("api_base", &self.api_base)
            .field("api_key", &"gtv_***")
            .field("max_upload_bytes", &self.max_upload_bytes)
            .finish_non_exhaustive()
    }
}

impl GankedClient {
    /// Build a client for `api_base` (e.g. `https://api.ganked.tv`) authenticating with `api_key`.
    /// A malformed base URL fails here, not with an opaque error on first use.
    pub fn new(api_base: &str, api_key: &str) -> Result<Self, UploadError> {
        Ok(Self {
            http: http_client()?,
            api_base: checked_base(api_base)?,
            api_key: api_key.to_owned(),
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
        })
    }

    /// Override the client-side size pre-check (the server stays authoritative).
    #[must_use]
    pub fn with_max_upload_bytes(mut self, max: u64) -> Self {
        self.max_upload_bytes = max;
        self
    }

    /// Upload the MP4 at `path` and return its id, share code, and initial processing status.
    pub async fn upload(
        &self,
        path: &Path,
        title: &str,
        visibility: Visibility,
    ) -> Result<UploadedClip, UploadError> {
        // Size check up front (before a clip record exists); the file itself is read only once
        // the presigned URL is in hand, to keep the in-memory window as short as possible.
        let size = tokio::fs::metadata(path).await?.len();
        if size > self.max_upload_bytes {
            return Err(UploadError::TooLarge {
                size,
                max: self.max_upload_bytes,
            });
        }

        let created: CreatedClip = self
            .api_json(self.http.post(self.url("/clips")).json(&serde_json::json!({
                "title": clamp_title(title),
                "visibility": visibility.as_str(),
            })))
            .await?;

        let target: UploadTarget = self
            .api_json(
                self.http
                    .post(self.url(&format!("/clips/{}/upload-url", created.id))),
            )
            .await?;

        // Straight to storage, deliberately WITHOUT the bearer key (it must not leak to the
        // storage host); the presigned signature covers the exact content type, so echo it. The
        // body streams from disk (an explicit Content-Length keeps S3-style endpoints happy)
        // instead of buffering a whole clip next to the live ring buffers.
        let file = tokio::fs::File::open(path).await?;
        let put = self
            .http
            .put(&target.url)
            .header(reqwest::header::CONTENT_TYPE, &target.content_type)
            .header(reqwest::header::CONTENT_LENGTH, size)
            .timeout(put_timeout(size))
            .body(reqwest::Body::wrap_stream(
                tokio_util::io::ReaderStream::new(file),
            ))
            .send()
            .await?;
        if !put.status().is_success() {
            return Err(UploadError::Storage {
                status: put.status().as_u16(),
            });
        }

        // The response body (id + size echo) carries nothing we need.
        self.api_send(
            self.http
                .post(self.url(&format!("/clips/{}/complete", created.id))),
        )
        .await?;

        // Transcoding continues server-side; one status read is enough for the share code.
        let status: ClipStatus = self
            .api_json(
                self.http
                    .get(self.url(&format!("/clips/{}/status", created.id))),
            )
            .await?;

        Ok(UploadedClip {
            id: created.id,
            share_code: status.share_code,
            status: status.status,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api_base)
    }

    /// [`api_request`] with this client's bearer key.
    async fn api_send(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, UploadError> {
        api_request(req.bearer_auth(&self.api_key)).await
    }

    /// [`api_send`], then parse the 2xx body as JSON.
    async fn api_json<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, UploadError> {
        Ok(self.api_send(req).await?.json().await?)
    }
}

/// An HTTP client with the shared connect timeout. Redirects are refused outright: every URL in
/// the flow is either configured or presigned, so a redirect can only mislead (and reqwest would
/// re-send the bearer header to wherever it points).
fn http_client() -> Result<reqwest::Client, UploadError> {
    Ok(reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Whether `url` may be reached over plain http: only loopback (the dev server). Anything else
/// would put the bearer key and device codes on the wire in cleartext.
fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // IPv6 hosts come bracketed in URLs.
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Validate an API base URL and normalize it (no trailing slash): https anywhere, http only on
/// loopback.
fn checked_base(api_base: &str) -> Result<String, UploadError> {
    let base = api_base.trim().trim_end_matches('/');
    let invalid = |reason: String| UploadError::InvalidUrl {
        url: base.to_owned(),
        reason,
    };
    match reqwest::Url::parse(base) {
        Ok(url) if url.query().is_some() || url.fragment().is_some() => Err(invalid(
            "the base URL must not carry a query or fragment".to_owned(),
        )),
        Ok(url) if url.scheme() == "http" && !is_loopback(&url) => {
            Err(invalid("http is only allowed for localhost".to_owned()))
        }
        Ok(url) if !matches!(url.scheme(), "http" | "https") => {
            Err(invalid(format!("unsupported scheme {:?}", url.scheme())))
        }
        Ok(url) if url.has_host() => Ok(base.to_owned()),
        Ok(_) => Err(invalid("no host".to_owned())),
        Err(e) => Err(invalid(e.to_string())),
    }
}

/// Cap on how much of an error body is read for diagnostics; the expected problem JSON is tiny,
/// and an unbounded read would let a hostile server exhaust the recorder's memory.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Send an API request; return the response on 2xx, or the parsed RFC 7807 problem as
/// [`UploadError::Api`]. The `code` may be flat or nested under `extensions`.
async fn api_request(req: reqwest::RequestBuilder) -> Result<reqwest::Response, UploadError> {
    let resp = req.timeout(API_TIMEOUT).send().await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = bounded_text(resp).await;
    let problem: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let string_of =
        |v: Option<&serde_json::Value>| v.and_then(serde_json::Value::as_str).map(str::to_owned);
    Err(UploadError::Api {
        status: status.as_u16(),
        // ASP.NET usually flattens problem extensions to the top level; accept nested too.
        code: string_of(
            problem
                .get("code")
                .or_else(|| problem.pointer("/extensions/code")),
        )
        .unwrap_or_else(|| "unknown".to_owned()),
        detail: string_of(problem.get("detail")).unwrap_or_else(|| snippet(&body).to_owned()),
    })
}

/// Read at most [`MAX_ERROR_BODY_BYTES`] of a response body as (lossy) text.
async fn bounded_text(resp: reqwest::Response) -> String {
    use futures_util::StreamExt;
    let mut collected: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        let room = MAX_ERROR_BODY_BYTES - collected.len();
        collected.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if collected.len() >= MAX_ERROR_BODY_BYTES {
            break;
        }
    }
    String::from_utf8_lossy(&collected).into_owned()
}

/// A started ganked.tv device login (RFC 8628): the user approves `user_code` in the browser
/// while the app polls with the (private) device code. Carries the API base it was started
/// against, so polling can't drift to a different server than the one that issued the code.
#[derive(Clone)]
pub struct DeviceLogin {
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    api_base: String,
    device_code: String,
    interval_secs: u64,
    expires_in_secs: u64,
    /// RFC 8628's backoff step on `slow_down` (5 s); a field so tests can shrink it.
    slow_down_step: Duration,
}

// Manual Debug: the device code mints an API key on approval; keep it out of logs.
impl std::fmt::Debug for DeviceLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceLogin")
            .field("user_code", &self.user_code)
            .field("api_base", &self.api_base)
            .field("device_code", &"dvc_***")
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceStartResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    token: String,
}

/// Start a device login against `api_base`; the caller shows/opens
/// [`DeviceLogin::verification_uri_complete`] and then awaits [`device_login_wait`].
pub async fn device_login_start(
    api_base: &str,
    client_name: &str,
) -> Result<DeviceLogin, UploadError> {
    let http = http_client()?;
    let base = checked_base(api_base)?;
    let resp: DeviceStartResponse = api_request(
        http.post(format!("{base}/auth/device"))
            .json(&serde_json::json!({ "clientName": client_name })),
    )
    .await?
    .json()
    .await?;
    Ok(DeviceLogin {
        user_code: resp.user_code,
        verification_uri: resp.verification_uri,
        verification_uri_complete: resp.verification_uri_complete,
        api_base: base,
        device_code: resp.device_code,
        // Server-controlled values, clamped so a hostile response can't wedge the login task
        // (u64::MAX would overflow the deadline arithmetic) or spin the poll loop.
        interval_secs: resp.interval.clamp(1, 60),
        expires_in_secs: resp.expires_in.min(1800),
        slow_down_step: Duration::from_secs(5),
    })
}

/// Poll until the user approves (returning the minted `gtv_` API key) or the flow terminates:
/// denial/expiry surface as [`UploadError::Api`] with the server's code, a locally-passed
/// deadline as [`UploadError::LoginExpired`]. Honors the server's interval and `slow_down`.
pub async fn device_login_wait(login: &DeviceLogin) -> Result<String, UploadError> {
    let http = http_client()?;
    let base = &login.api_base;
    // tokio's clock (not std) so the deadline follows the runtime's virtual time in tests.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(login.expires_in_secs);
    let mut interval = Duration::from_secs(login.interval_secs);
    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() > deadline {
            return Err(UploadError::LoginExpired);
        }
        let result = api_request(
            http.post(format!("{base}/auth/device/token"))
                .json(&serde_json::json!({ "deviceCode": login.device_code })),
        )
        .await;
        match result {
            Ok(resp) => return Ok(resp.json::<DeviceTokenResponse>().await?.token),
            Err(UploadError::Api { ref code, .. }) if code == "authorization_pending" => {}
            Err(UploadError::Api { ref code, .. }) if code == "slow_down" => {
                interval += login.slow_down_step;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Total-request deadline for the storage PUT, scaled to the file size (see the rate constant).
fn put_timeout(size: u64) -> Duration {
    PUT_BASE_TIMEOUT + Duration::from_secs(size / PUT_MIN_RATE_BYTES_PER_SEC)
}

/// Truncate to the server's title cap on a character boundary.
fn clamp_title(title: &str) -> &str {
    truncate_chars(title, MAX_TITLE_CHARS)
}

/// A short slice of a non-JSON error body for diagnostics.
fn snippet(body: &str) -> &str {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "no detail";
    }
    truncate_chars(trimmed, 200)
}

/// The longest prefix of `s` holding at most `max` characters (char-boundary safe).
fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A unique temp file with `contents`, removed by the caller.
    fn clip_file(contents: &[u8]) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rewynd-up-{}-{n}.mp4", std::process::id()));
        std::fs::write(&p, contents).expect("write clip fixture");
        p
    }

    #[test]
    fn visibility_round_trip_and_fails_closed() {
        assert_eq!(Visibility::parse("unlisted"), Visibility::Unlisted);
        assert_eq!(Visibility::parse("PUBLIC "), Visibility::Public);
        assert_eq!(Visibility::parse("public"), Visibility::Public);
        assert_eq!(Visibility::parse("Private"), Visibility::Private);
        // A typo must never widen visibility.
        assert_eq!(Visibility::parse("publik"), Visibility::Private);
        assert_eq!(Visibility::parse(""), Visibility::Private);
        assert_eq!(Visibility::Unlisted.as_str(), "unlisted");
        assert_eq!(Visibility::Private.as_str(), "private");
    }

    #[test]
    fn share_url_joins_and_handles_missing_code() {
        let with = UploadedClip {
            id: "x".into(),
            share_code: Some("ab12".into()),
            status: "processing".into(),
        };
        assert_eq!(
            with.share_url("https://ganked.tv/"),
            Some("https://ganked.tv/c/ab12".to_owned())
        );
        assert!(!with.failed());
        let without = UploadedClip {
            id: "x".into(),
            share_code: None,
            status: "failed".into(),
        };
        assert_eq!(without.share_url("https://ganked.tv"), None);
        assert!(without.failed());
    }

    #[test]
    fn put_timeout_scales_with_size() {
        assert_eq!(put_timeout(0), PUT_BASE_TIMEOUT);
        // 500 MiB at the 125 kB/s floor: over an hour of allowance, still bounded.
        let cap = put_timeout(DEFAULT_MAX_UPLOAD_BYTES);
        assert!(cap > Duration::from_secs(3600) && cap < Duration::from_secs(7200));
    }

    #[test]
    fn titles_are_clamped_on_char_boundaries() {
        assert_eq!(clamp_title("short"), "short");
        let long: String = "é".repeat(300);
        assert_eq!(clamp_title(&long).chars().count(), 255);
    }

    #[test]
    fn snippet_trims_and_bounds() {
        assert_eq!(snippet("  "), "no detail");
        assert_eq!(snippet(" boom "), "boom");
        let long = "x".repeat(500);
        assert_eq!(snippet(&long).len(), 200);
    }

    #[tokio::test]
    async fn happy_path_uploads_and_returns_share_code() {
        let server = MockServer::start().await;
        let auth = || header("authorization", "Bearer gtv_testkey");

        Mock::given(method("POST"))
            .and(path("/clips"))
            .and(auth())
            .and(body_json(serde_json::json!({
                "title": "my clip",
                "visibility": "unlisted",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "clip-1",
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/clips/clip-1/upload-url"))
            .and(auth())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": format!("{}/storage/obj", server.uri()),
                "expiresAt": "2099-01-01T00:00:00Z",
                "contentType": "video/mp4",
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/storage/obj"))
            .and(header("content-type", "video/mp4"))
            // The streamed body must arrive byte-for-byte, and the API key must never reach
            // the storage host.
            .and(wiremock::matchers::body_bytes(b"mp4!".to_vec()))
            .and(|req: &wiremock::Request| !req.headers.contains_key("authorization"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        // The real server echoes id + size; the client reads neither (the failed-status test
        // covers a body-less 204).
        Mock::given(method("POST"))
            .and(path("/clips/clip-1/complete"))
            .and(auth())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "clip-1",
                "fileSizeBytes": 4,
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/clips/clip-1/status"))
            .and(auth())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "clip-1",
                "status": "processing",
                "shareCode": "zz99",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client =
            GankedClient::new(&format!("{}/", server.uri()), "gtv_testkey").expect("client");
        let clip = client
            .upload(&file, "my clip", Visibility::Unlisted)
            .await
            .expect("upload succeeds");
        assert_eq!(clip.id, "clip-1");
        assert_eq!(clip.status, "processing");
        assert!(!clip.failed());
        assert_eq!(
            clip.share_url("https://ganked.tv"),
            Some("https://ganked.tv/c/zz99".to_owned())
        );
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn immediately_failed_clip_reports_failed_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/clips"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "c"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/clips/c/upload-url"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": format!("{}/storage/obj", server.uri()),
                "expiresAt": "2099-01-01T00:00:00Z",
                "contentType": "video/mp4",
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/storage/obj"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/clips/c/complete"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/clips/c/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "c",
                "status": "failed",
                "shareCode": null,
                "failureReason": "transcode_failed",
            })))
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client = GankedClient::new(&server.uri(), "gtv_k").expect("client");
        let clip = client
            .upload(&file, "t", Visibility::Public)
            .await
            .expect("flow completes");
        assert!(clip.failed(), "failed status must surface to the caller");
        assert_eq!(clip.share_url("https://ganked.tv"), None);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn api_problem_is_surfaced_with_code_flat_or_nested() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/clips"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "title": "Unauthorized",
                "status": 401,
                "detail": "Invalid, revoked, or expired API key.",
                "code": "unauthorized",
            })))
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client = GankedClient::new(&server.uri(), "gtv_bad").expect("client");
        let err = client
            .upload(&file, "t", Visibility::Public)
            .await
            .expect_err("401 fails");
        match err {
            UploadError::Api {
                status,
                code,
                detail,
            } => {
                assert_eq!(status, 401);
                assert_eq!(code, "unauthorized");
                assert!(detail.contains("API key"));
            }
            other => panic!("expected Api error, got {other:?}"),
        }

        // Nested `extensions.code` shape.
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/clips"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "detail": "bad title",
                "extensions": { "code": "invalid_title" },
            })))
            .mount(&server)
            .await;
        let err = client
            .upload(&file, "t", Visibility::Public)
            .await
            .expect_err("400 fails");
        match err {
            UploadError::Api { code, .. } => assert_eq!(code, "invalid_title"),
            other => panic!("expected Api error, got {other:?}"),
        }

        // A null `extensions` must not wipe the flat fields.
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/clips"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "detail": "bad title",
                "code": "invalid_title",
                "extensions": null,
            })))
            .mount(&server)
            .await;
        let err = client
            .upload(&file, "t", Visibility::Public)
            .await
            .expect_err("400 fails");
        match err {
            UploadError::Api { code, detail, .. } => {
                assert_eq!(code, "invalid_title");
                assert_eq!(detail, "bad title");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn non_json_error_body_becomes_snippet_detail() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/clips"))
            .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client = GankedClient::new(&server.uri(), "gtv_k").expect("client");
        match client
            .upload(&file, "t", Visibility::Public)
            .await
            .expect_err("502 fails")
        {
            UploadError::Api {
                status,
                code,
                detail,
            } => {
                assert_eq!(status, 502);
                assert_eq!(code, "unknown");
                assert!(detail.contains("bad gateway"));
            }
            other => panic!("expected Api error, got {other:?}"),
        }
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn storage_rejection_is_its_own_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/clips"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "c"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/clips/c/upload-url"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": format!("{}/storage/obj", server.uri()),
                "expiresAt": "2099-01-01T00:00:00Z",
                "contentType": "video/mp4",
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/storage/obj"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client = GankedClient::new(&server.uri(), "gtv_k").expect("client");
        match client
            .upload(&file, "t", Visibility::Public)
            .await
            .expect_err("storage 403 fails")
        {
            UploadError::Storage { status } => assert_eq!(status, 403),
            other => panic!("expected Storage error, got {other:?}"),
        }
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn oversized_clip_fails_before_any_request() {
        let server = MockServer::start().await;
        // No mocks mounted: any request would 404 and the expect(0) guard below would fail.
        let file = clip_file(b"four");
        let client = GankedClient::new(&server.uri(), "gtv_k")
            .expect("client")
            .with_max_upload_bytes(3);
        match client
            .upload(&file, "t", Visibility::Public)
            .await
            .expect_err("too large")
        {
            UploadError::TooLarge { size, max } => {
                assert_eq!((size, max), (4, 3));
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "the size pre-check must fire before any network call"
        );
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn checked_base_accepts_urls_and_rejects_garbage() {
        assert_eq!(
            checked_base("https://api.ganked.tv/").expect("valid"),
            "https://api.ganked.tv"
        );
        assert_eq!(
            checked_base(" http://localhost:5050 ").expect("valid"),
            "http://localhost:5050"
        );
        assert!(matches!(
            checked_base("api.ganked.tv"),
            Err(UploadError::InvalidUrl { .. })
        ));
        assert!(matches!(
            checked_base("file:///etc/passwd"),
            Err(UploadError::InvalidUrl { .. })
        ));
        assert!(matches!(
            GankedClient::new("not a url", "gtv_k"),
            Err(UploadError::InvalidUrl { .. })
        ));
    }

    #[tokio::test]
    async fn device_login_start_parses_the_grant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/device"))
            .and(body_json(serde_json::json!({ "clientName": "rewynd" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "deviceCode": "dvc_secret",
                "userCode": "ABCD-1234",
                "verificationUri": "https://ganked.tv/device",
                "verificationUriComplete": "https://ganked.tv/device?code=ABCD-1234",
                "expiresIn": 600,
                "interval": 5,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let login = device_login_start(&server.uri(), "rewynd")
            .await
            .expect("starts");
        assert_eq!(login.user_code, "ABCD-1234");
        assert_eq!(
            login.verification_uri_complete,
            "https://ganked.tv/device?code=ABCD-1234"
        );
        assert_eq!(login.device_code, "dvc_secret");
    }

    /// Zero-interval login fixture so the polling tests run without real sleeps.
    fn instant_login(api_base: &str, device_code: &str, expires_in_secs: u64) -> DeviceLogin {
        DeviceLogin {
            user_code: "ABCD-1234".into(),
            verification_uri: String::new(),
            verification_uri_complete: String::new(),
            api_base: api_base.trim_end_matches('/').to_owned(),
            device_code: device_code.into(),
            interval_secs: 0,
            expires_in_secs,
            slow_down_step: Duration::from_millis(50),
        }
    }

    #[tokio::test]
    async fn device_login_wait_polls_until_approved() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/device/token"))
            .and(body_json(serde_json::json!({ "deviceCode": "dvc_s" })))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "detail": "not yet",
                "code": "authorization_pending",
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/device/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "gtv_minted",
                "tokenType": "Bearer",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let token = device_login_wait(&instant_login(&server.uri(), "dvc_s", 600))
            .await
            .expect("approved");
        assert_eq!(token, "gtv_minted");
    }

    // The backoff step is shrunk by the fixture so this asserts the mechanism without real sleeps.
    #[tokio::test]
    async fn device_login_wait_honors_slow_down() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/device/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "detail": "too fast",
                "code": "slow_down",
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/device/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "gtv_late",
                "tokenType": "Bearer",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let started = std::time::Instant::now();
        let token = device_login_wait(&instant_login(&server.uri(), "dvc_s", 600))
            .await
            .expect("approved after backoff");
        assert_eq!(token, "gtv_late");
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "slow_down must actually back off"
        );
    }

    #[tokio::test]
    async fn device_login_wait_surfaces_denial_and_expiry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/device/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "detail": "The user denied the request.",
                "code": "access_denied",
            })))
            .mount(&server)
            .await;
        match device_login_wait(&instant_login(&server.uri(), "dvc_x", 600))
            .await
            .expect_err("denied")
        {
            UploadError::Api { code, .. } => assert_eq!(code, "access_denied"),
            other => panic!("expected Api error, got {other:?}"),
        }

        // A zero-lifetime login trips the local deadline before any poll succeeds.
        match device_login_wait(&instant_login(&server.uri(), "dvc_x", 0))
            .await
            .expect_err("expires")
        {
            UploadError::LoginExpired => {}
            other => panic!("expected LoginExpired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_file_is_an_io_error() {
        let client = GankedClient::new("http://127.0.0.1:1", "gtv_k").expect("client");
        match client
            .upload(Path::new("/nonexistent/clip.mp4"), "t", Visibility::Public)
            .await
            .expect_err("missing file")
        {
            UploadError::Io(_) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }
}
