//! Measuring the captured monitor on Linux/Wayland, so the encode resolution can follow it.
//!
//! Wayland has no "what resolution is my screen" call outside the compositor's own protocols, and
//! the ScreenCast portal only reports the stream's size in *compositor* (logical) space — which is
//! the right shape but the wrong number under fractional scaling. So this asks the compositor
//! directly: bind `wl_output`, find the output whose logical rectangle contains the portal's
//! reported origin (the same match the in-game badge makes), and take that output's current mode,
//! which is its true pixel resolution.
//!
//! Everything here is best-effort. A compositor that hides output geometry, a portal that reported
//! no position, a stream that isn't a monitor at all — each degrades to the next fallback and
//! finally to `None`, which leaves the recorder on its configured size.

use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_output, delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_output;
use wayland_client::{Connection, QueueHandle};

/// The captured monitor's size in pixels.
///
/// `origin` and `logical_size` come from the ScreenCast portal's stream properties. The origin
/// identifies *which* monitor on a multi-head setup; the logical size is the fallback when the
/// compositor won't say more (it still carries the right aspect ratio, which is most of the point).
#[must_use]
pub fn capture_geometry(
    origin: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
) -> Option<(u32, u32)> {
    let from_output = output_pixels(origin);
    if from_output.is_none() {
        tracing::debug!(
            ?origin,
            ?logical_size,
            "no matching wayland output; falling back to the portal's stream size"
        );
    }
    from_output.or_else(|| {
        let (w, h) = logical_size?;
        Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))
    })
}

/// The pixel size of the output containing `origin`, from its current mode.
fn output_pixels(origin: Option<(i32, i32)>) -> Option<(u32, u32)> {
    let (ox, oy) = origin?;
    let conn = Connection::connect_to_env().ok()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
    let qh = queue.handle();
    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
    };
    // Output geometry (including the xdg logical rects) only arrives after a roundtrip.
    queue.roundtrip(&mut state).ok()?;

    for output in state.output_state.outputs() {
        let Some(info) = state.output_state.info(&output) else {
            continue;
        };
        let (Some((x, y)), Some((lw, lh))) = (info.logical_position, info.logical_size) else {
            continue;
        };
        if ox < x || ox >= x + lw || oy < y || oy >= y + lh {
            continue;
        }
        let scale = info.scale_factor.max(1);
        // No mode advertised: the integer scale over the logical size is the best guess left.
        let (mw, mh) = match info.modes.iter().find(|m| m.current) {
            // A rotated output's mode is the panel's, in panel orientation; the stream arrives
            // in the rotated orientation the logical rect describes.
            Some(mode) if rotates_axes(info.transform) => (mode.dimensions.1, mode.dimensions.0),
            Some(mode) => mode.dimensions,
            None => (lw * scale, lh * scale),
        };
        let (Ok(w), Ok(h)) = (u32::try_from(mw), u32::try_from(mh)) else {
            continue;
        };
        if w == 0 || h == 0 {
            continue;
        }
        tracing::info!(
            output = info.name.as_deref().unwrap_or("?"),
            width = w,
            height = h,
            scale,
            "measured the captured monitor"
        );
        return Some((w, h));
    }
    None
}

/// Whether the output transform swaps width and height (a 90° or 270° rotation).
fn rotates_axes(transform: wl_output::Transform) -> bool {
    matches!(
        transform,
        wl_output::Transform::_90
            | wl_output::Transform::_270
            | wl_output::Transform::Flipped90
            | wl_output::Transform::Flipped270
    )
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_output!(State);
delegate_registry!(State);
