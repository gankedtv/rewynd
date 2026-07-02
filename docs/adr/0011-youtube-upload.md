# ADR 0011 — YouTube upload: loopback OAuth (PKCE), resumable videos.insert, embedded desktop client

- **Status:** Accepted (issue #57)
- **Supersedes / superseded by:** none
- **Relates to:** ADR 0009 (upload client, ganked.tv), ADR 0005 (config)

## Context

A saved clip should be publishable straight to the user's YouTube channel, as a second
destination beside ganked.tv. YouTube Data API v3 `videos.insert` requires OAuth 2.0 (no API-key
auth for uploads) and offers a resumable upload protocol suited to clips of tens of MB. The app
is a desktop binary with no server component, so the OAuth client and token handling live
entirely on the user's machine.

## Decision

- **Auth = the installed-app loopback flow (RFC 8252 §7.3) with PKCE (S256).** "Log in with
  YouTube" binds an ephemeral `127.0.0.1` port, opens Google's consent URL in the browser
  (`access_type=offline&prompt=consent`, so a refresh token is always issued), serves the single
  redirect with a small self-contained confirmation page, and exchanges the code. Only the
  **refresh token** is persisted; access tokens are minted on use (with an expiry margin) and
  never stored or logged.
  - The RFC 8628 device flow — which ganked.tv uses (ADR 0009) — was evaluated and **rejected as
    impossible**: Google's device flow supports only a fixed scope list, and
    `https://www.googleapis.com/auth/youtube.upload` is not on it (only `youtube` and
    `youtube.readonly`). Verified against Google's limited-input-device documentation at
    implementation time. Google's OOB/manual-copy flow is discontinued, leaving loopback as the
    only supported desktop flow for this scope.
- **Scope = `youtube.upload` only** — the minimal scope for `videos.insert`. It grants no read
  access, so the UI shows a plain "Connected" badge (no channel name without an extra scope).
- **Upload = the resumable protocol**: `POST
  https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status`
  with `{snippet:{title}, status:{privacyStatus, selfDeclaredMadeForKids:false}}` → session URL
  from `Location` → one `PUT` of the bytes, streamed from disk with a size-scaled deadline (the
  same ~1 Mbit/s floor as the ganked.tv PUT). Unlike a presigned storage PUT, the session URL
  stays on googleapis.com and **does** carry the bearer (encoded in the tests). Interruption
  resume (308 + `Content-Range`) is deferred: clips are small and the recorder retriggers a
  whole upload cheaply; the session init already speaks the resumable protocol if that changes.
- **Visibility** maps 1:1 onto `privacyStatus` (`Visibility::as_str` values are Google's own —
  public, unlisted and private alike); `[youtube] visibility` falls back to the shared
  `[upload]` default and fails closed to **private** on anything unrecognized, so a typo can
  only ever narrow who sees a clip.
- **OAuth client = embedded desktop-app client** (`option_env!("REWYND_YT_CLIENT_ID")` /
  `REWYND_YT_CLIENT_SECRET` at build time, currently empty), overridable at runtime via
  `[youtube] client_id/client_secret` and the settings' Advanced fields. Google's installed-app
  model explicitly treats the desktop client secret as **non-confidential** — embedding is the
  sanctioned pattern; PKCE binds the code to our process.
- **Errors are user-actionable**: quota exhaustion (403 `quotaExceeded`), channel upload limits
  (`uploadLimitExceeded`), and an invalid/expired refresh token (`invalid_grant` →
  `NeedsReauth`, surfaced as "log in with YouTube again") each get their own variant; secrets
  are Debug-redacted like the ganked.tv client.
- **Trigger = explicit tray action** ("Upload last clip to YouTube"), sharing the ganked.tv
  busy flag (one upload at a time across destinations), re-reading the config per click, and
  toasting the `https://youtu.be/<id>` link on success.

## Consequences

- **Shared-quota caveat:** `videos.insert` is expensive against the default project quota
  (historically ~1,600 of 10,000 daily units — about **6 uploads/day across all users of the
  embedded client** — with Google migrating projects to per-bucket upload quotas of similar
  stinginess). A per-user Google Cloud client (the Advanced override) sidesteps sharing;
  raising the embedded client's quota needs a Google audit. The quota error is surfaced
  verbatim as "try again after midnight Pacific time".
- **Unverified-app caveat:** while the Google Cloud consent screen is in *Testing* mode,
  refresh tokens expire after **7 days** and logins are limited to enrolled test users. The
  `NeedsReauth` path (re-login in the settings) covers this; publishing/verifying the app
  removes the expiry.
- The user must create the Google Cloud project + OAuth "Desktop app" client before the
  built-in login works; until then the settings offer the client-id/secret override.
- New deps: `sha2`, `base64`, `getrandom` (PKCE challenge + CSPRNG state/verifier) and
  reqwest's `form` feature (token endpoints are `application/x-www-form-urlencoded`). All
  permissive; `base64`/`getrandom` were already in the tree transitively.
- The OAuth/upload client is CI-covered with wiremock (token exchange against a real loopback
  redirect, refresh, resumable init + PUT, error mapping); the tray/settings wiring stays in
  the excluded `app/src/` + `settings/src/` and is validated live.
