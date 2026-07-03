# Trim filmstrip preview + destination-brand accent — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a live filmstrip preview to the lossless clip trim (#51), make the whole upload panel follow the destination brand colour (#111), and fade that colour on a destination switch (#110) — one PR.

**Architecture:** A single `Library::current_accent()` returns the upload panel's colour (destination brand, or an interpolated value during a fade); every accent in the panel reads it. A `window::frames()` subscription runs only while a fade is in flight. The trim panel gains a keyframe filmstrip (decoded via the existing `clip_preview_at` reader through a small background pool) whose kept-range band is drawn with a three-portion overlay.

**Tech Stack:** Rust, iced 0.14 (tiny-skia software renderer, no wgpu), openh264 decode, `rewynd_mux::read`, `image`.

## Global Constraints

- **iced 0.14, tiny-skia software renderer only** (ADR 0006); no wgpu. No continuous redraw while idle.
- **GPU pin unchanged:** do not touch `wgpu`/`gpu-video` (ADR 0001) — this work needs neither.
- **Resolution/framerate/bitrate stay parameters**, never hard-coded.
- **Comments minimal:** only non-obvious rationale/invariants/SAFETY. **No issue/PR numbers in source** (commits/PR only).
- **No AI attribution** in commits/PRs; human-sounding messages.
- **Coverage:** `crates/settings/src/` is excluded from the 85% gate (no display in CI). Put unit tests on pure logic anyway — they run under `cargo test` without a display and guard the math.
- **Build gates (all green before push):** `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`.
- Branch: `51-trim-preview-and-brand-accent` off `main` (already created; the design spec is committed on it).

---

### Task 1: Brand-aware card eyebrow (`theme::card_accent`)

**Files:**
- Modify: `crates/settings/src/theme.rs:103-117` (the `card` fn)

**Interfaces:**
- Produces: `pub fn card_accent<'a, M: 'a>(title: &'a str, accent: iced::Color, content: impl Into<Element<'a, M>>) -> Element<'a, M>` — a card whose eyebrow uses `accent`. `card(title, content)` delegates to it with `palette::ACCENT` (unchanged behaviour for every existing caller).

- [ ] **Step 1: Replace `card` with a delegating pair**

In `crates/settings/src/theme.rs`, replace the existing `card` fn (lines ~103-117) with:

```rust
/// A grouped card, Arena style: raised panel, hairline border, 8px radius, with the
/// title as a small uppercase eyebrow in the accent (mint by default).
pub fn card<'a, M: 'a>(title: &'a str, content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    card_accent(title, palette::ACCENT, content)
}

/// [`card`] with an explicit eyebrow accent, for the upload panel whose colour follows the
/// chosen destination (mint for ganked.tv, red for YouTube).
pub fn card_accent<'a, M: 'a>(
    title: &'a str,
    accent: iced::Color,
    content: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    let inner = column![
        text(title).size(10).font(UI_BOLD).style(tinted(accent)),
        content.into(),
    ]
    .spacing(14);
    container(inner)
        .width(Length::Fill)
        .padding(18)
        .style(card_style)
        .into()
}
```

- [ ] **Step 2: Build**

Run: `cargo build -p rewynd-settings`
Expected: compiles (all existing `card(...)` calls unchanged).

- [ ] **Step 3: Commit**

```bash
git add crates/settings/src/theme.rs
git commit -m "settings: add card_accent for a brand-aware card eyebrow"
```

---

### Task 2: Arbitrary-position filmstrip decode (`thumbs::load_at`)

**Files:**
- Modify: `crates/settings/src/thumbs.rs` (add `scaled_dims`, refactor `thumb_dims`, add `load_at`)
- Test: `crates/settings/src/thumbs.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `rewynd_mux::read::clip_preview_at(path, position) -> Result<(ClipSummary, Vec<u8>)>` (exists), `decode_first_frame` (exists, private).
- Produces: `pub fn load_at(path: &Path, position: f32, width: u32) -> Result<iced::widget::image::Handle, String>` — decode the keyframe nearest `position` (0.0..=1.0), downscaled to at most `width` px wide, as an in-memory handle (no disk cache). `fn scaled_dims(width: u32, height: u32, cap: u32) -> (u32, u32)`.

- [ ] **Step 1: Write the failing test for `scaled_dims`**

Add to the `tests` module in `crates/settings/src/thumbs.rs`:

```rust
#[test]
fn scaled_dims_caps_to_the_given_width() {
    assert_eq!(scaled_dims(1920, 1080, 120), (120, 67));
    assert_eq!(scaled_dims(1920, 1080, 480), (480, 270));
    assert_eq!(scaled_dims(64, 36, 120), (64, 36), "smaller than cap stays as-is");
    assert_eq!(scaled_dims(0, 0, 120), (1, 1), "degenerate input stays drawable");
}
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test -p rewynd-settings scaled_dims_caps -- --nocapture`
Expected: FAIL — `cannot find function scaled_dims`.

- [ ] **Step 3: Add `scaled_dims` and refactor `thumb_dims` onto it**

Replace `thumb_dims` (near line 223) with:

```rust
/// Thumbnail dimensions: capped at [`THUMB_WIDTH`] keeping the aspect ratio.
fn thumb_dims(width: u32, height: u32) -> (u32, u32) {
    scaled_dims(width, height, THUMB_WIDTH)
}

/// Dimensions capped at `cap` px wide, keeping the aspect ratio; smaller frames stay as they
/// are. Degenerate input collapses to 1x1 so the result is always drawable.
fn scaled_dims(width: u32, height: u32, cap: u32) -> (u32, u32) {
    if width <= cap || width == 0 {
        return (width.max(1), height.max(1));
    }
    let sh = (u64::from(height) * u64::from(cap) / u64::from(width)) as u32;
    (cap, sh.max(1))
}
```

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cargo test -p rewynd-settings scaled_dims_caps`
Expected: PASS. Also run `cargo test -p rewynd-settings thumb_dims_cap` — the existing `thumb_dims` test still passes.

- [ ] **Step 5: Add `load_at`**

After `load` (near line 64) add:

```rust
/// Decode the keyframe nearest `position` (0.0..=1.0) of the clip at `path`, downscaled to at
/// most `width` px wide, as an in-memory handle. No disk cache: filmstrip cells are small and
/// live only while the clip's detail page is open. Blocking; runs on a background task.
pub fn load_at(path: &Path, position: f32, width: u32) -> Result<Handle, String> {
    let (_summary, annexb) =
        rewynd_mux::read::clip_preview_at(path, position).map_err(|e| e.to_string())?;
    let (w, h, rgb) = decode_first_frame(&annexb)?;
    let frame =
        image::RgbImage::from_raw(w, h, rgb).ok_or_else(|| "decoded frame size mismatch".to_owned())?;
    let (tw, th) = scaled_dims(w, h, width);
    let thumb = image::imageops::thumbnail(&frame, tw, th);
    let rgba = image::DynamicImage::ImageRgb8(thumb).into_rgba8();
    Ok(Handle::from_rgba(tw, th, rgba.into_raw()))
}
```

- [ ] **Step 6: Build + commit**

Run: `cargo build -p rewynd-settings` (Expected: compiles.)

```bash
git add crates/settings/src/thumbs.rs
git commit -m "settings: decode a keyframe at an arbitrary clip position (load_at)"
```

---

### Task 3: Accent-fade model (pure logic, TDD)

**Files:**
- Modify: `crates/settings/src/library.rs` (imports, `AccentFade`, helpers, `Library` field + init, `current_accent`, `animating`)
- Test: `crates/settings/src/library.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `dest_accent(dest) -> (Color, Color)` (exists, private in library.rs).
- Produces: `struct AccentFade { from: (Color, Color), to: (Color, Color), start: Option<Instant>, progress: f32 }` with `fn accent(&self) -> (Color, Color)` and `fn advance(&mut self, now: Instant) -> bool` (returns `true` when the fade has reached its end). `fn ease(t: f32) -> f32`, `fn lerp_color(a: Color, b: Color, t: f32) -> Color`. On `Library`: `accent_fade: Option<AccentFade>`, `fn current_accent(&self) -> (Color, Color)`, `pub fn animating(&self) -> bool`.

- [ ] **Step 1: Write the failing tests**

Add a `#[cfg(test)]` module at the end of `crates/settings/src/library.rs` (or into the existing one if present — check first):

```rust
#[cfg(test)]
mod accent_tests {
    use super::*;
    use iced::Color;
    use std::time::{Duration, Instant};

    fn approx(a: Color, b: Color) {
        for (x, y) in [(a.r, b.r), (a.g, b.g), (a.b, b.b), (a.a, b.a)] {
            assert!((x - y).abs() < 1e-4, "{x} vs {y}");
        }
    }

    #[test]
    fn lerp_color_hits_both_ends() {
        let a = Color::from_rgb(0.0, 0.0, 0.0);
        let b = Color::from_rgb(1.0, 0.5, 0.25);
        approx(lerp_color(a, b, 0.0), a);
        approx(lerp_color(a, b, 1.0), b);
        approx(lerp_color(a, b, 0.5), Color::from_rgb(0.5, 0.25, 0.125));
    }

    #[test]
    fn ease_is_clamped_and_smooth() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert_eq!(ease(-1.0), 0.0);
        assert_eq!(ease(2.0), 1.0);
        assert!((ease(0.5) - 0.5).abs() < 1e-6, "symmetric midpoint");
    }

    #[test]
    fn fade_runs_from_source_to_target_then_ends() {
        let from = (palette::ACCENT, palette::INK_ON_ACCENT);
        let to = (palette::YOUTUBE, palette::INK_ON_YOUTUBE);
        let mut fade = AccentFade { from, to, start: None, progress: 0.0 };
        let t0 = Instant::now();

        // First tick anchors the clock; still fully at `from`.
        assert!(!fade.advance(t0));
        approx(fade.accent().0, from.0);

        // Partway through: strictly between the endpoints.
        assert!(!fade.advance(t0 + Duration::from_millis(90)));
        let mid = fade.accent().0;
        assert!(mid.r > from.0.r && mid.r < to.0.r + 1e-3);

        // At/after the duration: reports done and sits on the target.
        assert!(fade.advance(t0 + ACCENT_FADE));
        approx(fade.accent().0, to.0);
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p rewynd-settings accent_tests`
Expected: FAIL — `AccentFade` / `ease` / `lerp_color` not found.

- [ ] **Step 3: Add the model**

At the top of `library.rs`, extend the time import (it already has `use std::time::{Duration, SystemTime};`) to also bring in `Instant`:

```rust
use std::time::{Duration, Instant, SystemTime};
```

Add near the other private helpers (e.g. just above `fn dest_accent`, wherever that lives — search for `fn dest_accent`):

```rust
/// How long the upload-panel accent takes to fade when the destination switches.
const ACCENT_FADE: Duration = Duration::from_millis(180);

/// An in-flight accent fade between two (fill, ink) brand pairs. `start` is anchored on the
/// first tick (so all time comes from the frame subscription, never `Instant::now()` in update).
struct AccentFade {
    from: (iced::Color, iced::Color),
    to: (iced::Color, iced::Color),
    start: Option<Instant>,
    progress: f32,
}

impl AccentFade {
    /// The interpolated (fill, ink) at the current progress.
    fn accent(&self) -> (iced::Color, iced::Color) {
        (
            lerp_color(self.from.0, self.to.0, self.progress),
            lerp_color(self.from.1, self.to.1, self.progress),
        )
    }

    /// Advance to frame time `now`, anchoring the clock on the first call. Returns `true` once
    /// the fade has reached its end (the caller then drops it).
    fn advance(&mut self, now: Instant) -> bool {
        let start = *self.start.get_or_insert(now);
        let linear = now.duration_since(start).as_secs_f32() / ACCENT_FADE.as_secs_f32();
        self.progress = ease(linear);
        linear >= 1.0
    }
}

/// Smoothstep easing, clamped to `0.0..=1.0`.
fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Per-channel linear interpolation between two colours.
fn lerp_color(a: iced::Color, b: iced::Color, t: f32) -> iced::Color {
    iced::Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}
```

Add the field to the `Library` struct (near `dest: Dest,`):

```rust
    /// An in-flight destination-accent fade, or `None` when the panel sits on a fixed brand.
    accent_fade: Option<AccentFade>,
```

Initialise it in `Library::new` (near `dest: Dest::Ganked,`):

```rust
            accent_fade: None,
```

Add the read + animation-flag methods inside `impl Library` (near `current_accent`'s future callers — e.g. just above `fn view`):

```rust
    /// The (fill, ink) accent the upload panel paints with right now: the destination brand,
    /// or the interpolated value while a switch is fading.
    fn current_accent(&self) -> (iced::Color, iced::Color) {
        self.accent_fade
            .as_ref()
            .map_or_else(|| dest_accent(self.dest), AccentFade::accent)
    }

    /// Whether an accent fade is running (drives the frame subscription in `main`).
    pub fn animating(&self) -> bool {
        self.accent_fade.is_some()
    }
```

- [ ] **Step 4: Run tests to confirm they pass**

Run: `cargo test -p rewynd-settings accent_tests`
Expected: PASS (3 tests). `dest_accent` is used by `current_accent`, so no dead-code warning yet even before Task 4 wires it in.

- [ ] **Step 5: Commit**

```bash
git add crates/settings/src/library.rs
git commit -m "settings: model the upload-panel accent fade (pure, tested)"
```

---

### Task 4: Thread `current_accent` through the upload panel (#111)

**Files:**
- Modify: `crates/settings/src/library.rs` — `upload_panel`, `upload_status`, `link_actions`

**Interfaces:**
- Consumes: `self.current_accent()` (Task 3), `theme::card_accent` (Task 1).
- Produces: `fn upload_status(&self, entry, accent: iced::Color) -> Element<Message>` and `fn link_actions(url: &str, accent: iced::Color) -> Element<'static, Message>` (accent-parameterised). After this task the whole panel recolors with `self.dest` (statically — the fade is Task 5).

- [ ] **Step 1: Recolour the destination pill's active segment**

In `upload_panel`, the `seg` closure computes `let (accent, ink) = dest_accent(dest);`. Change it to use the live accent for the **active** segment only (inactive segments never show their accent):

```rust
        let seg = |label: &'static str, dest: Dest, ready: bool| {
            let active = self.dest == dest;
            let (accent, ink) = if active {
                self.current_accent()
            } else {
                dest_accent(dest)
            };
```

(The rest of the closure is unchanged.)

- [ ] **Step 2: Point the Upload button at the live accent**

Still in `upload_panel`, replace:

```rust
        let (accent, ink) = dest_accent(self.dest);
        let accent_hover = match self.dest {
            Dest::Ganked => palette::ACCENT_HOVER,
            Dest::YouTube => palette::YOUTUBE_HOVER,
        };
```

with:

```rust
        let (accent, ink) = self.current_accent();
        // Hover tracks the destination target (a hover mid-fade is rare and cosmetic).
        let accent_hover = match self.dest {
            Dest::Ganked => palette::ACCENT_HOVER,
            Dest::YouTube => palette::YOUTUBE_HOVER,
        };
```

- [ ] **Step 3: Swap the card + status calls to the accent-aware variants**

At the end of `upload_panel`, replace:

```rust
        panel = panel.push(self.upload_status(entry));
        card("UPLOAD", panel)
```

with:

```rust
        panel = panel.push(self.upload_status(entry, accent));
        theme::card_accent("UPLOAD", accent, panel)
```

- [ ] **Step 4: Parameterise `upload_status` and `link_actions` on the accent**

Change the signature of `upload_status` to `fn upload_status(&self, entry: &ClipEntry, accent: iced::Color) -> Element<'_, Message>`. Inside it, replace every `tinted(palette::ACCENT)` with `tinted(accent)` (the "Uploaded to …", "Live on …", and "Already on …" success lines), and every `link_actions(url)` call with `link_actions(url, accent)`.

Change `link_actions` to:

```rust
/// A share/watch link with Open + Copy-link buttons, in the destination's brand accent.
fn link_actions(url: &str, accent: iced::Color) -> Element<'static, Message> {
    row![
        text(url.to_owned()).size(12).font(UI_SEMIBOLD).style(tinted(accent)),
        button(text("Open").size(11).font(UI_SEMIBOLD))
            .on_press(Message::OpenLink(url.to_owned()))
            .style(secondary_button)
            .padding([6, 12]),
        button(text("Copy link").size(11).font(UI_SEMIBOLD))
            .on_press(Message::CopyLink(url.to_owned()))
            .style(secondary_button)
            .padding([6, 12]),
    ]
    .spacing(12)
    .align_y(iced::Alignment::Center)
    .into()
}
```

Leave the `TRIM` card's "Saved a trimmed clip." line on `palette::ACCENT` (trim is destination-agnostic).

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p rewynd-settings && cargo clippy -p rewynd-settings --all-targets -- -D warnings`
Expected: compiles clean. The `card` import may now be unused in `library.rs` — if clippy flags it, drop `card` from the `use crate::theme::{...}` list (keep `card_accent` is via `theme::`; confirm which names remain used).

- [ ] **Step 6: Commit**

```bash
git add crates/settings/src/library.rs
git commit -m "settings: recolour the whole upload panel to the destination brand"
```

---

### Task 5: Fade on switch + frame subscription (#110)

**Files:**
- Modify: `crates/settings/src/library.rs` — `Message`, `DestPicked`, `Open`, add `Tick`/`advance_fade`
- Modify: `crates/settings/src/main.rs` — `subscription`

**Interfaces:**
- Consumes: `AccentFade`, `current_accent`, `animating` (Task 3).
- Produces: `library::Message::Tick(std::time::Instant)`; `Library::animating()` gates a `iced::window::frames()` subscription in `main.rs`.

- [ ] **Step 1: Add the `Tick` message**

In the `Message` enum in `library.rs`, add:

```rust
    /// A frame tick while an accent fade is running (carries the frame instant).
    Tick(Instant),
```

- [ ] **Step 2: Start a fade on a real destination change**

Replace the `Message::DestPicked` arm:

```rust
            Message::DestPicked(dest) => {
                if self.dest != dest {
                    let from = self.current_accent();
                    self.dest = dest;
                    self.visibility = self.default_visibility(config);
                    self.accent_fade = Some(AccentFade {
                        from,
                        to: dest_accent(dest),
                        start: None,
                        progress: 0.0,
                    });
                }
            }
```

- [ ] **Step 3: Handle `Tick` and clear a finished fade**

Add an arm to `update` (near `DestPicked`):

```rust
            Message::Tick(now) => self.advance_fade(now),
```

Add the helper inside `impl Library` (near `current_accent`):

```rust
    /// Advance any running accent fade to frame time `now`, dropping it once complete so the
    /// frame subscription in `main` stops (no idle redraw).
    fn advance_fade(&mut self, now: Instant) {
        if let Some(fade) = &mut self.accent_fade
            && fade.advance(now)
        {
            self.accent_fade = None;
        }
    }
```

- [ ] **Step 4: Snap (don't fade) when opening a clip**

In the `Message::Open(path)` arm, add alongside the other resets (near `self.upload = UploadState::Idle;`):

```rust
                self.accent_fade = None;
```

- [ ] **Step 5: Gate a frame subscription on `animating()`**

In `crates/settings/src/main.rs`, replace `App::subscription` (around line 1044) with:

```rust
    fn subscription(&self) -> iced::Subscription<Message> {
        let focus = iced::event::listen_with(|event, _status, _id| match event {
            iced::Event::Window(iced::window::Event::Focused) => Some(Message::WindowFocused),
            _ => None,
        });
        let dir = config::clips_dir(self.config.output_dir().as_deref())
            .to_string_lossy()
            .into_owned();
        let clips = iced::Subscription::run_with(dir, |dir| {
            clip_watch_stream(std::path::PathBuf::from(dir))
        });
        let mut subs = vec![focus, clips];
        // Drive the accent fade only while one is running: iced re-diffs subscriptions after each
        // update, so when the fade clears this is dropped and the software renderer goes idle.
        if self.library.animating() {
            subs.push(
                iced::window::frames().map(|at| Message::Library(library::Message::Tick(at))),
            );
        }
        iced::Subscription::batch(subs)
    }
```

- [ ] **Step 6: Add the `advance_fade` regression test**

In `library.rs`'s `accent_tests` module, add:

```rust
    #[test]
    fn advance_fade_clears_when_complete() {
        // Drive the fade directly on a struct (no Library/Config/disk needed).
        let mut fade = AccentFade {
            from: (palette::ACCENT, palette::INK_ON_ACCENT),
            to: (palette::YOUTUBE, palette::INK_ON_YOUTUBE),
            start: None,
            progress: 0.0,
        };
        let t0 = Instant::now();
        assert!(!fade.advance(t0));
        assert!(fade.advance(t0 + ACCENT_FADE + Duration::from_millis(1)));
    }
```

- [ ] **Step 7: Build, test, clippy**

Run:
```bash
cargo test -p rewynd-settings accent_tests && \
cargo build -p rewynd-settings && \
cargo clippy -p rewynd-settings --all-targets -- -D warnings
```
Expected: tests PASS, compiles clean.

- [ ] **Step 8: Commit**

```bash
git add crates/settings/src/library.rs crates/settings/src/main.rs
git commit -m "settings: fade the upload accent when the destination switches"
```

---

### Task 6: Filmstrip layout math (pure logic, TDD)

**Files:**
- Modify: `crates/settings/src/library.rs` (add `filmstrip_positions`, `range_portions`, `FILMSTRIP_FRAMES`)
- Test: `crates/settings/src/library.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `const FILMSTRIP_FRAMES: usize`; `fn filmstrip_positions(n: usize) -> Vec<f32>` (centred, evenly spaced sample positions in `0.0..1.0`); `fn range_portions(start: f32, end: f32, dur: f32) -> (u16, u16, u16)` (FillPortion weights for the left scrim, kept band, right scrim).

- [ ] **Step 1: Write the failing tests**

Add a `#[cfg(test)]` module to `library.rs`:

```rust
#[cfg(test)]
mod filmstrip_tests {
    use super::*;

    #[test]
    fn positions_are_centred_and_ordered() {
        let p = filmstrip_positions(4);
        assert_eq!(p.len(), 4);
        assert!((p[0] - 0.125).abs() < 1e-6);
        assert!((p[3] - 0.875).abs() < 1e-6);
        assert!(p.windows(2).all(|w| w[0] < w[1]), "strictly increasing");
        assert!(p.iter().all(|&x| x > 0.0 && x < 1.0), "inside (0,1)");
        assert!(filmstrip_positions(0).is_empty());
    }

    #[test]
    fn portions_split_the_track_by_time() {
        // Whole clip kept: no scrim on either side.
        assert_eq!(range_portions(0.0, 10.0, 10.0), (0, 1000, 0));
        // A middle window.
        assert_eq!(range_portions(2.0, 9.0, 10.0), (200, 700, 100));
        // Empty band (start == end): both scrims, no kept middle.
        assert_eq!(range_portions(5.0, 5.0, 10.0), (500, 0, 500));
        // Degenerate duration stays drawable as "all kept".
        assert_eq!(range_portions(0.0, 0.0, 0.0), (0, 1000, 0));
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p rewynd-settings filmstrip_tests`
Expected: FAIL — functions not found.

- [ ] **Step 3: Implement the math**

Add near the other module constants in `library.rs` (e.g. below `MAX_DECODES`):

```rust
/// Keyframe thumbnails shown across the trim filmstrip.
const FILMSTRIP_FRAMES: usize = 12;
```

Add near the other free helpers:

```rust
/// `n` evenly spaced, centred sample positions in `0.0..1.0` (cell i samples its own midpoint),
/// so the strip skips the very first and last frames.
fn filmstrip_positions(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (i as f32 + 0.5) / n as f32)
        .collect()
}

/// FillPortion weights (left scrim, kept band, right scrim) for the `[start, end]` window over
/// `dur`, on a fixed 1000 scale. A zero weight renders as zero width (iced treats FillPortion(0)
/// as non-fluid). A non-positive `dur` degenerates to "all kept".
fn range_portions(start: f32, end: f32, dur: f32) -> (u16, u16, u16) {
    if dur <= 0.0 {
        return (0, 1000, 0);
    }
    let clamp = |v: f32| (v / dur).clamp(0.0, 1.0);
    let left = (clamp(start) * 1000.0).round() as u16;
    let right = ((1.0 - clamp(end)) * 1000.0).round() as u16;
    let mid = 1000u16.saturating_sub(left).saturating_sub(right);
    (left, mid, right)
}
```

- [ ] **Step 4: Run to confirm pass**

Run: `cargo test -p rewynd-settings filmstrip_tests`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/settings/src/library.rs
git commit -m "settings: filmstrip position + kept-range portion math (tested)"
```

---

### Task 7: Filmstrip decode pool + view (#51)

**Files:**
- Modify: `crates/settings/src/library.rs` — `Message`, `StripCell` state on `Library`, `Open`/`Back`/`show_grid`/`Deleted` reset, decode pool, `filmstrip` view, `trim_panel`
- Manual test (GPU/display box): drag the trim, watch the strip.

**Interfaces:**
- Consumes: `thumbs::load_at` (Task 2), `filmstrip_positions`/`range_portions`/`FILMSTRIP_FRAMES` (Task 6), `MAX_DECODES` (exists).
- Produces: `library::Message::StripDone(PathBuf, usize, Result<iced::widget::image::Handle, String>)`; a filmstrip element at the top of the TRIM card.

- [ ] **Step 1: Add strip state to `Library`**

Add the cell type near `enum Thumb` (top of file):

```rust
/// One filmstrip cell for the open clip: a decoded keyframe, still decoding, or undecodable.
enum StripCell {
    Loading,
    Ready(iced::widget::image::Handle),
    Failed,
}
```

Add fields to the `Library` struct (near `open: Option<PathBuf>,`):

```rust
    /// The clip the filmstrip cells belong to (guards late `StripDone` for a since-closed clip).
    strip_path: Option<PathBuf>,
    /// One slot per [`FILMSTRIP_FRAMES`] position for the open clip; empty when no clip is open.
    strip: Vec<StripCell>,
    /// Cells not yet decoding, drained into flight as slots free up: (cell index, position).
    strip_pending: VecDeque<(usize, f32)>,
    /// How many strip decodes are in flight (capped at [`MAX_DECODES`]).
    strip_decoding: usize,
```

Initialise in `Library::new` (near `open: None,`):

```rust
            strip_path: None,
            strip: Vec::new(),
            strip_pending: VecDeque::new(),
            strip_decoding: 0,
```

- [ ] **Step 2: Add the `StripDone` message**

In `Message`, add:

```rust
    StripDone(PathBuf, usize, Result<iced::widget::image::Handle, String>),
```

- [ ] **Step 3: A clear + a start-decodes helper**

Add inside `impl Library` (near `start_pending_decodes`):

```rust
    /// Drop the filmstrip for whatever clip was open (free the decoded frames).
    fn clear_strip(&mut self) {
        self.strip_path = None;
        self.strip.clear();
        self.strip_pending.clear();
        self.strip_decoding = 0;
    }

    /// Queue all filmstrip cells for `path` and kick off the first decodes.
    fn build_strip(&mut self, path: PathBuf) -> Task<Message> {
        self.strip_path = Some(path);
        self.strip = (0..FILMSTRIP_FRAMES).map(|_| StripCell::Loading).collect();
        self.strip_pending = filmstrip_positions(FILMSTRIP_FRAMES)
            .into_iter()
            .enumerate()
            .collect();
        self.strip_decoding = 0;
        self.start_strip_decodes()
    }

    /// Start queued strip decodes until [`MAX_DECODES`] are in flight; each is one blocking
    /// `thumbs::load_at`. [`Message::StripDone`] frees a slot and returns here for the next.
    fn start_strip_decodes(&mut self) -> Task<Message> {
        let Some(path) = self.strip_path.clone() else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        while self.strip_decoding < MAX_DECODES {
            let Some((index, position)) = self.strip_pending.pop_front() else {
                break;
            };
            self.strip_decoding += 1;
            let path = path.clone();
            tasks.push(Task::perform(
                async move {
                    let result = tokio::task::spawn_blocking({
                        let path = path.clone();
                        move || thumbs::load_at(&path, position, FILMSTRIP_CELL_WIDTH)
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                    (path, index, result)
                },
                |(path, index, result)| Message::StripDone(path, index, result),
            ));
        }
        Task::batch(tasks)
    }
```

Add the cell-width constant near `FILMSTRIP_FRAMES`:

```rust
/// Decoded width of one filmstrip cell (~2x its ~60px logical size for hidpi sharpness).
const FILMSTRIP_CELL_WIDTH: u32 = 120;
```

- [ ] **Step 4: Handle `StripDone`**

Add an arm to `update`:

```rust
            Message::StripDone(path, index, result) => {
                if self.strip_path.as_deref() == Some(path.as_path()) {
                    self.strip_decoding = self.strip_decoding.saturating_sub(1);
                    if let Some(cell) = self.strip.get_mut(index) {
                        *cell = match result {
                            Ok(handle) => StripCell::Ready(handle),
                            Err(e) => {
                                tracing::warn!(path = %path.display(), index, error = %e, "no filmstrip frame");
                                StripCell::Failed
                            }
                        };
                    }
                    return self.start_strip_decodes();
                }
            }
```

- [ ] **Step 5: Build the strip on open, clear it on leave**

In `Message::Open(path)`, after `self.open = Some(path);` add (build for the same path):

```rust
                let strip_task = self.build_strip(path.clone());
```

and change that arm to return `strip_task` instead of `Task::none()`. (The arm currently falls through to the shared `Task::none()` at the end of `update`; make it `return strip_task;` as its last line.)

Add `self.clear_strip();` to the `Message::Back` arm, the `show_grid` method, and the `Message::Deleted(Ok(path))` arm (when the deleted clip was open).

- [ ] **Step 6: Add the `filmstrip` view**

Add inside `impl Library` (near `trim_panel`):

```rust
    /// A row of keyframe thumbnails across the whole clip, the kept `[start, end]` band bright
    /// with a mint edge and the rest scrimmed. Driven by the trim sliders; purely visual.
    fn filmstrip(&self) -> Element<'_, Message> {
        if self.strip.is_empty() {
            return Space::new().into();
        }
        let mut cells = row![].width(Length::Fill);
        for cell in &self.strip {
            let content: Element<Message> = match cell {
                StripCell::Ready(handle) => iced::widget::image(handle.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(iced::ContentFit::Cover)
                    .into(),
                _ => container(Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(|_: &Theme| container::Style {
                        background: Some(Background::Color(palette::HIGH)),
                        ..container::Style::default()
                    })
                    .into(),
            };
            cells = cells.push(container(content).width(Length::FillPortion(1)));
        }

        let (left, mid, right) = range_portions(self.trim_start, self.trim_end, self.open_dur);
        let scrim = |portion: u16| -> Element<Message> {
            container(Space::new())
                .width(Length::FillPortion(portion))
                .height(Length::Fill)
                .style(|_: &Theme| container::Style {
                    background: Some(Background::Color(iced::Color {
                        a: 0.62,
                        ..palette::BACKGROUND
                    })),
                    ..container::Style::default()
                })
                .into()
        };
        let kept = container(Space::new())
            .width(Length::FillPortion(mid))
            .height(Length::Fill)
            .style(|_: &Theme| container::Style {
                border: Border {
                    color: palette::ACCENT,
                    width: 2.0,
                    radius: 4.0.into(),
                },
                ..container::Style::default()
            });
        let overlay = row![scrim(left), kept, scrim(right)].width(Length::Fill);

        container(iced::widget::stack![cells, overlay])
            .width(Length::Fill)
            .height(Length::Fixed(64.0))
            .style(theme::card_style)
            .into()
    }
```

- [ ] **Step 7: Put the strip at the top of the TRIM card**

In `trim_panel`, the panel is built as `let panel = column![ setting("Start", ...), ... ]`. Prepend the filmstrip: change the first line of that `column!` so the strip is the first child, e.g.:

```rust
        let panel = column![
            self.filmstrip(),
            setting(
                "Start",
                secs_label(start),
                slider(0.0..=dur, start, Message::TrimStartChanged)
                    .step(0.1f32)
                    .style(theme::arena_slider),
            ),
            // ... rest unchanged ...
```

- [ ] **Step 8: Import `stack` if needed, build, clippy**

`stack!` is used via `iced::widget::stack!`. Confirm the `use iced::widget::{...}` list — the code above fully-qualifies `iced::widget::image` and `iced::widget::stack!`, so no import edit is required, but if clippy prefers, add `stack` to the import list.

Run:
```bash
cargo build -p rewynd-settings && \
cargo clippy -p rewynd-settings --all-targets -- -D warnings
```
Expected: compiles clean.

- [ ] **Step 9: Commit**

```bash
git add crates/settings/src/library.rs
git commit -m "settings: live filmstrip preview over the trim range"
```

---

### Task 8: Workspace validation + review

**Files:** none (validation only).

- [ ] **Step 1: Full workspace gates**

Run:
```bash
cargo fmt --all --check && \
cargo build --workspace && \
cargo clippy --workspace --all-targets -- -D warnings && \
cargo test --workspace
```
Expected: all green. (GPU/Vulkan tests stay `#[ignore]`d; that is fine in CI.)

- [ ] **Step 2: Manual check on the display box**

Launch the settings app (`cargo run -p rewynd-settings`), open a clip:
- The TRIM card shows a filmstrip; dragging Start/End moves the bright band and dims the rest; the band matches what the saved trim keeps.
- In the UPLOAD card, switching ganked.tv ↔ YouTube fades the pill, "Upload to …" button, "UPLOAD" eyebrow, the "Uploaded/Already on …" line and the share link mint ↔ red over ~180 ms, then settles. No flicker or continuous repaint when idle (the window is still after the fade).

- [ ] **Step 3: Run the `/review` skill and apply findings**

Per CLAUDE.md: run the `/review` skill over the branch diff before pushing; apply its findings, then re-run the Step 1 gates.

- [ ] **Step 4: Push + open the PR**

```bash
git push -u origin 51-trim-preview-and-brand-accent
```
Open a PR whose body notes it closes #51, #110, #111, then handle the CodeRabbit review per CLAUDE.md.

---

## Self-Review

**Spec coverage:**
- #111 (whole panel follows brand) → Task 1 (`card_accent`) + Task 4 (pill, button, eyebrow, status, link). ✓
- #110 (fade) → Task 3 (model) + Task 5 (trigger + subscription). ✓
- #51 (live preview) → Task 2 (`load_at`) + Task 6 (math) + Task 7 (pool + view). ✓
- Crop deferred → out of scope in the spec; no task. ✓
- Field labels / TRIM "Saved" line stay mint → Task 4 Step 4 explicitly leaves them. ✓
- No idle redraw → Task 5 Step 5 gates `window::frames()` on `animating()`. ✓

**Placeholder scan:** every code step carries full code; test steps carry real assertions; no TBD/"handle edge cases". ✓

**Type consistency:** `current_accent -> (Color, Color)`, `AccentFade::{accent, advance}`, `Message::Tick(Instant)`, `Message::StripDone(PathBuf, usize, Result<Handle, String>)`, `load_at(path, position, width)`, `range_portions(start, end, dur) -> (u16, u16, u16)`, `filmstrip_positions(n) -> Vec<f32>`, `card_accent(title, accent, content)` — used identically across tasks. ✓

**Risks called out for the implementer:**
- If clippy flags `card` as an unused import in `library.rs` after Task 4, drop it from the `use crate::theme::{...}` list.
- `iced::widget::stack!` and `iced::widget::image` are fully qualified in Task 7; add to the import list only if clippy prefers.
