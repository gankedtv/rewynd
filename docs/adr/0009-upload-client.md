# ADR 0009 — ganked.tv upload client: reqwest/rustls, API-key auth, tray-triggered

- **Status:** Accepted (issue #18)
- **Supersedes / superseded by:** none
- **Relates to:** PLAN §8 (Phase 8), ADR 0005 (config), ADR 0007 (tray), gankedtv#155 (API keys)

## Context

Phase 8 connects the recorder to ganked.tv: a saved clip should be shareable without leaving the
desktop. The server (gankedtv#155) mints `gtv_`-prefixed API keys that authenticate the full upload
flow — `POST /clips` → `POST /clips/{id}/upload-url` → presigned `PUT` to storage → `POST
/clips/{id}/complete` → `GET /clips/{id}/status` (share code) — with RFC 7807 problem responses.
Limits: 500 MiB, `video/mp4` (we already encode H.264 MP4), 120 s (our buffer caps at 120 s).

## Decision

- **`rewynd-upload`** wraps the flow in a `GankedClient` (reqwest 0.13, **rustls** default TLS — no
  system TLS build dep). Errors surface the server's problem `code`/`detail` verbatim.
- **Auth = API key** as `Authorization: Bearer gtv_…`. The user creates the key at
  `ganked.tv/settings/api-keys` and pastes it into the settings window (masked input). The key
  lives in `config.toml` under `[upload]`; `save_to` tightens the file to `0600` on unix.
- **Trigger = explicit tray action** ("Upload last clip"), per the user's choice — saving stays
  local-only; nothing leaves the machine without a deliberate click. The recorder remembers the
  last saved clip path; the upload runs as its own tokio task (one at a time — a second click
  while one runs gets an "already running" toast, not a duplicate clip) and toasts the resulting
  `ganked.tv/c/<code>` share link, the server's rejection, or the failure. The upload settings are
  re-read from the config **per click**, so enabling uploads or fixing the key needs no recorder
  restart.
- **Visibility** (`public`/`unlisted`) is a config default with a settings dropdown; per-clip
  choice arrives with the trim/upload UI (issue #51).
- Defaults: API `https://api.ganked.tv`, share links `https://ganked.tv`; both overridable (dev
  runs against `http://localhost:5050`).

## Options evaluated

| Option | Verdict |
| --- | --- |
| **reqwest 0.13 (rustls)** | **chosen** — de-facto standard client, async fits our runtime, rustls avoids OpenSSL |
| ureq (blocking) | rejected — would need its own threads next to an existing tokio runtime |
| Device authorization grant (RFC 8628, also in gankedtv#155) | deferred — better UX (no copy-paste; rewynd shows a code, user approves in the browser) but more moving parts; the client shape stays the same, so it can replace key-pasting later |
| Secret-service/keyring for the key | deferred — plaintext-in-`0600`-config matches common CLI practice; a keyring adds another D-Bus dependency and failure mode |

## Consequences

- New deps: `reqwest` 0.13 (+rustls stack), `serde_json`, `jiff` (local-time clip titles),
  `wiremock` (dev-only, client tests). All permissive, GPL-compatible; no new CI system deps.
- The clip is read into memory for the PUT (tens of MB at the 30 s default; ~200 MB worst case at
  120 s), only after the presigned URL is in hand. Streaming from disk is a later refinement —
  presigned S3 PUTs generally want a Content-Length, so it needs care. The PUT deadline scales
  with file size (~1 Mbit/s floor) instead of a flat timeout, so slow-but-progressing uploads
  survive.
- One `GET /status` after complete fetches the share code; transcoding continues server-side and
  the link is valid while it runs. No polling loop in the recorder.
- The upload crate is CI-covered (wiremock); the tray/recorder wiring stays in the excluded
  `app/src/` and is validated live.
