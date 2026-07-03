# Baking in the YouTube OAuth client

rewynd uploads to YouTube with a **compiled-in Google OAuth client** so users can connect their
account without creating their own Google Cloud project. The client id/secret are read at build
time via `option_env!` in [`crates/upload/src/youtube.rs`](../crates/upload/src/youtube.rs):
`REWYND_YT_CLIENT_ID` and `REWYND_YT_CLIENT_SECRET`. Unset → the built-in client is empty and the
YouTube login is disabled until a user pastes their own under the app's Advanced options.

This is the standard "installed app" model: the secret ends up in the distributed binary and is not
truly confidential. Security comes from **PKCE** (rewynd uses S256) and, above all, **each user's own
consent** — nobody's clips upload without them signing in with their Google account and choosing to
upload. Never try to hide or bypass that consent.

## Google Cloud setup (once)

Use a **separate project** from any server project (e.g. keep it apart from a gankedtv Cloud
project). It can sit under the same Organization/folder as a parent — that's just billing/IAM
hierarchy and doesn't share credentials, consent screen, or quota (those are per-project).

1. **New project** (optionally under your org as parent resource).
2. **APIs & Services → Library → YouTube Data API v3 → Enable.** (The scope won't appear, and uploads
   will 403, until this API is on.)
3. **Google Auth Platform → Branding**: app name `rewynd`, logo, homepage. This is what users see on
   the consent screen.
4. **Audience**: User type **External**. Add yourself (and any early testers) as **test users**.
5. **Data Access → Add or remove scopes**: add exactly
   `https://www.googleapis.com/auth/youtube.upload` (the only scope rewynd needs — a *sensitive*
   scope). Save.
6. **Clients → Create OAuth client ID → Desktop app** → copy the **Client ID** and **Client secret**.

### Testing vs. production

- **Testing** (default): only your listed test users (max 100) can authorize; refresh tokens expire
  after **7 days**, so testers re-log-in weekly.
- **In production**: anyone can use it, but the sensitive `youtube.upload` scope requires **Google
  verification** (privacy policy URL, scope justification, sometimes a demo video). Unverified in
  production shows an "unverified app" warning and caps new users until verified.

## Build vars

`option_env!` is read at compile time. The `build.rs` in `rewynd-upload` marks these vars with
`rerun-if-env-changed`, so a rebuild picks up new values without a `cargo clean`.

Local build (fish shell — `env` because inline `VAR=val cmd` isn't fish syntax):

```fish
env REWYND_YT_CLIENT_ID=xxxx.apps.googleusercontent.com \
    REWYND_YT_CLIENT_SECRET=GOCSPX-yyyy \
    cargo build --release -p rewynd
```

### In CI (the release/packaging build)

The client only needs to be baked into the **distributed** binary, so it belongs in the release
workflow that ships artifacts — **not** in `ci.yml`, whose test builds are never distributed (putting
the secret there just leaks it into logs/cache for no benefit).

When that release workflow exists (see the installers/packaging work), add the two as **GitHub repo
secrets** (Settings → Secrets and variables → Actions: `REWYND_YT_CLIENT_ID`,
`REWYND_YT_CLIENT_SECRET`) and inject them as `env:` on the release build step:

```yaml
- name: Build rewynd (release)
  env:
    REWYND_YT_CLIENT_ID: ${{ secrets.REWYND_YT_CLIENT_ID }}
    REWYND_YT_CLIENT_SECRET: ${{ secrets.REWYND_YT_CLIENT_SECRET }}
  run: cargo build --release -p rewynd
```

Do not commit the id/secret to source. (They are extractable from the shipped binary regardless — the
point is to keep them out of the repo history and to be able to rotate them.)
