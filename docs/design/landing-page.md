# rewynd landing page — design spec

**Date:** 2026-07-12
**Status:** Agreed. Implemented as an Astro static site in [`site/`](../../site).
**Design system:** Arena (see [`docs/design/arena.md`](arena.md)) — shared with ganked.tv.

---

## 1. What this is

A marketing landing page for **rewynd** — the lightweight, native instant-replay clip
recorder (Linux · Windows · macOS beta). It ships on its own domain but is part of the
ganked.tv project, and lives in this repo (`site/`) so it versions with the product.

**Goal:** get the right people to download the beta, while establishing rewynd's
open-source, local-first credibility.

## 2. Positioning (decided)

**Standalone + open-source, in that order.** rewynd leads as a fast native recorder
that makes standalone MP4s; ganked.tv (and YouTube) are optional, first-class upload
integrations shown mid-page — not the headline. This matches `PLAN.md` ("general-purpose
first; ganked.tv integration is a later, optional feature, not the core identity").

The page is a blend of two directions explored in brainstorming:

- **A — product-first** (backbone): download-led, shows the real app.
- **B — developer/open-source** (woven in): one-line install command, zero-copy story,
  prominent GitHub, GPL, "built in the open."

## 3. Design language

Straight from Arena — do not drift:

- **Surfaces:** `#0b0b0f` base → `#111116` raised → `#18181f` high. Depth via surface +
  border (`rgba(255,255,255,.07)`), **never shadows**.
- **One accent:** mint `#00e5a0` for every interactive/emphasis state. No second accent.
- **Type:** Barlow Condensed (700–900, uppercase) for wordmark/hero/section titles;
  Inter (400–700) for everything else.
- **Radii:** 8px cards/buttons, 6–7px inputs, pills fully round.
- **No UI gradients** except sanctioned brand art: the logo mark's internal gradients,
  the hero ambient glow behind the product shot, and the game-cover legibility tints.
- **Filter pills** are the only pill shapes (used for the in-shot game filters).
- Dark-only for v1. Light mode can follow later using the same tokens.

**Logo:** the mint HUD-frame play mark (`docs/design/logo.svg`), wordmark `REWYND` in
Barlow Condensed 900, with a `Beta` badge.

## 4. Page structure (one route, top → bottom)

1. **Beta bar** — "rewynd is in public beta — early, evolving, built in the open → GitHub."
2. **Nav** (sticky) — logo + `Beta`, section links, ★ GitHub, `Get ganked.tv` (secondary),
   `Download` (primary mint).
3. **Hero** (stacked, centered) — kicker, H1 "Instant replay for your gameplay.", subhead,
   one-line `curl … | sh` install (copy button), Download + Star CTAs, beta/WIP note,
   platform chips, then a large **product shot** of the Library (see §5) with a soft mint
   glow and a "press F10 → clip saved" callout.
4. **Trust strip** — 4 honest stat tiles: 60s buffered · zero-copy GPU · 1080p60 H.264 ·
   GPL-3.0/local.
5. **How it works** — 3 steps: set up once (wizard) → just play → hit the hotkey.
6. **Features** — 6 cards: tray recorder · zero-copy pipeline · clean playable clips ·
   library & trim · fully tunable (1080p60) · optional uploads.
7. **Why it stays light** — the zero-copy pipeline diagram (capture → ring buffer →
   hotkey → MP4) + the Rust/wgpu/Vulkan Video/ScreenCaptureKit note.
8. **ganked.tv strip** — "One click to ganked.tv"; upload destinations (ganked.tv,
   YouTube, local).
9. **Open-source band** — GPL, local-first, built-in-the-open, SignPath signing incoming;
   a repo/terminal card.
10. **Download band** — Linux (AppImage curl), Windows (installer), macOS (build from
    source, beta) + unsigned/SmartScreen note.
11. **Footer** — product / open-source / ganked.tv link columns + license.

## 5. The product shot

A faithful mock of the app's **Library** view: left sidebar (REWYND, Library active /
Settings, "● Recording: Desktop", "Check for updates", version) and a main pane with the
LIBRARY heading, "N clips · size on disk", search field, game filter pills, and clips
**grouped by game** (Valorant / Counter-Strike 2 / Overwatch 2) with named highlight
clips.

It is CSS placeholder art, not a screenshot — swap for a real capture when available.
The clip names and game grouping are slightly ahead of the app (they imply per-clip
naming + game detection/tagging); keep them honest as the app catches up.

## 6. Data honesty

No invented metrics. No fake CPU %, download counts, "players online," or star totals.
Everything shown maps to a real capability or a stated target (1080p60). Beta status is
visible from the top, not buried.

## 7. Tech

- **Astro** (static output, one page, no client framework) in `site/`. Componentized:
  `layouts/Base.astro` + section components in `src/components/`, the placeholder clip
  list in `src/data/clips.ts`, and the Arena system in `src/styles/global.css`. Assets
  in `public/assets/`. `npm run build` → static `dist/`, hostable anywhere.
- Google Fonts (Barlow Condensed + Inter) via `<link>` in `Base.astro`.
- Minimal progressive-enhancement JS (inline `<script>` in `Hero.astro`): OS-detect the
  primary download label + copy buttons. Page is fully functional without JS.
- Accessibility: semantic landmarks, focus-visible rings, reduced-motion honored.

## 8. Open items

- Real **domain** (placeholder: `rewynd.gg`) — appears in the install command, links, OG.
- Real **screenshot** for the hero; proper **1200×630 OG card**.
- **Discord** URL.
- Optional later: light mode, a real `/download` and `/docs` route, animated hero clip.

---

## Appendix — build/handoff prompt

For rebuilding or restyling the page from scratch (e.g. a fresh design pass):

> Build a single-page static marketing site for **rewynd**, a lightweight native
> instant-replay clip recorder (Linux/Windows/macOS-beta, open source, GPL-3.0, part of
> ganked.tv). Use the **Arena design system**: near-black surfaces (`#0b0b0f`/`#111116`/
> `#18181f`), one mint accent (`#00e5a0`), borders not shadows, Barlow Condensed
> (uppercase display) + Inter (body), 8px radii, no UI gradients except the logo mark and
> a subtle hero glow. Dark mode only.
>
> Position it **standalone + open-source first**, ganked.tv as an integration. Sections
> in order: beta bar → sticky nav → stacked hero (H1 "Instant replay for your gameplay.",
> subhead, one-line `curl … | sh` install with copy, Download + Star CTAs, beta note,
> platform chips, then a large product shot of the app's game-grouped clip Library with a
> mint glow) → honest 4-tile trust strip → how-it-works (3 steps) → 6 feature cards →
> zero-copy pipeline diagram → ganked.tv upload strip → open-source band with a repo card
> → download band (Linux AppImage / Windows installer / macOS build-from-source) → footer.
>
> Be data-honest: no invented CPU %, download counts, or star totals. Keep beta/WIP
> visible from the top. Ship as a fully static build (`npm run build` → `dist/`,
> hostable anywhere). The logo is the mint HUD-frame play mark. Reference
> implementation: `site/`.
