//! In-game save feedback for Linux/Wayland: the counterpart of the Windows `overlay` module.
//!
//! On a save we show a small badge on the wlr-layer-shell **overlay** layer — which the compositor
//! draws above normal *and* fullscreen windows, so it lands over a game the way the Windows GDI
//! badge does (and, on Wayland, even over "exclusive" fullscreen, since games still go through the
//! compositor). The badge is best-effort: [`show`] returns an error on a compositor without
//! layer-shell (GNOME/Mutter), so the caller can fall back to a desktop notification.
//!
//! The chime is played through rodio rather than the notification server: KDE mutes notification
//! sound under its fullscreen Do-Not-Disturb, which is exactly when a clip is most likely saved.

use std::num::NonZero;
use std::time::{Duration, Instant};

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use anyhow::{Context, Result, anyhow};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};
use tiny_skia::{Color, FillRule, Mask, Paint, PathBuilder, Pixmap, Rect, Transform};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

/// How long the badge stays on screen: long enough to catch in the corner of the eye mid-fight,
/// short enough to never read as clutter. Matches the Windows badge.
const BADGE_MS: Duration = Duration::from_millis(4000);
/// Whole-badge opacity: solid enough to read, soft enough to feel like an overlay, not a dialog.
const BADGE_ALPHA: u8 = 242;
/// Gap from the top-right corner of the screen (logical px).
const MARGIN: i32 = 28;
/// Badge box metrics (logical px).
const HEIGHT: u32 = 68;
const PAD: f32 = 18.0;
const LOGO: f32 = 32.0;
const GAP: f32 = 14.0;
const RADIUS: f32 = 10.0;
const TEXT_PX: f32 = 17.0;

/// The clip-saved chime, shared with the Windows path (`overlay::play_chime`).
static CHIME: &[u8] = crate::CLIP_SAVED_WAV;
/// The badge's label face: Inter SemiBold, embedded so there is no system-font scan.
static FONT: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.ttf");

/// The one visual bit of outcome signal (mirrors the Windows badge accent).
#[derive(Clone, Copy)]
pub enum Accent {
    Success,
    Failure,
}

impl Accent {
    /// The accent bar colour: mint on success, red on failure.
    fn rgb(self) -> (u8, u8, u8) {
        match self {
            Accent::Success => (0x2E, 0xCC, 0x71),
            Accent::Failure => (0xE7, 0x4C, 0x3C),
        }
    }
}

/// Play the save sound off-thread: the chime on success, a short error tone on failure — the
/// audible half of the feedback, and the half that still lands over a fullscreen game.
pub fn play(accent: Accent) {
    let _ = std::thread::Builder::new()
        .name("rewynd-chime".to_owned())
        .spawn(move || play_blocking(accent));
}

fn play_blocking(accent: Accent) {
    let (samples, channels, rate) = match accent {
        Accent::Success => match decode_wav(CHIME) {
            Some(decoded) => decoded,
            None => return,
        },
        Accent::Failure => (error_tone(), 1, 44_100),
    };
    // The sink owns the device stream and must outlive playback; no sound server just means the
    // save confirms silently (the badge still shows).
    let Ok(mut sink) = rodio::DeviceSinkBuilder::open_default_sink() else {
        return;
    };
    sink.log_on_drop(false);
    let player = rodio::Player::connect_new(sink.mixer());
    let (Some(channels_nz), Some(rate_nz)) = (NonZero::new(channels), NonZero::new(rate)) else {
        return;
    };
    let frames = samples.len() as f64 / f64::from(channels);
    player.append(rodio::buffer::SamplesBuffer::new(
        channels_nz,
        rate_nz,
        samples,
    ));
    // Let the queued samples drain before the sink drops (which would cut the tail).
    std::thread::sleep(
        Duration::from_secs_f64(frames / f64::from(rate)) + Duration::from_millis(120),
    );
}

/// Decode a 16-bit PCM WAV (the embedded chime) to `(interleaved f32 samples, channels, sample
/// rate)`. Enough for our own asset; not a general parser. `None` if the layout is unexpected.
fn decode_wav(bytes: &[u8]) -> Option<(Vec<f32>, u16, u32)> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut fmt = None;
    let mut samples = None;
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().ok()?) as usize;
        let body = i + 8;
        if body + size > bytes.len() {
            break;
        }
        if id == b"fmt " && size >= 16 {
            let channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().ok()?);
            let rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?);
            fmt = Some((channels, rate));
        } else if id == b"data" {
            samples = Some(
                bytes[body..body + size]
                    .chunks_exact(2)
                    .map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32_768.0)
                    .collect::<Vec<f32>>(),
            );
        }
        // Chunks are word-aligned: an odd size carries a pad byte.
        i = body + size + (size & 1);
    }
    let ((channels, rate), samples) = (fmt?, samples?);
    (channels > 0).then_some((samples, channels, rate))
}

/// A short descending two-tone beep for the failure case (the Windows path uses the system error
/// beep here; we synthesise one so it plays without a sound theme).
fn error_tone() -> Vec<f32> {
    let rate = 44_100.0;
    let mut out = Vec::new();
    for (freq, ms) in [(620.0_f32, 110.0_f32), (440.0, 150.0)] {
        let n = (rate * ms / 1000.0) as usize;
        for k in 0..n {
            let t = k as f32 / rate;
            // A short raised-cosine envelope so the tone doesn't click on/off.
            let env = (std::f32::consts::PI * k as f32 / n as f32).sin();
            out.push(0.28 * env * (2.0 * std::f32::consts::PI * freq * t).sin());
        }
    }
    out
}

/// Show the in-game badge. Renders the pixels up front (so a font/asset problem fails before we
/// touch Wayland), completes the layer-shell configure/draw handshake, then hands the live
/// connection to a short-lived thread that keeps the badge on screen for [`BADGE_MS`]. Returns
/// `Err` — so the caller can fall back to a desktop notification — when the compositor has no
/// layer-shell or the surface never draws.
pub fn show(accent: Accent, text: &str) -> Result<()> {
    let badge = render(accent, text).context("render badge")?;

    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init::<State>(&conn).context("init registry")?;
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| anyhow!("compositor: {e}"))?;
    let layer_shell =
        LayerShell::bind(&globals, &qh).map_err(|e| anyhow!("no wlr-layer-shell: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow!("shm: {e}"))?;

    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("rewynd-badge"), None);
    layer.set_anchor(Anchor::TOP | Anchor::RIGHT);
    layer.set_margin(MARGIN, MARGIN, 0, 0);
    layer.set_size(badge.width, badge.height);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();

    let pool = SlotPool::new((badge.width * badge.height * 4) as usize, &shm)
        .context("create shm pool")?;
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        badge,
        drawn: false,
    };

    // Complete the configure/draw handshake before reporting success, so a caller only falls back
    // to a notification when the badge truly didn't appear.
    let setup_deadline = Instant::now() + Duration::from_millis(500);
    while !state.drawn && Instant::now() < setup_deadline {
        event_queue
            .roundtrip(&mut state)
            .context("wayland roundtrip")?;
    }
    if !state.drawn {
        return Err(anyhow!("badge surface did not draw"));
    }

    // Keep the connection alive — the compositor holds the committed buffer on screen — until the
    // deadline, then let everything drop, which destroys the surface.
    std::thread::Builder::new()
        .name("rewynd-badge".to_owned())
        .spawn(move || {
            let deadline = Instant::now() + BADGE_MS;
            while Instant::now() < deadline {
                if event_queue.roundtrip(&mut state).is_err() {
                    break;
                }
                conn.flush().ok();
                std::thread::sleep(Duration::from_millis(60));
            }
        })
        .context("spawn badge thread")?;
    Ok(())
}

/// The rendered badge: premultiplied ARGB (in `wl_shm` byte order) plus its size.
struct Badge {
    argb: Vec<u8>,
    width: u32,
    height: u32,
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    badge: Badge,
    drawn: bool,
}

impl State {
    fn draw(&mut self) {
        let (w, h) = (self.badge.width, self.badge.height);
        let stride = (w * 4) as i32;
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
        else {
            return;
        };
        canvas[..self.badge.argb.len()].copy_from_slice(&self.badge.argb);
        let surface = self.layer.wl_surface();
        if buffer.attach_to(surface).is_ok() {
            surface.damage_buffer(0, 0, w as i32, h as i32);
            surface.commit();
            self.drawn = true;
        }
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wayland_client::protocol::wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {}

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if !self.drawn {
            self.draw();
        }
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_layer!(State);
delegate_shm!(State);
delegate_registry!(State);

/// Draw the badge to premultiplied ARGB (`wl_shm` byte order): a rounded panel with a coloured
/// accent bar, the brand mark, and the label.
fn render(accent: Accent, text: &str) -> Result<Badge> {
    let font = FontRef::try_from_slice(FONT).context("load badge font")?;
    let scale = PxScale::from(TEXT_PX);
    let scaled = font.as_scaled(scale);

    // Lay the label out first so the badge can size to it.
    let mut pen = 0.0_f32;
    let mut glyphs = Vec::new();
    let mut prev = None;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(p) = prev {
            pen += scaled.kern(p, id);
        }
        glyphs.push((id, pen));
        pen += scaled.h_advance(id);
        prev = Some(id);
    }
    let text_w = pen.ceil();

    let text_left = PAD + LOGO + GAP;
    let width = (text_left + text_w + PAD).ceil() as u32;
    let height = HEIGHT;

    let mut pixmap = Pixmap::new(width, height).context("alloc pixmap")?;

    // Panel: rounded rect, dark, translucent.
    let panel =
        rounded_rect(0.0, 0.0, width as f32, height as f32, RADIUS).context("panel path")?;
    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };
    paint.set_color(Color::from_rgba8(0x18, 0x1C, 0x22, BADGE_ALPHA));
    pixmap.fill_path(
        &panel,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // Accent bar down the left edge.
    let (r, g, b) = accent.rgb();
    let bar = rounded_rect(0.0, 0.0, 5.0, height as f32, RADIUS).context("accent bar path")?;
    paint.set_color(Color::from_rgba8(r, g, b, 0xFF));
    pixmap.fill_path(&bar, &paint, FillRule::Winding, Transform::identity(), None);

    // Brand mark, vertically centred (best-effort — skipped if it can't be decoded).
    if let Some(logo) = logo_pixmap(LOGO as u32) {
        let y = (height as f32 - LOGO) / 2.0;
        pixmap.draw_pixmap(
            (PAD + 2.0) as i32,
            y as i32,
            logo.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }

    // Label, as a coverage mask filled with near-white.
    let baseline = (height as f32 - (scaled.ascent() + scaled.descent())) / 2.0 + scaled.ascent();
    if let Some(mask) = text_mask(&font, &glyphs, scale, text_left, baseline, width, height) {
        let mut text_paint = Paint::default();
        text_paint.set_color(Color::from_rgba8(0xF2, 0xF5, 0xF7, 0xFF));
        if let Some(rect) = Rect::from_xywh(0.0, 0.0, width as f32, height as f32) {
            pixmap.fill_rect(rect, &text_paint, Transform::identity(), Some(&mask));
        }
    }

    Ok(Badge {
        argb: to_argb8888(pixmap.data(), width, height),
        width,
        height,
    })
}

/// Rasterise the laid-out glyphs into a coverage [`Mask`] the fill is clipped through.
fn text_mask(
    font: &FontRef,
    glyphs: &[(ab_glyph::GlyphId, f32)],
    scale: PxScale,
    left: f32,
    baseline: f32,
    width: u32,
    height: u32,
) -> Option<Mask> {
    let mut mask = Mask::new(width, height)?;
    let data = mask.data_mut();
    for &(id, x) in glyphs {
        let glyph = id.with_scale_and_position(scale, point(left + x, baseline));
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outline.px_bounds();
        outline.draw(|gx, gy, coverage| {
            let px = bounds.min.x as i32 + gx as i32;
            let py = bounds.min.y as i32 + gy as i32;
            if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                return;
            }
            let idx = py as usize * width as usize + px as usize;
            let v = (coverage * 255.0) as u8;
            data[idx] = data[idx].max(v);
        });
    }
    Some(mask)
}

/// Decode the brand mark nearest `size` into a premultiplied tiny-skia [`Pixmap`].
fn logo_pixmap(size: u32) -> Option<Pixmap> {
    let bytes = rewynd_config::BRAND_ICONS
        .iter()
        .find(|(s, _)| *s >= size)
        .or(rewynd_config::BRAND_ICONS.last())
        .map(|(_, b)| *b)?;
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .ok()?
        .into_rgba8();
    let (w, h) = (img.width(), img.height());
    let mut pixmap = Pixmap::new(w, h)?;
    for (dst, src) in pixmap.pixels_mut().iter_mut().zip(img.pixels()) {
        let [r, g, b, a] = src.0;
        *dst = tiny_skia::PremultipliedColorU8::from_rgba(mul(r, a), mul(g, a), mul(b, a), a)?;
    }
    // Scale to the requested size via a resample only when needed.
    if w == size && h == size {
        return Some(pixmap);
    }
    let mut out = Pixmap::new(size, size)?;
    out.draw_pixmap(
        0,
        0,
        pixmap.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        Transform::from_scale(size as f32 / w as f32, size as f32 / h as f32),
        None,
    );
    Some(out)
}

fn mul(c: u8, a: u8) -> u8 {
    ((c as u16 * a as u16 + 127) / 255) as u8
}

/// A rounded-rectangle path. `None` for a degenerate (zero-area) rectangle.
fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

/// tiny-skia stores premultiplied `RGBA`; `wl_shm` `Argb8888` wants premultiplied `BGRA` bytes.
fn to_argb8888(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0u8; (width * height * 4) as usize];
    for (dst, src) in out.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
        dst[0] = src[2]; // B
        dst[1] = src[1]; // G
        dst[2] = src[0]; // R
        dst[3] = src[3]; // A
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_embedded_chime() {
        let (samples, channels, rate) = decode_wav(CHIME).expect("chime is a valid 16-bit WAV");
        assert!(channels >= 1, "at least one channel, got {channels}");
        assert!(rate >= 8_000, "plausible sample rate, got {rate}");
        assert!(!samples.is_empty(), "chime has samples");
        assert_eq!(
            samples.len() % channels as usize,
            0,
            "whole interleaved frames"
        );
        assert!(
            samples.iter().all(|s| (-1.0..=1.0).contains(s)),
            "samples are normalised to [-1, 1]"
        );
    }

    #[test]
    fn rejects_non_wav() {
        assert!(decode_wav(b"not a wav file at all........").is_none());
        assert!(decode_wav(&[]).is_none());
    }

    #[test]
    fn argb8888_swaps_red_and_blue() {
        // One opaque pixel, tiny-skia order R,G,B,A -> wl_shm Argb8888 bytes B,G,R,A.
        let rgba = [0x11, 0x22, 0x33, 0xFF];
        assert_eq!(to_argb8888(&rgba, 1, 1), vec![0x33, 0x22, 0x11, 0xFF]);
    }
}
