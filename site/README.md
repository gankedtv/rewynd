# rewynd.dev — landing page

The marketing site for **rewynd**, the instant-replay clip recorder. It ships on its
own domain but lives in this repo so the product and its site version together.

Built with **[Astro](https://astro.build)** — component-based, static output (one page,
no client framework). Requires Node ≥ 22.12.

## Develop

```sh
cd site
npm install
npm run dev       # dev server with HMR  → http://localhost:4321
npm run build     # static build          → dist/
npm run preview   # serve the built dist/
```

(`npm` shown; `pnpm`/`bun`/`yarn` work too — the scripts are runner-agnostic.)

## Structure

```text
site/
  astro.config.mjs        # set `site` to the real domain (drives canonical + OG URLs)
  public/assets/          # logo.svg + PNG icons (served at /assets/…)
  src/
    layouts/Base.astro    # <head>: meta, Open Graph, fonts, global.css, SvgDefs
    pages/index.astro     # composes the section components
    components/           # Nav, Hero, LibraryShot, WhatItDoes, HowItWorks, Platforms,
                          # Practical, BetaNote, Download, Footer, LogoMark, SvgDefs
    data/clips.ts         # the placeholder clip library shown below the hero
    data/release.ts       # pinned release tag + download-asset URLs
    styles/global.css     # design tokens + all component styles
```

The hero's OS-aware download label and the "copy" buttons are a small inline
`<script>` in `Hero.astro` — progressive enhancement; the page works without JS.

## Design

Ported from the design canvas ("Rewynd Site"): calm dark blue-slate surfaces, one
seafoam accent (`#6fcfae`), 1px borders instead of shadows, Source Serif 4 (display)
+ Instrument Sans (body) + IBM Plex Mono (data), 8-12px radii. Tokens live at the
top of `styles/global.css`. Dark-only for now.

Positioning: **standalone, open-source recorder first**; ganked.tv is a first-class
integration, not the headline. See the full spec:
`../docs/design/landing-page.md`.

## Deploy

`npm run build` emits a fully static `dist/` — host it anywhere (GitHub Pages,
Cloudflare Pages, Netlify, a bucket + CDN). No server runtime.

On every merge to `main` that touches `site/`, CI (`.github/workflows/site.yml`) builds
`Dockerfile` — a static build served by nginx — and pushes it to
`ghcr.io/gankedtv/rewynd-site` (`:latest` + the commit SHA). The package is private by
default; pull it wherever you deploy. Build/run it locally with
`docker build -t rewynd-site site && docker run -p 8080:80 rewynd-site`.

## Before it goes live — placeholders to replace

- **Domain:** `rewynd.dev` in `astro.config.mjs` drives canonical + OG URLs — repoint it
  if the production domain differs. The install command is centralized in `src/data/release.ts`
  (`INSTALL_CMD`) and already fetches the repo's `install.sh`, so it needs no domain.
- **Release tag:** download buttons pin `RELEASE_TAG` in `src/data/release.ts` (GitHub's
  `/releases/latest` 404s while every release is a prerelease). The release workflow
  (`.github/workflows/release.yml`, job `site-pin`) commits the new tag to `main` once the
  assets are uploaded, which rebuilds the site image; bump it by hand only if that fails.
- **Product shot:** the hero "Library" is a CSS mockup (`LibraryShot.astro`), not a
  screenshot. Drop in a real capture when ready — the layout won't move.
- **Clip titles + game grouping** (`data/clips.ts`) are aspirational — they assume
  per-clip naming and game auto-detection/tagging. Keep in sync with the app.
- **Social card:** `og:image` points at `assets/logo-512.png`; replace with a proper
  1200×630 card.

Numbers on the page are deliberately honest — no invented CPU %, download counts, or
star totals (Arena "data honesty"). Keep it that way.
