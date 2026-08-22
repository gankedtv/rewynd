//! ganked.tv upload client (docs/adr/0009): API-key auth over the 3-step presigned flow —
//! create the clip record, PUT the MP4 to presigned storage, complete, then read the share code.
//!
//! The client is transport-only: what to upload and when is the caller's business (the recorder
//! triggers it from the tray). Errors carry the server's RFC 7807 `code`/`detail` so the caller
//! can show something actionable.

pub mod youtube;

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// Server-side upload cap (500 MiB by default); pre-checked here so an oversized clip fails fast
/// instead of after the whole PUT.
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 524_288_000;
/// Clip-length cap until the server reports its own (per-deployment `MAX_CLIP_DURATION_SECS`,
/// fed back through [`GankedClient::with_max_clip_secs`]); pre-checked so a too-long clip fails
/// before the upload instead of server-side after the whole PUT.
pub const DEFAULT_MAX_CLIP_SECS: u64 = 120;
/// The range the server accepts for its own cap; outside it, config is stale or corrupt.
const SERVER_CLIP_SECS_RANGE: std::ops::RangeInclusive<u64> = 1..=3600;
/// Server-side title length cap (characters).
const MAX_TITLE_CHARS: usize = 255;
/// Description cap, matching what ganked.tv's own upload form allows (the API's hard ceiling is
/// higher, but the form is what this mirrors).
pub const MAX_DESCRIPTION_CHARS: usize = 500;
/// Tag slug bounds and the per-clip count, mirrored from the server's normalizer; a list longer
/// than this is rejected outright rather than truncated server-side.
const MIN_TAG_CHARS: usize = 2;
const MAX_TAG_CHARS: usize = 24;
pub const MAX_TAGS: usize = 5;
/// Suggestions pulled per keystroke for the game and tag pickers. Public because a caller
/// cannot otherwise tell a complete answer from a page that hit the limit.
pub const SUGGESTION_LIMIT: u32 = 8;
/// Server-side cap on a `?search=` term; a longer one is a 400, so trim locally.
const MAX_SEARCH_CHARS: usize = 100;

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

    /// Whether `s` names a recognized level, i.e. [`Self::parse`] won't fall back.
    #[must_use]
    pub fn is_recognized(s: &str) -> bool {
        let s = s.trim();
        Self::ALL.iter().any(|v| s.eq_ignore_ascii_case(v.as_str()))
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

/// What gets published alongside the clip file: the fields ganked.tv's own upload form offers.
/// Both destinations read it, each taking what it supports (YouTube has no game catalogue).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipMetadata {
    pub title: String,
    pub description: String,
    /// Free text as typed; [`normalize_tags`] slugs it on the way out.
    pub tags: Vec<String>,
    /// The ganked.tv catalogue id of the game, when one was picked.
    pub game_id: Option<i32>,
    pub visibility: Visibility,
}

impl ClipMetadata {
    /// Bare metadata: a title at the given visibility, nothing else set.
    #[must_use]
    pub fn new(title: impl Into<String>, visibility: Visibility) -> Self {
        Self {
            title: title.into(),
            visibility,
            ..Self::default()
        }
    }

    /// The title as it will be sent (clamped to the server's cap).
    #[must_use]
    pub fn sent_title(&self) -> &str {
        truncate_chars(&self.title, MAX_TITLE_CHARS)
    }

    /// The description as it will be sent: trimmed, clamped, possibly empty.
    #[must_use]
    pub fn sent_description(&self) -> &str {
        truncate_chars(self.description.trim(), MAX_DESCRIPTION_CHARS)
    }

    /// The `POST /clips` body. Optional fields are omitted rather than sent empty, so a clip
    /// without them looks the same as one created before they existed.
    fn create_body(&self) -> serde_json::Value {
        let mut body = serde_json::json!({
            "title": self.sent_title(),
            "visibility": self.visibility.as_str(),
        });
        let fields = body
            .as_object_mut()
            .expect("json! built an object literal here");
        let description = self.sent_description();
        if !description.is_empty() {
            fields.insert("description".to_owned(), description.into());
        }
        if let Some(id) = self.game_id {
            fields.insert("gameId".to_owned(), id.into());
        }
        let tags = normalize_tags(&self.tags);
        if !tags.is_empty() {
            fields.insert("tags".to_owned(), tags.into());
        }
        body
    }
}

/// Normalize one raw tag into the slug ganked.tv stores, mirroring the server: ASCII letters
/// lowercase, digits kept, whitespace/`-`/`_` collapsing to a single `-`, everything else
/// dropped. `None` when what survives falls outside the server's length range, i.e. the tag
/// would be rejected.
#[must_use]
pub fn normalize_tag(raw: &str) -> Option<String> {
    let mut slug = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if (ch == '-' || ch == '_' || ch.is_whitespace())
            && !slug.is_empty()
            && !slug.ends_with('-')
        {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    // Slugs are ASCII by construction, so byte length is the character count the server counts.
    (MIN_TAG_CHARS..=MAX_TAG_CHARS)
        .contains(&slug.len())
        .then(|| slug.to_owned())
}

/// The tag list actually sent: normalized, de-duplicated, and capped at [`MAX_TAGS`].
#[must_use]
pub fn normalize_tags<S: AsRef<str>>(raw: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in raw {
        let Some(slug) = normalize_tag(tag.as_ref()) else {
            continue;
        };
        if !out.contains(&slug) {
            out.push(slug);
            if out.len() == MAX_TAGS {
                break;
            }
        }
    }
    out
}

/// A detected game name as a catalogue search term. Window titles carry trademark decoration
/// ("Overwatch®") that a catalogue storing the plain name never matches, so it comes off.
#[must_use]
pub fn game_search_name(raw: &str) -> String {
    let stripped: String = raw
        .chars()
        .map(|c| {
            if matches!(c, '®' | '™' | '©') {
                ' '
            } else {
                c
            }
        })
        .collect();
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One game from ganked.tv's catalogue, for the upload form's picker.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Game {
    pub id: i32,
    pub name: String,
    pub slug: String,
}

/// One tag ganked.tv already knows, for the tag field's autocomplete.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TagSuggestion {
    pub slug: String,
    #[serde(default)]
    pub clip_count: u32,
}

/// Errors from talking to ganked.tv.
#[derive(Debug, Error)]
pub enum UploadError {
    #[error("could not read the clip: {0}")]
    Io(#[from] std::io::Error),
    #[error("clip is {} MB; ganked.tv accepts at most {} MB", size.div_ceil(1_000_000), max / 1_000_000)]
    TooLarge { size: u64, max: u64 },
    #[error("clip is {secs} seconds; ganked.tv accepts at most {max} seconds")]
    TooLong { secs: u64, max: u64 },
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("ganked.tv rejected the request (HTTP {status}, {code}): {detail}")]
    Api {
        status: u16,
        code: String,
        detail: String,
    },
    #[error("storage rejected the upload (HTTP {status}): {detail}")]
    Storage { status: u16, detail: String },
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
        share_url_from_code(self.share_code.as_deref(), share_base)
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
    failure_reason: Option<String>,
    duration_secs: Option<u32>,
    max_clip_duration_secs: Option<u32>,
}

/// A clip's server-side processing state, read after upload: richer than [`UploadedClip`] because
/// it carries the failure reason and duration facts the app surfaces when a clip fails to process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipStatusReport {
    pub status: String,
    pub share_code: Option<String>,
    pub failure_reason: Option<String>,
    pub duration_secs: Option<u32>,
    pub max_clip_duration_secs: Option<u32>,
}

impl ClipStatusReport {
    /// The clip finished processing and is playable.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == "ready"
    }

    /// The server gave up on the clip; `failure_message` explains why.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.status == "failed"
    }

    /// No further state change is coming — polling can stop.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.is_ready() || self.failed()
    }

    /// The public watch URL, if a share code was issued (validated like [`UploadedClip::share_url`]).
    #[must_use]
    pub fn share_url(&self, share_base: &str) -> Option<String> {
        share_url_from_code(self.share_code.as_deref(), share_base)
    }

    /// A human line explaining a failure, from the server's structured reason + duration facts.
    #[must_use]
    pub fn failure_message(&self) -> String {
        match self.failure_reason.as_deref() {
            Some("source_too_long") => match (self.duration_secs, self.max_clip_duration_secs) {
                (Some(d), Some(m)) => {
                    format!("Your clip is {d} seconds; ganked.tv's limit is {m} seconds.")
                }
                _ => "Your clip is too long for ganked.tv.".to_owned(),
            },
            Some("source_too_large") => "Your clip is too large for ganked.tv.".to_owned(),
            Some("source_unavailable" | "fetch_failed") => {
                "ganked.tv could not fetch the clip to process it.".to_owned()
            }
            _ => "ganked.tv could not process the clip.".to_owned(),
        }
    }
}

/// Validate a share code and build its watch URL under `share_base`. Shared by [`UploadedClip`]
/// and [`ClipStatusReport`]: a code is server-provided text headed for a URL, so anything outside
/// the code alphabet yields `None` rather than an injectable string.
fn share_url_from_code(share_code: Option<&str>, share_base: &str) -> Option<String> {
    share_code
        .filter(|c| {
            !c.is_empty()
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .map(|code| format!("{}/c/{code}", share_base.trim_end_matches('/')))
}

/// Client for uploading finished clips to ganked.tv.
#[derive(Clone)]
pub struct GankedClient {
    http: reqwest::Client,
    api_base: String,
    api_key: String,
    max_upload_bytes: u64,
    max_clip_secs: u64,
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
            max_clip_secs: DEFAULT_MAX_CLIP_SECS,
        })
    }

    /// Override the client-side size pre-check (the server stays authoritative).
    #[must_use]
    pub fn with_max_upload_bytes(mut self, max: u64) -> Self {
        self.max_upload_bytes = max;
        self
    }

    /// Pre-check lengths against the cap this deployment reported
    /// ([`ClipStatusReport::max_clip_duration_secs`]) instead of drifting from it. A cap the
    /// server itself would reject falls back to [`DEFAULT_MAX_CLIP_SECS`].
    #[must_use]
    pub fn with_max_clip_secs(mut self, max: u64) -> Self {
        self.max_clip_secs = if SERVER_CLIP_SECS_RANGE.contains(&max) {
            max
        } else {
            DEFAULT_MAX_CLIP_SECS
        };
        self
    }

    /// Upload the MP4 at `path` and return its id, share code, and initial processing status.
    pub async fn upload(
        &self,
        path: &Path,
        meta: &ClipMetadata,
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

        // Length check up front too: the server rejects an over-long clip only after the
        // whole PUT. Whole seconds, matching how the server truncates before comparing.
        let duration = {
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || clip_duration(&path))
                .await
                .ok()
                .flatten()
        };
        if let Some(duration) = duration
            && duration.as_secs() > self.max_clip_secs
        {
            return Err(UploadError::TooLong {
                secs: duration.as_secs(),
                max: self.max_clip_secs,
            });
        }

        let created: CreatedClip = self
            .api_json(self.http.post(self.url("/clips")).json(&meta.create_body()))
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
            let status = put.status().as_u16();
            let body = bounded_text(put).await;
            return Err(UploadError::Storage {
                status,
                detail: snippet(&body).to_owned(),
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

    /// Search the game catalogue for the upload form's picker. An empty query lists the
    /// catalogue's head, which is what the field shows before the user types.
    pub async fn search_games(&self, query: &str) -> Result<Vec<Game>, UploadError> {
        let mut req = self
            .http
            .get(self.url("/games"))
            .query(&[("limit", SUGGESTION_LIMIT)]);
        let query = truncate_chars(query.trim(), MAX_SEARCH_CHARS);
        if !query.is_empty() {
            req = req.query(&[("search", query)]);
        }
        self.api_json(req).await
    }

    /// Tags ganked.tv already knows that start with `prefix` (the server normalizes it, so the
    /// user's literal keystrokes can go straight out).
    pub async fn suggest_tags(&self, prefix: &str) -> Result<Vec<TagSuggestion>, UploadError> {
        let prefix = truncate_chars(prefix.trim(), MAX_TAG_CHARS);
        self.api_json(
            self.http
                .get(self.url("/tags"))
                .query(&[("prefix", prefix)])
                .query(&[("limit", SUGGESTION_LIMIT)]),
        )
        .await
    }

    /// Read a clip's current processing status (a richer view than [`upload`](Self::upload)
    /// returns, with the failure reason + duration facts).
    pub async fn clip_status(&self, clip_id: &str) -> Result<ClipStatusReport, UploadError> {
        let status: ClipStatus = self
            .api_json(self.http.get(self.url(&format!("/clips/{clip_id}/status"))))
            .await?;
        Ok(ClipStatusReport {
            status: status.status,
            share_code: status.share_code,
            failure_reason: status.failure_reason,
            duration_secs: status.duration_secs,
            max_clip_duration_secs: status.max_clip_duration_secs,
        })
    }

    /// Poll [`clip_status`](Self::clip_status) until it reaches a terminal state (ready/failed) or
    /// `max_reads` reads elapse, waiting `interval` between reads. A transient read error (a blip
    /// or a 5xx) does not end the poll — it is kept and retried, so a single hiccup can't collapse
    /// the whole multi-minute window. Returns the last status seen; only if no read ever succeeded
    /// does it surface the last error.
    pub async fn poll_status(
        &self,
        clip_id: &str,
        interval: Duration,
        max_reads: u32,
    ) -> Result<ClipStatusReport, UploadError> {
        let mut last: Option<ClipStatusReport> = None;
        let mut last_error: Option<UploadError> = None;
        for read in 0..max_reads.max(1) {
            if read > 0 {
                tokio::time::sleep(interval).await;
            }
            match self.clip_status(clip_id).await {
                Ok(report) if report.is_terminal() => return Ok(report),
                Ok(report) => {
                    last = Some(report);
                    last_error = None;
                }
                Err(e) => last_error = Some(e),
            }
        }
        // Budget spent without a terminal state: prefer the last status, else the last error.
        last.ok_or_else(|| {
            last_error.unwrap_or_else(|| UploadError::Api {
                status: 0,
                code: "no_status".to_owned(),
                detail: "the clip status could not be read".to_owned(),
            })
        })
    }

    /// Whether the clip still exists server-side. A 404 (the clip was deleted) returns `Ok(false)`
    /// so a re-upload can be allowed; other errors propagate (an offline check must not read as
    /// "gone" and green-light a duplicate).
    pub async fn clip_exists(&self, clip_id: &str) -> Result<bool, UploadError> {
        match self.clip_status(clip_id).await {
            Ok(_) => Ok(true),
            Err(UploadError::Api { status: 404, .. }) => Ok(false),
            Err(e) => Err(e),
        }
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
pub(crate) async fn bounded_text(resp: reqwest::Response) -> String {
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

/// An upload error in words a user can act on (shared by the tray toasts and the library
/// view, so both surfaces speak identically). Callers log the full error themselves.
#[must_use]
pub fn user_facing_upload_error(e: &UploadError) -> String {
    match e {
        UploadError::Http(_) => {
            "Could not reach ganked.tv; check your connection and the API server URL.".to_owned()
        }
        UploadError::Io(_) => "The clip file could not be read.".to_owned(),
        UploadError::Storage { status: 413, .. } => {
            "Your clip is too large for ganked.tv's storage; try trimming it shorter or \
             lowering the bitrate before uploading."
                .to_owned()
        }
        other => other.to_string(),
    }
}

/// The suggested title for a clip from `name` (the detected game, or the app itself):
/// "Name YYYY-MM-DD HH:MM" in local time.
#[must_use]
pub fn titled(name: &str) -> String {
    format!("{name} {}", jiff::Zoned::now().strftime("%Y-%m-%d %H:%M"))
}

/// The default title every upload surface stamps on a clip when no game is known.
#[must_use]
pub fn default_title() -> String {
    titled("rewynd")
}

/// The clip's container duration, when parseable. Lenient by design: an unreadable or exotic
/// file skips the client-side length pre-check and the server stays authoritative.
fn clip_duration(path: &Path) -> Option<Duration> {
    let file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).ok()?;
    Some(reader.duration())
}

/// A short slice of a non-JSON error body for diagnostics.
pub(crate) fn snippet(body: &str) -> &str {
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

    /// A real MP4 spanning `secs` seconds, written by the workspace muxer.
    fn mp4_spanning(secs: u64) -> PathBuf {
        use rewynd_buffer::EncodedChunk;
        let keyframe: std::sync::Arc<[u8]> = vec![
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f, 0x8c, 0x8d, 0x40, // SPS
            0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, // PPS
            0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33, 0xff, // IDR
        ]
        .into();
        let delta: std::sync::Arc<[u8]> =
            vec![0, 0, 0, 1, 0x41, 0x9a, 0x24, 0x6c, 0x41, 0x4f].into();
        let path =
            std::env::temp_dir().join(format!("rewynd-span-{}-{secs}.mp4", std::process::id()));
        // Three samples so the muxer's last-frame heuristic (reuse the previous gap)
        // adds one second, not a doubled runtime: total = secs + 1.
        rewynd_mux::Mp4Muxer::new(64, 64, 60)
            .write_mp4(
                &[
                    EncodedChunk {
                        bytes: keyframe,
                        is_keyframe: true,
                        pts: Duration::ZERO,
                    },
                    EncodedChunk {
                        bytes: delta.clone(),
                        is_keyframe: false,
                        pts: Duration::from_secs(secs - 1),
                    },
                    EncodedChunk {
                        bytes: delta,
                        is_keyframe: false,
                        pts: Duration::from_secs(secs),
                    },
                ],
                &path,
            )
            .expect("mux test clip");
        path
    }

    #[tokio::test]
    async fn a_too_long_clip_is_refused_before_any_request() {
        let path = mp4_spanning(DEFAULT_MAX_CLIP_SECS + 1);
        // Port 9 (discard) is never a ganked.tv server: reaching it at all would fail the
        // test with an Http error instead of the expected pre-check.
        let client = GankedClient::new("http://127.0.0.1:9", "gtv_k").expect("client");
        match client
            .upload(&path, &ClipMetadata::new("t", Visibility::Unlisted))
            .await
        {
            Err(UploadError::TooLong { secs, max }) => {
                assert_eq!(max, DEFAULT_MAX_CLIP_SECS);
                assert!(secs > DEFAULT_MAX_CLIP_SECS, "{secs}");
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_short_clip_passes_the_length_guard() {
        let path = mp4_spanning(5);
        let client = GankedClient::new("http://127.0.0.1:9", "gtv_k").expect("client");
        match client
            .upload(&path, &ClipMetadata::new("t", Visibility::Unlisted))
            .await
        {
            Err(UploadError::TooLong { .. }) => panic!("5s must pass the default guard"),
            Err(_) => {}
            Ok(_) => panic!("nothing is listening on the discard port"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn the_length_guard_follows_the_cap_the_server_reported() {
        let path = mp4_spanning(90);
        let client = GankedClient::new("http://127.0.0.1:9", "gtv_k").expect("client");

        // Under the default cap, a 90s clip is fine...
        if let Err(UploadError::TooLong { .. }) = client
            .clone()
            .upload(&path, &ClipMetadata::new("t", Visibility::Unlisted))
            .await
        {
            panic!("90s is under the {DEFAULT_MAX_CLIP_SECS}s default");
        }
        // ...but a deployment that reported a lower cap must be honored.
        match client
            .with_max_clip_secs(60)
            .upload(&path, &ClipMetadata::new("t", Visibility::Unlisted))
            .await
        {
            Err(UploadError::TooLong { secs, max }) => {
                assert_eq!(max, 60);
                assert!(secs >= 90, "{secs}");
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_cap_the_server_would_not_accept_falls_back_to_the_default() {
        let client = GankedClient::new("https://api.ganked.tv", "gtv_k").expect("client");
        for bogus in [0, 3601, u64::MAX] {
            assert_eq!(
                client.clone().with_max_clip_secs(bogus).max_clip_secs,
                DEFAULT_MAX_CLIP_SECS,
                "{bogus}"
            );
        }
        assert_eq!(client.with_max_clip_secs(3600).max_clip_secs, 3600);
    }

    #[test]
    fn clip_duration_is_lenient_on_non_mp4s() {
        let path = clip_file(b"not an mp4");
        assert_eq!(clip_duration(&path), None);
        let _ = std::fs::remove_file(&path);
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
    fn default_title_is_rewynd_plus_a_local_stamp() {
        let title = default_title();
        assert!(title.starts_with("rewynd "), "{title}");
        // "rewynd YYYY-MM-DD HH:MM"
        assert_eq!(title.len(), "rewynd ".len() + 16, "{title}");
    }

    #[test]
    fn user_facing_upload_errors_read_like_advice() {
        let io = UploadError::Io(std::io::Error::other("boom"));
        assert_eq!(
            user_facing_upload_error(&io),
            "The clip file could not be read."
        );
        let http = UploadError::Http(
            reqwest::Client::new()
                .get("not a url")
                .build()
                .expect_err("invalid URL"),
        );
        assert!(user_facing_upload_error(&http).contains("Could not reach ganked.tv"));
        let api = UploadError::Api {
            status: 401,
            code: "unauthorized".into(),
            detail: "bad key".into(),
        };
        assert_eq!(user_facing_upload_error(&api), api.to_string());
        let too_large = UploadError::Storage {
            status: 413,
            detail: "entity too large".into(),
        };
        assert_eq!(
            user_facing_upload_error(&too_large),
            "Your clip is too large for ganked.tv's storage; try trimming it shorter or \
             lowering the bitrate before uploading."
        );
        let other_storage = UploadError::Storage {
            status: 500,
            detail: "boom".into(),
        };
        assert_eq!(
            user_facing_upload_error(&other_storage),
            other_storage.to_string()
        );
    }

    #[test]
    fn put_timeout_scales_with_size() {
        assert_eq!(put_timeout(0), PUT_BASE_TIMEOUT);
        // 500 MiB at the 125 kB/s floor: over an hour of allowance, still bounded.
        let cap = put_timeout(DEFAULT_MAX_UPLOAD_BYTES);
        assert!(cap > Duration::from_secs(3600) && cap < Duration::from_secs(7200));
    }

    #[test]
    fn titles_and_descriptions_are_clamped_on_char_boundaries() {
        let short = ClipMetadata {
            description: "  hi  ".to_owned(),
            ..ClipMetadata::new("short", Visibility::Public)
        };
        assert_eq!(short.sent_title(), "short");
        assert_eq!(short.sent_description(), "hi");

        let long = ClipMetadata {
            description: "é".repeat(900),
            ..ClipMetadata::new("é".repeat(300), Visibility::Public)
        };
        assert_eq!(long.sent_title().chars().count(), MAX_TITLE_CHARS);
        assert_eq!(
            long.sent_description().chars().count(),
            MAX_DESCRIPTION_CHARS
        );
    }

    #[test]
    fn tags_normalize_the_way_the_server_does() {
        assert_eq!(normalize_tag("Clutch Play").as_deref(), Some("clutch-play"));
        assert_eq!(normalize_tag("  ACE  ").as_deref(), Some("ace"));
        assert_eq!(
            normalize_tag("one__two--three").as_deref(),
            Some("one-two-three")
        );
        assert_eq!(normalize_tag("héro!").as_deref(), Some("hro"));
        // Outside the server's 2..=24 range, so it would be rejected.
        assert_eq!(normalize_tag("a"), None);
        assert_eq!(normalize_tag("  "), None);
        assert_eq!(normalize_tag("!!"), None);
        assert_eq!(normalize_tag(&"x".repeat(25)), None);
        assert_eq!(normalize_tag(&"x".repeat(24)).map(|t| t.len()), Some(24));
    }

    #[test]
    fn a_detected_game_name_loses_its_trademark_decoration() {
        // The recorder's own folder name for this game; the catalogue stores "Overwatch".
        assert_eq!(game_search_name("Overwatch®"), "Overwatch");
        assert_eq!(game_search_name("Command & Conquer™ "), "Command & Conquer");
        assert_eq!(game_search_name("  Elden   Ring  "), "Elden Ring");
        assert_eq!(game_search_name("®"), "");
    }

    #[test]
    fn the_tag_list_is_deduped_and_capped() {
        let raw = [
            "Clutch Play",
            "clutch-play", // the same slug typed differently
            "a",           // too short to survive
            "one",
            "two",
            "three",
            "four",
            "five", // past the cap
        ];
        assert_eq!(
            normalize_tags(&raw),
            ["clutch-play", "one", "two", "three", "four"]
        );
        assert!(normalize_tags::<&str>(&[]).is_empty());
    }

    #[test]
    fn the_create_body_omits_the_fields_that_were_left_empty() {
        let bare = ClipMetadata::new("t", Visibility::Public);
        assert_eq!(
            bare.create_body(),
            serde_json::json!({ "title": "t", "visibility": "public" })
        );
        let full = ClipMetadata {
            description: "context".to_owned(),
            tags: vec!["Clutch Play".to_owned(), "!".to_owned()],
            game_id: Some(42),
            ..ClipMetadata::new("t", Visibility::Unlisted)
        };
        assert_eq!(
            full.create_body(),
            serde_json::json!({
                "title": "t",
                "visibility": "unlisted",
                "description": "context",
                "gameId": 42,
                "tags": ["clutch-play"],
            })
        );
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
        // covers a body-less 204). The request body must stay empty: trimming happens locally,
        // and the API-key path rejects a trim body with 400 trim_not_supported.
        Mock::given(method("POST"))
            .and(path("/clips/clip-1/complete"))
            .and(auth())
            .and(wiremock::matchers::body_bytes(Vec::new()))
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
            .upload(&file, &ClipMetadata::new("my clip", Visibility::Unlisted))
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
    async fn the_full_form_reaches_the_clip_record() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/clips"))
            .and(body_json(serde_json::json!({
                "title": "my clip",
                "visibility": "public",
                "description": "the last round",
                "gameId": 7,
                "tags": ["clutch-play", "ace"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "c"})))
            .expect(1)
            .mount(&server)
            .await;
        // The flow stops at the upload-url step; the create body is what this asserts.
        let file = clip_file(b"mp4!");
        let client = GankedClient::new(&server.uri(), "gtv_k").expect("client");
        let meta = ClipMetadata {
            description: "  the last round  ".to_owned(),
            tags: vec!["Clutch Play".to_owned(), "ACE".to_owned()],
            game_id: Some(7),
            ..ClipMetadata::new("my clip", Visibility::Public)
        };
        let _ = client.upload(&file, &meta).await;
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn game_search_and_tag_suggestions_carry_the_query_and_the_key() {
        let server = MockServer::start().await;
        let auth = || header("authorization", "Bearer gtv_testkey");
        use wiremock::matchers::query_param;

        Mock::given(method("GET"))
            .and(path("/games"))
            .and(auth())
            .and(query_param("search", "elden"))
            .and(query_param("limit", SUGGESTION_LIMIT.to_string()))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "id": 12,
                    "name": "Elden Ring",
                    "slug": "elden-ring",
                    "tag": "eldenring",
                    "coverUrl": null,
                }])),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/tags"))
            .and(auth())
            .and(query_param("prefix", "clu"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "id": 3,
                    "slug": "clutch-play",
                    "name": "clutch-play",
                    "clipCount": 9,
                }])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = GankedClient::new(&server.uri(), "gtv_testkey").expect("client");
        let games = client.search_games("  elden  ").await.expect("games");
        assert_eq!(
            games,
            [Game {
                id: 12,
                name: "Elden Ring".to_owned(),
                slug: "elden-ring".to_owned(),
            }]
        );
        let tags = client.suggest_tags("clu").await.expect("tags");
        assert_eq!(
            tags,
            [TagSuggestion {
                slug: "clutch-play".to_owned(),
                clip_count: 9,
            }]
        );
    }

    #[tokio::test]
    async fn an_empty_game_query_lists_the_catalogue_head() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/games"))
            // No `search` at all, rather than an empty one the server would LIKE-match.
            .and(|req: &wiremock::Request| !req.url.query_pairs().any(|(k, _)| k == "search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;
        let client = GankedClient::new(&server.uri(), "gtv_k").expect("client");
        assert!(client.search_games("   ").await.expect("games").is_empty());
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
            .upload(&file, &ClipMetadata::new("t", Visibility::Public))
            .await
            .expect("flow completes");
        assert!(clip.failed(), "failed status must surface to the caller");
        assert_eq!(clip.share_url("https://ganked.tv"), None);
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn clip_status_reads_rich_failure_fields() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/clips/c/status"))
            .and(header("authorization", "Bearer gtv_testkey"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "failed",
                "failureReason": "source_too_long",
                "durationSecs": 75,
                "maxClipDurationSecs": 60,
            })))
            .mount(&server)
            .await;
        let client = GankedClient::new(&server.uri(), "gtv_testkey").expect("client");
        let report = client.clip_status("c").await.expect("status");
        assert!(report.failed());
        let message = report.failure_message();
        assert!(
            message.contains("75 seconds") && message.contains("60 seconds"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn poll_status_returns_on_the_first_terminal_read() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/clips/c/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ready",
                "shareCode": "ab12",
            })))
            .expect(1) // terminal on the first read → no further polls
            .mount(&server)
            .await;
        let client = GankedClient::new(&server.uri(), "gtv_testkey").expect("client");
        let report = client
            .poll_status("c", std::time::Duration::ZERO, 5)
            .await
            .expect("poll");
        assert!(report.is_ready());
        assert_eq!(
            report.share_url("https://ganked.tv"),
            Some("https://ganked.tv/c/ab12".to_owned())
        );
    }

    #[tokio::test]
    async fn poll_status_gives_up_after_the_read_budget() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/clips/c/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "processing",
            })))
            .expect(3) // exactly max_reads, then give up while still non-terminal
            .mount(&server)
            .await;
        let client = GankedClient::new(&server.uri(), "gtv_testkey").expect("client");
        let report = client
            .poll_status("c", std::time::Duration::ZERO, 3)
            .await
            .expect("poll");
        assert!(
            !report.is_terminal(),
            "still processing after the read budget"
        );
    }

    #[tokio::test]
    async fn poll_status_keeps_reading_through_transient_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/clips/c/status"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3) // a 5xx must not collapse the whole window after one read
            .mount(&server)
            .await;
        let client = GankedClient::new(&server.uri(), "gtv_testkey").expect("client");
        let result = client.poll_status("c", std::time::Duration::ZERO, 3).await;
        assert!(result.is_err(), "no status ever read surfaces the error");
    }

    #[tokio::test]
    async fn clip_exists_is_false_only_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/clips/gone/status"))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(serde_json::json!({"code": "not_found"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/clips/live/status"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": "ready"})),
            )
            .mount(&server)
            .await;
        let client = GankedClient::new(&server.uri(), "gtv_testkey").expect("client");
        assert!(!client.clip_exists("gone").await.expect("gone check"));
        assert!(client.clip_exists("live").await.expect("live check"));
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
            .upload(&file, &ClipMetadata::new("t", Visibility::Public))
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
            .upload(&file, &ClipMetadata::new("t", Visibility::Public))
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
            .upload(&file, &ClipMetadata::new("t", Visibility::Public))
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
            .upload(&file, &ClipMetadata::new("t", Visibility::Public))
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
            .respond_with(ResponseTemplate::new(403).set_body_string("access denied"))
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client = GankedClient::new(&server.uri(), "gtv_k").expect("client");
        match client
            .upload(&file, &ClipMetadata::new("t", Visibility::Public))
            .await
            .expect_err("storage 403 fails")
        {
            UploadError::Storage { status, detail } => {
                assert_eq!(status, 403);
                assert!(detail.contains("access denied"));
            }
            other => panic!("expected Storage error, got {other:?}"),
        }
        let _ = std::fs::remove_file(&file);
    }

    #[tokio::test]
    async fn storage_413_gets_actionable_user_facing_copy() {
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
            .respond_with(ResponseTemplate::new(413).set_body_string("entity too large"))
            .mount(&server)
            .await;

        let file = clip_file(b"mp4!");
        let client = GankedClient::new(&server.uri(), "gtv_k").expect("client");
        let err = client
            .upload(&file, &ClipMetadata::new("t", Visibility::Public))
            .await
            .expect_err("storage 413 fails");
        match &err {
            UploadError::Storage { status, detail } => {
                assert_eq!(*status, 413);
                assert!(detail.contains("entity too large"));
            }
            other => panic!("expected Storage error, got {other:?}"),
        }
        assert_eq!(
            user_facing_upload_error(&err),
            "Your clip is too large for ganked.tv's storage; try trimming it shorter or \
             lowering the bitrate before uploading."
        );
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
            .upload(&file, &ClipMetadata::new("t", Visibility::Public))
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
            .upload(
                Path::new("/nonexistent/clip.mp4"),
                &ClipMetadata::new("t", Visibility::Public),
            )
            .await
            .expect_err("missing file")
        {
            UploadError::Io(_) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }
}
