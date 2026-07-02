# ganked.tv "Arena" design system — spec for iced 0.14 recreation

Extracted from `GankedTV Arena.dc.html` (primary board, incl. a real `/settings` page),
`GankedNav.dc.html` (nav component) and `ganked.tv Logo.dc.html` (logo colors).
All values are quoted verbatim from the source CSS. This spec replaces reading the HTML.

The board's own "load-bearing rules" (quoted from the file — treat as law):

> - One mint accent owns every interactive state
> - Depth from surface color, never box-shadow
> - No hover transforms — game tiles are the one exception
> - Plain section names, no editorial kickers
> - No issue numbers — ranks are plain numerals
> - 8px card radius, 6px inputs, full only on avatars
> - No UI gradients except the thumbnail legibility overlay
> - Backdrop blur on the nav only
> - Feed tabs are underlines; pills are filter-only
> - Light mode deepens mint to #00b87d for AA contrast

---

## 1. Color tokens

### 1.1 Dark theme (primary; `:root`)

| Token | Value | Role |
|---|---|---|
| `--color-surface-base` | `#0b0b0f` | Window/page background (near-black, slight blue) |
| `--color-surface-raised` | `#111116` | Cards, panels, nav bar fill |
| `--color-surface-high` | `#18181f` | Inputs, wells, segmented-control track, unread-row highlight, avatar fill |
| `--color-text-primary` | `#f0f0f4` | Headings, card titles, values |
| `--color-text-secondary` | `rgba(255,255,255,0.50)` | Body copy, field labels, secondary buttons |
| `--color-text-muted` | `rgba(255,255,255,0.28)` | Captions, placeholders, inactive tabs, meta |
| `--color-border` | `rgba(255,255,255,0.07)` | Default 1px border for cards, inputs, dividers |
| `--color-border-strong` | `rgba(255,255,255,0.12)` | Emphasized borders: outline buttons, screen frames, avatar rings, scrollbar thumb |
| `--color-accent` | `#00e5a0` | THE accent (mint). Primary buttons, active states, links, live dots, focus borders |
| `--color-accent-bg` | `rgba(0,229,160,0.08)` | Tint fill behind accent chips/badges/active nav pill |
| `--color-accent-border` | `rgba(0,229,160,0.25)` | Border for accent chips/badges; hover border on tiles |

Non-token recurring colors:

| Value | Role |
|---|---|
| `#08120e` | Ink on accent — text/icon color on every mint-filled button/badge |
| `#f4f1e8` | Warm off-white used ONLY on top of video/thumbnail imagery (overlay titles, play icons, duration text) |
| `rgba(0,0,0,0.75)` | Duration-badge scrim on thumbnails |
| `rgba(0,0,0,0.55)` + `1px rgba(255,255,255,0.3)` border | Circular play button on thumbnails |
| `linear-gradient(transparent, rgba(0,0,0,0.85–0.88))` | Thumbnail legibility scrim (the only sanctioned UI gradient) |
| `#4285F4` | One-off Google button glyph square |
| Placeholder thumbnail tones | `#15161c #191a22 #13171b #1b1822 #16191e #1a1622 #141a1e #181520 #12161a #171419` (clips) and `#1a1c24 #16131c #121a20 #1c1814 #141a18 #181420 #101418 #1a1416 #13181c #181a20` (game covers), `#14171c` (hero) |

**Pre-composited solid equivalents** (for places where you prefer opaque colors; computed over the surface they usually sit on — iced supports alpha, so the rgba values also work as `Color::from_rgba8`):

| Token | over `#0b0b0f` (base) | over `#111116` (raised) |
|---|---|---|
| text-secondary (50% white) | `#858587` | `#88888b` |
| text-muted (28% white) | `#4f4f53` | `#545457` |
| border (7% white) | `#1c1c20` | `#222226` |
| border-strong (12% white) | `#28282c` | `#2e2e32` |
| accent-bg (8% mint) | `#0a1c1b` | `#102221` |
| accent-border (25% mint) | `#084233` | `#0d4639` |

Primary-button hover is `filter:brightness(1.06)` on `#00e5a0` → approximate solid `#0df3ab` (or just `#1aeaaa`).

### 1.2 Light theme (`:root[data-theme="light"]`) — optional, included for completeness

| Token | Value |
|---|---|
| surface-base | `#f7f5f0` (warm cream, NOT white) |
| surface-raised | `#ffffff` |
| surface-high | `#f0ece3` |
| text-primary | `#1a1a22` |
| text-secondary | `#888070` |
| text-muted | `#b0a898` |
| border | `#e8e4dc` |
| border-strong | `#d0ccc0` |
| accent | `#00b87d` (deepened mint for AA) |
| accent-bg | `#e8faf4` |
| accent-border | `#b3ead7` |

Ink on accent stays `#08120e` in both themes.

### 1.3 Logo colors (from `ganked.tv Logo.dc.html`)

- Frame gradient: `#7dffd8 → #00e5a0 → #00a376` (linear, TL→BR)
- Screen (inner) radial: `#13211a → #060b09`
- Play glyph gradient: `#b6ffe6 → #00e5a0`
- Simplified/flat (favicon) variant: mint `#00e5a0` frame + play on `#070d0a` screen — **use this flat pair when gradients are unavailable**
- Glow: `drop-shadow(0 0 4–5px rgba(0,229,160,0.45–0.5))` (decorative)
- Wordmark: `GANKED` in text-primary + `.TV` in accent, Barlow Condensed 900, uppercase, letter-spacing 0.04em

---

## 2. Typography

### 2.1 Families

- **Display: `'Barlow Condensed', sans-serif`** — weights 600/700/800/900 (Google Fonts; OFL). All display uses are UPPERCASE with *positive* letter-spacing 0.01–0.04em.
- **UI/body: `Inter, sans-serif`** — weights 400/500/600/700 (occasionally 800 for tiny count badges).
- **Code/route badges: `ui-monospace, Menlo, monospace`**.

Linux/iced reality: bundle Barlow Condensed + Inter TTFs via `include_bytes!` (both OFL). Realistic system fallbacks: Inter → `Noto Sans`/`DejaVu Sans`; Barlow Condensed → `Liberation Sans Narrow`/`DejaVu Sans Condensed` (imperfect; bundling is strongly preferred).

### 2.2 Scale (as used)

| Role | Spec (font shorthand from source) |
|---|---|
| Page H1 (e.g. "Settings") | Barlow Cond **900 32px** (38–42px on bigger pages), uppercase, ls 0.02em, line-height 1, text-primary |
| Section title ("Top Games") | Barlow Cond **800 20–22px**, uppercase, ls 0.02–0.03em, text-primary |
| Sub-panel title ("Comments · 4", "Sign in") | Barlow Cond **800 16–17px**, uppercase, ls 0.03em |
| Wordmark | Barlow Cond **900 16–20px**, uppercase, ls 0.04em |
| Big stat number | Barlow Cond **900 22–28px**, line-height 1 |
| Rank numeral | Barlow Cond **900 15–22px**; #1 in accent, rest in text-muted |
| Kicker label ("Browse", "Live") | Inter **700 10px**, ls **0.14em**, uppercase, **accent** color |
| Panel eyebrow ("Step 2 · Details") | Inter **700 10px**, ls **0.12em**, uppercase, text-muted (accent when the panel is active) |
| Form field label | Inter **700 9–10px**, ls **0.10em**, uppercase, text-secondary, 5–6px below-gap |
| Body copy | Inter **400 12.5–13px / 1.5–1.6**, text-secondary |
| Card title | Inter **600 12px / 1.3**, text-primary |
| List row title | Inter **600 11.5px / 1.25**, text-primary |
| Meta/caption ("@author · 248k views") | Inter **400 10–11px**, text-muted (author name in accent, 500–600) |
| Buttons | Inter **700 12–13px** (primary), **600–700 11–12px** (secondary/small) |
| Nav links / tabs | Inter **600 12px** |
| Chips/badges | Inter **700 9–10px**, ls 0.07em, uppercase |
| Input value text | Inter **500 12px** (14px w/ ls 0.18em for password dots) |
| kbd hint (⌘K / esc) | Inter **600 10px**, text-muted |

Uppercase usage: ALL Barlow Condensed display text, all kickers/eyebrows/field labels, stat labels. Body, buttons, card titles, meta are sentence case.

---

## 3. Shape language

| Component | Radius | Border | Fill |
|---|---|---|---|
| Cards / panels | **8px** | 1px `border` | `surface-raised` |
| Screen frames / login card / notif dropdown | 10–12px | 1px `border-strong` | `surface-base` / `surface-raised` |
| Text inputs | **6px** | 1px `border` (focus: 1px `accent`) | `surface-high` |
| Buttons (all) | **8px** (7px small ≤32px, 6px inside wizard cards) | primary: none; secondary: 1px `border-strong`; tertiary: 1px `border` | primary: `accent`; others transparent |
| Nav-link hover pill / hero tabs | 7px | — / 1px | `accent-bg` when active |
| Game-tag badge | 5px | 1px `accent-border` | `accent-bg` |
| Duration badge / kbd | 4px | — / 1px `border` | `rgba(0,0,0,0.75)` / transparent |
| Filter pills, tag chips | 9999px (full) | 1px (`accent-border` active, `border` inactive) | `accent-bg` active, transparent inactive |
| Avatars, status dots, play button, progress bar | full circle / 9999px | avatar ring: 1px `accent` (self) or `border-strong` (others) | `surface-high` |
| Segmented control container | 8px (7px joined variant) | 1px `border` | `surface-high`, 3px inner padding |
| Dropzone | 8px | **1.5px dashed** `border-strong` | transparent |
| Mobile device frame | 30px | 1px `border-strong` | base |

Border width is **1px everywhere** (1.5px dashed dropzone; 2px only: tab underline, profile avatar ring, notification-badge ring).

**Shadows:** rule is "Depth from surface color, never box-shadow". Only two exceptions, both decorative: the nav notification dropdown (`box-shadow:0 18px 48px -12px rgba(0,0,0,0.55)`) and logo glow drop-shadows. In iced: skip shadows entirely; use `border-strong` + the surface ladder (base → raised → high) for depth. That is exactly how the design itself creates depth, so nothing is lost.

**Gradients:** none in UI chrome (only logo mark + thumbnail bottom scrims). Solid fills fully preserve the look.

---

## 4. Spacing scale

Recurring values (px): **2/3/4 · 6/7 · 8/9/10 · 11/12/13/14 · 16/18/20/22 · 24/26/28/30 · 40**.

- Page shell: `max-width` 480 (settings) / 680 / 760 / 1000–1200; padding `30px 28px 40px`; content column `gap:24–30px`.
- Card padding: **18–20px** (`18px` wizard/settings-style cards, `20px 22px` prose cards); compact info panel `13–15px`; clip-card text block `11px 12px 13px`.
- Section header: kicker + title on one baseline row, `gap:12px`, `margin-bottom:15–18px`.
- Form stacks: field gap **13–16px**; label→input gap **5–6px**; button gets extra `margin-top:6px`.
- Grid gaps: **12–14px** (cards), 16px (panel columns), 20px (main/sidebar split).
- Nav: height **56px**, `padding:0 22px`, `gap:18px`; nav-link padding `6px 10px`.
- Tabs: `padding:9px 14px 11px`, container `gap:4–6px`.
- Buttons: heights 28 (inline follow) / 32 / 34 (nav) / 36 (action row) / 38 / 40 / **42 (form CTA)**; horiz padding `0 12–22px`; icon gap 7px.
- List rows: `padding:10–13px 0` + 1px bottom border; grid columns with `gap:11–14px`.
- Inputs: height **40px** (38 wizard, 48 large search), `padding:0 12px`.

---

## 5. Component inventory for a SETTINGS window

### 5.1 The actual Settings page (quote)

```html
<div style="max-width:480px;margin:0 auto;padding:30px 28px 40px;">
  <h2 style="margin:0 0 22px;font:900 32px 'Barlow Condensed',sans-serif;letter-spacing:0.02em;
             text-transform:uppercase;color:var(--color-text-primary);line-height:1;">Settings</h2>
  <div style="display:flex;flex-direction:column;gap:16px;">
    <div>
      <div style="font:700 10px Inter,sans-serif;letter-spacing:0.1em;text-transform:uppercase;
                  color:var(--color-text-secondary);margin-bottom:6px;">Current password</div>
      <div style="height:40px;display:flex;align-items:center;padding:0 12px;
                  background:var(--color-surface-high);border:1px solid var(--color-border);
                  border-radius:6px;font:600 14px Inter,sans-serif;letter-spacing:0.18em;
                  color:var(--color-text-primary);">••••••••••</div>
    </div>
    <!-- focused field: identical but border:1px solid var(--color-accent) -->
    <button style="height:42px;margin-top:6px;border:none;border-radius:8px;
                   background:var(--color-accent);color:#08120e;font:700 13px Inter,sans-serif;"
            style-hover="filter:brightness(1.06);">Save changes</button>
  </div>
</div>
```

Pattern: narrow centered column, huge condensed uppercase H1, unboxed field stack (no card wrapper on this page), full-width mint CTA. For a multi-section settings window, wrap groups in the standard card (below) with an eyebrow label per card, as the Upload wizard does.

### 5.2 Cards / panels

```css
background:var(--color-surface-raised); border:1px solid var(--color-border);
border-radius:8px; padding:18px;   /* 20px–22px for prose */
```
Active/highlighted card swaps border to `var(--color-accent-border)` (see wizard Step 2). Eyebrow inside card: `font:700 10px Inter; letter-spacing:0.12em; uppercase; color:var(--color-text-muted); margin-bottom:13px;` (accent-colored when card is active).

### 5.3 Section headers (page level)

```html
<span style="font:700 10px Inter;letter-spacing:0.14em;text-transform:uppercase;color:var(--color-accent);">Browse</span>
<span style="font:800 20px 'Barlow Condensed';letter-spacing:0.03em;text-transform:uppercase;color:var(--color-text-primary);">Top Games</span>
```
Baseline-aligned row, gap 12px; optional right-aligned accent link (`600 11px Inter`, accent). Sections separated by `border-top:1px solid var(--color-border); padding-top:24–28px`.

### 5.4 Buttons

| Variant | Style | Hover |
|---|---|---|
| **Primary** | `height:38–42px; padding:0 14–22px; border:none; border-radius:8px; background:var(--color-accent); color:#08120e; font:700 12–13px Inter` | `filter:brightness(1.06)` → lighter mint `#0df3ab` |
| **Secondary (outline)** | `background:transparent; border:1px solid var(--color-border-strong); border-radius:8px; color:var(--color-text-secondary); font:600–700 11–12px Inter; height:32–38px` | `border-color:var(--color-accent); color:var(--color-accent)` |
| **Tertiary (quiet)** | same but `border:1px solid var(--color-border)` | `border-color:var(--color-border-strong); color:var(--color-text-primary)` |
| **Icon button** | `34–38px square; border:1px solid var(--color-border); border-radius:8px; background:transparent; color:var(--color-text-secondary)` | border-strong (+accent for emphasized) |
| **Disabled** | outline button with `color:var(--color-text-muted)` (wizard "View clip"); tabs use `opacity:0.4; cursor:not-allowed` |

**There is NO red/danger style.** Destructive "Remove" (admin) is just the secondary outline whose hover turns *accent*; error text is mint too (`font:500 11px Inter; color:var(--color-accent)` under the field — "mint error text" per the roadmap). Keep the one-accent rule: do not invent a red.

### 5.5 Text inputs

```css
height:40px; padding:0 12px; background:var(--color-surface-high);
border:1px solid var(--color-border); border-radius:6px;
font:500 12px Inter; color:var(--color-text-primary);
/* placeholder */ font:400 12px Inter; color:var(--color-text-muted);
/* focused    */ border:1px solid var(--color-accent);
/* big search */ height:48px; padding:0 16px; border:1px solid var(--color-accent);
                 outline:3px solid var(--color-accent-bg);  /* soft focus halo */
```
In iced: focus = accent 1px border; the 3px halo can be approximated by a wrapping container with `accent-bg` background and 3px padding, or skipped.

### 5.6 Dropdown / select

Rendered as an input shell with the current value + optional leading swatch, and a `▾` affordance:
```html
<div style="height:38px;...;background:var(--color-surface-high);border:1px solid var(--color-border);border-radius:6px;">
  <span style="width:18px;height:24px;border-radius:3px;background:#1a1c24;"></span>
  <span style="font:500 12px Inter;color:var(--color-text-primary);">Counter-Strike 2</span>
</div>
<!-- compact sort dropdown: -->
<span style="font:600 10px Inter;letter-spacing:0.04em;text-transform:uppercase;color:var(--color-text-muted);">Top ▾</span>
```
For iced `pick_list`: surface-high fill, 6px radius, 1px border, muted chevron; menu = surface-base bg, border-strong 1px, 12px radius (per notif dropdown), hover row `surface-high`.

### 5.7 Toggles / checkboxes / segmented controls

No literal checkbox/toggle exists. Two sanctioned patterns:

- **Checkmark chip** (checked state): `width/height:15px; border-radius:5px; background:var(--color-accent-bg); border:1px solid var(--color-accent-border);` containing a 10px accent `✓`. Unchecked equivalent: `border:1px solid var(--color-border-strong)`, empty (cf. wizard step-3 circle).
- **Segmented control** (boolean/enum choice — use this instead of a toggle):
```html
<div style="display:inline-flex;gap:2px;padding:3px;border:1px solid var(--color-border);
            border-radius:8px;background:var(--color-surface-high);">
  <button style="padding:5px 15px;border-radius:6px;border:none;
                 background:var(--color-accent);color:#08120e;font:600 12px Inter;">Dark</button>
  <button style="...;background:transparent;color:var(--color-text-secondary);">Light</button>
</div>
```
Joined full-width variant (Public/Unlisted, Upload/Import): equal-flex segments, `font:600 11px Inter`, active = accent bg + `#08120e`, inactive = transparent + 1px `border` + text-secondary, 6–7px radius.

### 5.8 Sliders / progress

Only a progress bar exists:
```css
/* track */ height:6px; border-radius:9999px; background:var(--color-surface-high);
/* fill  */ height:100%; background:var(--color-accent); border-radius:9999px;
/* label */ font:600 11px Inter; color:var(--color-text-secondary);  /* "60%" in accent */
```
A slider should reuse this: 6px track surface-high, accent fill, circular accent handle.

### 5.9 Chips / badges / status

- Game tag: `font:700 9px Inter; ls 0.07em; uppercase; color:accent; background:accent-bg; border:1px accent-border; border-radius:5px; padding:3px 6–7px`.
- Filter pill (active): `font:600 11px Inter; color:accent; background:accent-bg; border:1px accent-border; border-radius:9999px; padding:4px 12px`; inactive: text-muted + 1px `border`, transparent.
- Removable tag chip: pill, `padding:3px 9px`, text "clutch ×".
- Count badge ("2 new"): `font:700 9px Inter; accent on accent-bg; 1px accent-border; radius 5px; padding 2px 6px`.
- Route/code badge: `font:600 10px ui-monospace; accent on accent-bg; 1px accent-border; radius 5px; padding 2px 7px`.
- Live/status dot: `7px` circle, `background:accent` (+ label `500 11px Inter` text-secondary).
- Notification unread dot: 7–8px accent circle; unread row gets `background:var(--color-surface-high)` (8px radius on full page).

### 5.10 Tabs

Underline tabs only: `font:600 12px Inter; padding:9px 14px 11px; border-bottom:2px solid` — active: accent underline + `text-primary`; inactive: transparent underline + `text-muted` (hover → text-secondary); disabled: `opacity:0.4`. Container has `border-bottom:1px solid var(--color-border)`.

### 5.11 Dividers

Plain `1px solid var(--color-border)` lines (`border-top`/`border-bottom`). Labeled divider: two 1px flex lines + centered `font:500 10px Inter` muted "OR". Vertical divider in nav: `border-left:1px solid var(--color-border); padding-left:10px`.

### 5.12 Stepper (wizard)

Step marker: `22px` circle — done/active: `background:accent` + `#08120e` numeral (700 11px); todo: transparent + `1px border-strong` ring + muted. Label `font:700 12px Inter` (accent when active, muted when todo). Connector: `width:34px; height:1px; background:var(--color-border-strong)`.

### 5.13 Avatars

Circle, `background:var(--color-surface-high)`, ring `1px solid var(--color-accent)` (own account/highlighted) or `1px solid var(--color-border-strong)` (others), initials `font:700 10–12px Inter` in accent or text-secondary respectively. Sizes 22–42px (80px profile hero with 2px ring).

---

## 6. Nav / header treatment (GankedNav) + overall look

**Nav bar:** height 56px, `padding:0 22px`, `gap:18px`, `border-bottom:1px solid var(--color-border)`, background `color-mix(in srgb, var(--color-surface-raised) 90%, transparent)` + `backdrop-filter:blur(12px)`. In iced: **solid `#111116`** (nothing scrolls under a native settings titlebar, so the blur is moot). Contents: logo mark (23px) + wordmark; nav links (`600 12px Inter`, text-secondary, `padding:6px 10px`, radius 7px; active = accent text on accent-bg pill; hover = accent text); centered search shell (36px, surface-raised, 1px border, radius 8, ⌘K kbd chip); status dot + "12,847 online"; mint Upload button (34px, radius 8); bell icon button (34px, 1px border, radius 8) with accent count bubble (`2px solid var(--color-surface-raised)` ring); 30px avatar with accent ring.

**Notification dropdown:** `width:344px; background:var(--color-surface-base); border:1px solid var(--color-border-strong); border-radius:12px;` header row `13px 15px` w/ bottom border; rows grid `30px 1fr auto auto`, `padding:11px 15px`, unread bg `surface-high`; footer link accent 700 11px, hover bg accent-bg. (Its box-shadow is the one exception — replace with the border-strong outline.)

**Aesthetic summary:** a flat, near-black esports UI in three closely-spaced blue-black surfaces (`#0b0b0f → #111116 → #18181f`), hairline white-alpha borders instead of shadows, and exactly one neon-mint accent (`#00e5a0`) carrying every interactive/active/brand moment, always paired with dark-green ink `#08120e` when used as a fill. Display type is loud condensed uppercase (Barlow Condensed 800/900) against small, quiet Inter UI text (10–13px), giving a broadcast-graphics feel. No gradients, no glass, no glow in the chrome — the whole look survives solid-color recreation perfectly; the only web-only tricks (nav blur, dropdown shadow, thumbnail scrims, logo gradient) are decorative and have stated flat fallbacks (solid raised nav, strong border, flat `#070d0a`/`#00e5a0` logo).

---

## 7. Animated / interactive — decorative only

- Hover transitions `.15s` on color/border/filter everywhere (border→strong or →accent; text→accent; primary button `brightness(1.06)`).
- Game tiles: `transform:translateY(-2px)` + accent-border on hover — the *only* transform in the system.
- Hero treatment 1e auto-rotates every **3200ms** (pauses via "● Auto / ▶ Play" toggle) — purely a board demo.
- Theme toggle persists to `localStorage('gtv-theme')`; segmented `[data-seg]` buttons restyle via CSS attribute selectors.
- Nav bell toggles the dropdown; backdrop blur on nav/tab-bar; logo drop-shadow glows.
- None of these are load-bearing; static equivalents (hover color swaps only) are faithful.
