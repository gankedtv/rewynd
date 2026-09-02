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
  astro.config.mjs        # set `site` to the real domain (drives canonical, OG URLs, sitemap)
  nginx.conf              # runtime config: 404 page, gzip, immutable caching for /_astro/
  public/assets/          # logo.svg + PNG icons + og-card.png (served at /assets/…)
  src/
    layouts/Base.astro    # <head>: meta, Open Graph, JSON-LD, fonts, global.css, SvgDefs
    pages/index.astro     # composes the section components
    pages/404.astro       # served by nginx's error_page for unknown paths
    pages/robots.txt.ts   # points crawlers at the sitemap; Cloudflare appends its own block
    components/           # Nav, Hero, LibraryShot, WhatItDoes, HowItWorks, Platforms,
                          # Practical, BetaNote, Download, Footer, LogoMark, SvgDefs
    data/clips.ts         # the placeholder clip library shown below the hero
    data/release.ts       # pinned release tag + download-asset URLs
    styles/global.css     # design tokens + all component styles
```

The hero's OS-aware download label and the "copy" buttons are a small inline
`<script>` in `Hero.astro` — progressive enhancement; the page works without JS.

`Nav` and `Footer` are shared with the 404 page, so their in-page links are written
root-relative (`/#how`, not `#how`) — on the homepage that is still a same-document
hash jump, and from `/404` it actually goes somewhere.

## SEO

`@astrojs/sitemap` emits `/sitemap-index.xml` at build time (the 404 route is excluded).
`Base.astro` carries the canonical link, the Open Graph / Twitter card meta, and a
`SoftwareApplication` JSON-LD block. That block deliberately has no `aggregateRating`
or download count — we have no honest numbers, and inventing them risks a manual action.

`404.astro` passes `noindex` to the layout, which drops the canonical, the `og:url` and
the JSON-LD: the page is only ever reached through nginx's `error_page`, so the URL a
canonical would name returns 404 itself, and the site entity belongs on the homepage.

The lockfile is generated on Linux, so it keeps the platform-specific optional deps
(`@emnapi/*`) that `npm ci` needs inside the `node:22-alpine` build stage. Regenerating
it on macOS prunes them and breaks the Docker build; do it in the image instead:

```sh
docker run --rm -v "$PWD":/w -w /w node:22-alpine npm install --package-lock-only
```

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

Numbers on the page are deliberately honest — no invented CPU %, download counts, or
star totals (Arena "data honesty"). Keep it that way.
