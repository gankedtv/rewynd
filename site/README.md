# rewynd.gg — landing page

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
    components/           # BetaBar, Nav, Hero, LibraryShot, TrustStrip, HowItWorks,
                          # Features, Pipeline, GankedStrip, OpenSource, Download,
                          # Footer, LogoMark, SvgDefs
    data/clips.ts         # the placeholder clip library shown in the hero
    styles/global.css     # the Arena design system (tokens + all component styles)
```

The hero's OS-aware download label and the "copy" buttons are a small inline
`<script>` in `Hero.astro` — progressive enhancement; the page works without JS.

## Design

Follows the **Arena** design system (see `../docs/design/arena.md`): near-black
surfaces, one mint accent (`#00e5a0`), borders instead of shadows, Barlow Condensed
(display) + Inter (body), 8px radii. Dark-only for now (add light mode later with the
same tokens).

Positioning: **standalone, open-source recorder first**; ganked.tv is a first-class
integration, not the headline. See the full spec:
`../docs/superpowers/specs/2026-07-12-rewynd-landing-page-design.md`.

## Deploy

`npm run build` emits a fully static `dist/` — host it anywhere (GitHub Pages,
Cloudflare Pages, Netlify, a bucket + CDN). No server runtime.

## Before it goes live — placeholders to replace

- **Domain:** `rewynd.gg` in `astro.config.mjs`, the install command, links, and OG tags.
- **Product shot:** the hero "Library" is a CSS mockup (`LibraryShot.astro`), not a
  screenshot. Drop in a real capture when ready — the layout won't move.
- **Clip titles + game grouping** (`data/clips.ts`) are aspirational — they assume
  per-clip naming and game auto-detection/tagging. Keep in sync with the app.
- **Social card:** `og:image` points at `assets/logo-512.png`; replace with a proper
  1200×630 card.
- **Discord link** in the footer is a placeholder (`#`).

Numbers on the page are deliberately honest — no invented CPU %, download counts, or
star totals (Arena "data honesty"). Keep it that way.
