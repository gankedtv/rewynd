# Trim filmstrip preview + destination-brand accent — design

Covers three issues, shipped as **one PR** (bundled so CodeRabbit spends one review slot):

- **#51** — clip trim tool for the upload flow: add a **live filmstrip preview** to the
  existing lossless trim. (Spatial crop is explicitly deferred — see Out of scope.)
- **#111** — make every accent in the upload panel follow the destination brand (mint for
  ganked.tv, red for YouTube), not just the pill and the Upload button.
- **#110** — **fade** that accent when the destination switches, instead of a hard swap.

Suggested branch: `51-trim-preview-and-brand-accent` off `main`.

All UI lives in `crates/settings/` (the `library`, `theme`, `thumbs` modules); the clip
read/trim/preview helpers live in `crates/mux/src/read.rs`. iced 0.14, tiny-skia software
renderer (ADR 0006); no wgpu.

---

## Part A — Unified, animated brand accent (#111 + #110)

These two issues are one change: #111 makes a single accent value drive the whole upload
panel; #110 makes that value fade on a destination switch. Doing them separately would edit
the same call sites twice, so they land together.

### A single accent source

Today the accent is read two ways: `dest_accent(dest)` (mint/red pair) at the pill and the
Upload button, and a hardcoded `palette::ACCENT` at the card eyebrow, the success line, and the
share link. That is why only the pill and button recolor.

Add one method to `Library`:

```rust
/// The accent (fill, ink) the upload panel paints with right now: the destination brand,
/// or the interpolated value while a switch is fading.
fn current_accent(&self) -> (Color, Color)
```

- No transition in flight → `dest_accent(self.dest)` (unchanged behaviour).
- Transition in flight → per-channel lerp `from → to` of both the fill and the ink, by an
  eased progress (smoothstep over `ACCENT_FADE`, ~220 ms).

Every accent in the upload panel reads from `current_accent()`:

| Element | Today | After |
| --- | --- | --- |
| Active destination pill | `dest_accent(dest)` per segment | active segment uses `current_accent().0` |
| "Upload to …" button | `dest_accent(self.dest)` + hover | `current_accent()` + matching hover |
| Card eyebrow "UPLOAD" | `card(...)` → hardcoded `ACCENT` | new `card_accent(title, accent, content)` |
| Status success line | `tinted(palette::ACCENT)` | `tinted(current_accent().0)` |
| Share link (`link_actions`) | `tinted(palette::ACCENT)` | `tinted(accent)` param |

`theme::card_accent(title, accent, content)` is the brand-aware card; existing `theme::card`
delegates to it with `palette::ACCENT`, so every **other** card (RECORDING, AUDIO, TRIM, …)
stays mint with no change. `upload_status` and the free fn `link_actions` gain an
`accent: Color` parameter, fed `current_accent().0` from the upload panel.

**Not recolored** (per #111): the grey field labels (TITLE/DESTINATION/VISIBILITY), and the
TRIM card's own "Saved a trimmed clip." line — trimming is destination-agnostic, so it stays
mint. The `accent_chip("On YouTube")` library badges are outside the upload panel and stay mint.

### Fade plumbing

State added to `Library`:

```rust
/// An in-flight accent fade: where it started, the target, and when it began. `None` when idle.
/// `start` anchors lazily on the first frame tick, so all time comes from the subscription.
struct AccentFade {
    from: (Color, Color),
    to: (Color, Color),
    start: Option<Instant>,
}
accent_fade: Option<AccentFade>,
```

- **`DestPicked(dest)`** (dest actually changed): capture `from = current_accent()` — the value
  shown *now*, so a switch back mid-fade eases from the partial colour instead of snapping —
  set `to = dest_accent(dest)`, `start = <tick instant>`, store the fade.
- **`Tick(Instant)`**: recompute is implicit (the view reads `current_accent()`); when progress
  reaches `1.0`, clear `accent_fade` so the panel settles on the exact brand colour.
- **`fn animating(&self) -> bool`**: `accent_fade` is `Some`.

The instant comes from the subscription, not `Instant::now()` in the update — so progress is a
pure function of `(fade.start, tick_instant)` and is unit-testable with injected instants.

**Subscription (in `App::subscription`, `main.rs`):** fold in `iced::window::frames()` mapped to
`Message::Library(library::Message::Tick(instant))` **only while `self.library.animating()`**.
`window::frames()` yields the frame `Instant` and drives continuous redraws *while subscribed*;
iced re-diffs subscriptions after every update, so the moment the fade clears, the subscription
is dropped and the software renderer returns to idle — no continuous repaint when nothing moves
(the ADR 0006 constraint). `window::frames()` is core iced, so no new Cargo feature.

### Rejected alternative

Thread raw `dest_accent(dest)` into each call site (the literal wording of #111) without the
single `current_accent()` source: it duplicates the mint/red branch at five sites and leaves
nowhere to hang the #110 interpolation. The single source is both DRY and the substrate #110
needs.

---

## Part B — Trim filmstrip live preview (#51)

The lossless trim (Start/End sliders, trimmed-length readout, "Save trimmed clip") already
exists and is correct. It ships blind: the sliders give no picture of where the cut lands. Add
a **filmstrip** above the sliders.

### What the user sees

A horizontal strip of ~12 keyframe thumbnails spanning the whole clip. The kept `[start, end]`
band is shown at full brightness with an accent edge on each side; everything outside is dimmed
by a scrim. It updates live as the sliders move.

Because both the preview and the lossless trim snap to keyframes (~1 s IDR granularity), the
strip shows **exactly** the frames the saved clip will keep — what you see is what you get.

### Decode

`crates/settings/src/thumbs.rs` today decodes one mid-clip keyframe (`load`). Refactor so the
decode + downscale core is shared, and add:

```rust
/// Decode the keyframe nearest `position` (0.0..=1.0) of the clip, downscaled to `width`.
/// In-memory only (no disk cache): filmstrip cells are small and live only while the clip is open.
pub fn load_at(path: &Path, position: f32, width: u32) -> Result<Handle, String>
```

It reuses `rewynd_mux::read::clip_preview_at(path, position)` (already returns the nearest-
keyframe Annex-B) → `decode_first_frame` → `image::thumbnail`. Cells are ~120 px wide.

On clip open, queue `FILMSTRIP_FRAMES` decodes at evenly spaced positions through a small
background pool that mirrors the existing thumbnail pool (`MAX_DECODES`, `pending`/`decoding`
maps, superseding-by-key on completion). Results live in a `Vec<FilmstripCell>` on `Library`,
keyed to the open clip's path; cleared when the detail page closes or another clip opens. A cell
that fails to decode renders as a neutral placeholder (never a crash), matching `Thumb::Failed`.

### Layout — range overlay without per-frame math

`iced::widget::stack!`:

- **Bottom layer:** a `row!` of the decoded cells (placeholder while a cell is still decoding),
  each `Length::FillPortion(1)` so the strip fills the panel width evenly.
- **Top layer:** a `row!` of three width-portioned regions built from the times —
  `[scrim: 0 → start]`, `[clear, accent-bordered: start → end]`, `[scrim: end → dur]` — using
  `Length::FillPortion` with integer portions derived from the start/end/duration. This paints
  the kept range precisely, independent of where the frame boundaries fall, and needs no
  hit-testing.

The scrim is a semi-transparent dark fill (surface-base at ~0.6α); the kept region is a
transparent container with a 1–2 px accent border. The accent here stays **mint** — the TRIM
card is destination-agnostic (Part A leaves it mint).

### Interaction

The **sliders drive the strip** (they are already keyboard-accessible; the strip is pure
visualization that reads `trim_start` / `trim_end`). No new input handling.

### Rejected alternatives

- **Custom draggable-handle widget over the strip:** replaces the working accessible sliders
  with raw mouse handling for marginal gain, and hurts keyboard/screen-reader access (the app's
  accessibility goal). The sliders-drive-strip split keeps one accessible control with a rich
  view.
- **Decode-on-scrub (follow the handle):** heavier and needs debounce; the whole-clip strip
  decoded once on open is cheaper and shows both endpoints and the middle at all times.

Clicking a frame to jump the nearest handle is a plausible later enhancement, not part of this
work.

---

## Data flow

```text
open clip ─▶ read clip_summary (duration) ─▶ reset trim to full span
         └▶ queue FILMSTRIP_FRAMES decodes (clip_preview_at at i/N) ─▶ pool ─▶ cells fill in

drag Start/End slider ─▶ trim_start/trim_end update ─▶ strip overlay re-portions live
                                                     └▶ trimmed-length readout updates

pick destination (changed) ─▶ accent_fade = {from: current_accent(), to: dest_accent, start}
                          └▶ animating() ⇒ window::frames() subscription active
Tick(instant) ─▶ view reads current_accent() (lerp) ─▶ progress ≥ 1 ⇒ clear fade ⇒ subscription drops
```

## Components touched

- `crates/settings/src/library.rs` — `current_accent`, `AccentFade` state, `Tick`/`DestPicked`
  handling, `animating`, filmstrip state + decode-pool, `trim_panel` gains the strip,
  `upload_panel`/`upload_status`/`link_actions` read the accent param.
- `crates/settings/src/theme.rs` — `card_accent(title, accent, content)`; `card` delegates to it.
- `crates/settings/src/thumbs.rs` — shared decode core + `load_at`.
- `crates/settings/src/main.rs` — conditional `window::frames()` subscription; `Tick` routing.

No changes to `crates/mux` (reuses `clip_preview_at`, `trim_clip`) or config.

## Testing

Library crates gate 85% coverage; the iced GUI code (`settings/src/`) is excluded from the gate
(no display in CI), so tests target the pure logic:

- **Accent lerp** (`current_accent` / a `lerp_accent(from, to, t)` helper): endpoints return the
  exact brand colours at `t=0`/`t=1`; midpoint is between; `animating()` flips false when a fade
  completes. Injected instants, no real clock. These live in `settings` unit tests (runnable
  without a display) even though the crate is coverage-excluded.
- **Filmstrip positions:** the helper that maps `FILMSTRIP_FRAMES` → evenly spaced
  `0.0..=1.0` positions is a pure function; assert count, monotonicity, and the range overlay
  portion math (start/end/dur → three integer portions) at edges (start=0, end=dur, empty span).
- **`thumbs::load_at`** shares the decode path already exercised by `load`; a decode of a
  synthetic multi-keyframe clip (as in `read.rs` tests) at a mid position returns a handle.
- Existing `mux::read` trim/preview tests are unchanged and still cover the lossless cut.

Manual (GPU/display box): open a clip, drag the trim, confirm the strip highlights the kept
range and the saved clip matches; switch ganked.tv ↔ YouTube and confirm the whole panel
(pill, button, "UPLOAD" eyebrow, success line, link) fades mint ↔ red and settles, with no
repaint while idle.

## Out of scope

- **Spatial frame crop** (#51 "possibly"): a pixel crop forces a full decode + re-encode of
  every frame (the GPU encode pipeline) and breaks the lossless keyframe-cut model. Deferred to
  its own future issue; not built here.
- Text-recolor of anything outside the upload panel.
- Making the filmstrip an input control (drag handles / click-to-seek).
