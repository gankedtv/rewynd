//! WGC stream setup + shareable D3D11 textures (PLAN §3.5, §6.1).
//!
//! `windows-capture` drives the Windows Graphics Capture session (monitor item,
//! frame pool, dispatcher thread). WGC's own pool textures are recycled and not
//! shareable, so each arriving frame is copied into a slot from a small
//! round-robin pool of textures created with
//! `D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE`. The copy is
//! flushed and waited on (D3D11 event query) so the content is complete before any
//! other device reads it — there is no implicit cross-API sync on the handle — and
//! the slot's NT handle is then duplicated into an [`OwnedHandle`] the callback
//! owns. That handle is what `wgpu-hal`'s `texture_from_d3d11_shared_handle`
//! (`VULKAN_EXTERNAL_MEMORY_WIN32`, `VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_BIT`)
//! imports; the Vulkan side references the resource, so the callback's handle can be
//! closed after import.
//!
//! Mirrors the shape of the Linux backend ([`crate::linux::capture_stream`]): a
//! blocking call driving a per-frame callback with a cooperative stop flag. One
//! difference is forced by WGC: frames are delivered on a capture thread the API
//! owns, so the callback must be `Send` (the Linux callback runs on the calling
//! thread and need not be).

use std::ops::ControlFlow;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIResource1,
};
use windows::core::{BOOL, Interface};
use windows_capture::capture::{
    CaptureControlError, Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::{CaptureError, StreamPrefs};

/// How many shareable slot textures the copies rotate through. Deep enough that a
/// consumer importing/encoding a frame is done before the slot is overwritten
/// (WGC's own frame pool has a similar depth), without holding excess VRAM.
const SHARED_SLOTS: usize = 4;

/// How often the watchdog polls the cooperative stop flag. WGC delivers no frames
/// while the screen is static, so the flag must be observed off the frame path too.
const STOP_POLL: Duration = Duration::from_millis(200);

/// How often the game detector re-checks the foreground window while no game is
/// running (or after a session ended). Cheap Win32 queries; sub-second pickup.
const GAME_POLL: Duration = Duration::from_millis(500);

/// Consecutive unexpected session failures tolerated before the game loop gives
/// up — enough to absorb a window closing mid-setup, small enough that a broken
/// capture backend surfaces instead of spinning silently.
const SESSION_FAILURE_BUDGET: u32 = 3;

/// Bound on the CPU-side wait for a slot copy to complete. A healthy copy finishes
/// in well under a millisecond; hitting this means the device is lost or hung.
const COPY_WAIT_TIMEOUT: Duration = Duration::from_secs(1);

const STREAM_END_ERROR: &str =
    "capture ended without a stop request (monitor disconnected or the session failed)";

/// A single captured frame as a shareable D3D11 texture's NT handle.
///
/// The handle is an owned duplicate (closes on drop) referring to one of the
/// backend's slot textures. Its *content* is only guaranteed until the slot is
/// reused, [`SHARED_SLOTS`] − 1 frames later — import it (or copy it out) before
/// returning from the callback chain that far behind. The Vulkan import
/// (`texture_from_d3d11_shared_handle`) takes its own reference to the resource,
/// so this handle can be dropped right after a successful import.
#[derive(Debug)]
pub struct CapturedD3d11Frame {
    /// Owned NT shared handle to the slot texture (duplicated per frame).
    pub handle: OwnedHandle,
    pub width: u32,
    pub height: u32,
    pub dxgi_format: DXGI_FORMAT,
    /// Monotonic capture time relative to the stream epoch. Inter-frame deltas
    /// reflect the real (damage-driven, variable) delivery cadence, so downstream
    /// PTS stays wall-clock-accurate.
    pub pts: Duration,
}

impl CapturedD3d11Frame {
    /// The `wgpu` texture format to import this frame as, or `None` if the DXGI
    /// format isn't a supported packed 32-bit RGB layout.
    #[must_use]
    pub fn texture_format(&self) -> Option<wgpu::TextureFormat> {
        match self.dxgi_format {
            DXGI_FORMAT_B8G8R8A8_UNORM => Some(wgpu::TextureFormat::Bgra8Unorm),
            DXGI_FORMAT_R8G8B8A8_UNORM => Some(wgpu::TextureFormat::Rgba8Unorm),
            _ => None,
        }
    }
}

/// One shareable texture + the NT handle created for it. The handle is duplicated
/// (never given away) so the slot can hand out an owned copy per frame.
struct SharedSlot {
    texture: ID3D11Texture2D,
    handle: OwnedHandle,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
}

impl SharedSlot {
    /// Create a shareable texture matching the captured frame plus its NT handle.
    /// `SHARED | SHARED_NTHANDLE` (no keyed mutex): the Vulkan import path doesn't
    /// speak keyed mutexes, so cross-device ordering is done with the event-query
    /// wait after each copy instead.
    fn create(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<Self, CaptureError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                as u32,
        };

        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: FFI.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|e| CaptureError::D3d11(format!("create shareable texture: {e}")))?;
        let texture = texture
            .ok_or_else(|| CaptureError::D3d11("CreateTexture2D returned no texture".to_owned()))?;

        let resource: IDXGIResource1 = texture
            .cast()
            .map_err(|e| CaptureError::D3d11(format!("query IDXGIResource1: {e}")))?;
        // SAFETY: FFI; anonymous handle, default security.
        let handle = unsafe {
            resource.CreateSharedHandle(
                None,
                (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0,
                None,
            )
        }
        .map_err(|e| CaptureError::D3d11(format!("CreateSharedHandle: {e}")))?;
        // SAFETY: `CreateSharedHandle` returned a valid NT handle we now own.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };

        Ok(Self {
            texture,
            handle,
            width,
            height,
            format,
        })
    }

    fn matches(&self, width: u32, height: u32, format: DXGI_FORMAT) -> bool {
        self.width == width && self.height == height && self.format == format
    }
}

/// Everything the WGC handler needs, passed through `Settings` flags.
struct HandlerFlags<F> {
    on_frame: F,
    epoch: Instant,
    stop: Option<Arc<AtomicBool>>,
    /// Set when the callback breaks: a deliberate, successful end.
    success: Arc<AtomicBool>,
    /// Set when the stop flag was observed: a clean cooperative stop.
    stopped: Arc<AtomicBool>,
}

/// The `windows-capture` handler: copies each frame into a shareable slot and
/// drives the per-frame callback.
struct Handler<F> {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// One event query, reused for every per-frame copy-completion wait.
    query: ID3D11Query,
    slots: Vec<SharedSlot>,
    cursor: usize,
    frames: u64,
    flags: HandlerFlags<F>,
}

impl<F> Handler<F> {
    /// Block until previously issued GPU work (the slot copy) completes, so the
    /// shared texture is safe for another device to read. `GetData` writes the
    /// event's BOOL only once signalled; until then the call succeeds with
    /// `S_FALSE` and leaves it untouched.
    fn wait_for_copy(&self) -> Result<(), CaptureError> {
        // SAFETY: FFI; `query` was created on this device.
        unsafe {
            self.context.End(&self.query);
            self.context.Flush();
        }
        let deadline = Instant::now() + COPY_WAIT_TIMEOUT;
        loop {
            let mut done = BOOL(0);
            // SAFETY: FFI; `done` is a valid 4-byte target matching the event
            // query's data size.
            unsafe {
                self.context.GetData(
                    &self.query,
                    Some(std::ptr::from_mut(&mut done).cast()),
                    size_of::<BOOL>() as u32,
                    0,
                )
            }
            .map_err(|e| CaptureError::D3d11(format!("event query GetData: {e}")))?;
            if done.as_bool() {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(CaptureError::D3d11(
                    "timed out waiting for the slot copy (device lost?)".to_owned(),
                ));
            }
            std::thread::yield_now();
        }
    }

    /// The index of the slot for this frame, (re)created if absent or mismatching
    /// the frame's size/format (e.g. after a display-mode change).
    fn slot_for(
        &mut self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<usize, CaptureError> {
        let index = self.cursor % SHARED_SLOTS;
        self.cursor = self.cursor.wrapping_add(1);
        while self.slots.len() <= index {
            let len = self.slots.len();
            tracing::debug!(slot = len, width, height, "creating shareable slot texture");
            self.slots
                .push(SharedSlot::create(&self.device, width, height, format)?);
        }
        if !self.slots[index].matches(width, height, format) {
            tracing::info!(
                slot = index,
                width,
                height,
                "capture size/format changed; recreating slot"
            );
            self.slots[index] = SharedSlot::create(&self.device, width, height, format)?;
        }
        Ok(index)
    }
}

impl<F> GraphicsCaptureApiHandler for Handler<F>
where
    F: FnMut(CapturedD3d11Frame) -> ControlFlow<()> + Send + 'static,
{
    type Flags = HandlerFlags<F>;
    type Error = CaptureError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let query_desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut query: Option<ID3D11Query> = None;
        // SAFETY: FFI.
        unsafe { ctx.device.CreateQuery(&query_desc, Some(&mut query)) }
            .map_err(|e| CaptureError::D3d11(format!("create event query: {e}")))?;
        let query =
            query.ok_or_else(|| CaptureError::D3d11("CreateQuery returned no query".to_owned()))?;

        Ok(Self {
            device: ctx.device,
            context: ctx.device_context,
            query,
            slots: Vec::with_capacity(SHARED_SLOTS),
            cursor: 0,
            frames: 0,
            flags: ctx.flags,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame<'_>,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self
            .flags
            .stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Relaxed))
        {
            self.flags.stopped.store(true, Ordering::Relaxed);
            capture_control.stop();
            return Ok(());
        }

        let width = frame.width();
        let height = frame.height();
        let format = frame.desc().Format;
        let frame_texture = frame.as_raw_texture().clone();

        let slot = self.slot_for(width, height, format)?;
        // SAFETY: FFI; both textures live on this device and have identical
        // size/format (the slot was just matched against the frame's descriptor).
        unsafe {
            self.context
                .CopyResource(&self.slots[slot].texture, &frame_texture)
        };
        self.wait_for_copy()?;

        let handle = self.slots[slot]
            .handle
            .try_clone()
            .map_err(|e| CaptureError::D3d11(format!("duplicate shared NT handle: {e}")))?;

        if self.frames == 0 {
            tracing::info!(width, height, format = ?format, "first WGC frame delivered");
        }
        self.frames += 1;

        let captured = CapturedD3d11Frame {
            handle,
            width,
            height,
            dxgi_format: format,
            pts: self.flags.epoch.elapsed(),
        };
        if let ControlFlow::Break(()) = (self.flags.on_frame)(captured) {
            self.flags.success.store(true, Ordering::Relaxed);
            capture_control.stop();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        tracing::info!("capture item closed (monitor disconnected?)");
        Ok(())
    }
}

/// Unwrap the handler's own [`CaptureError`] out of `windows-capture`'s error
/// nesting; anything else becomes [`CaptureError::Wgc`].
fn map_api_error(e: GraphicsCaptureApiError<CaptureError>) -> CaptureError {
    match e {
        GraphicsCaptureApiError::NewHandlerError(e)
        | GraphicsCaptureApiError::FrameHandlerError(e) => e,
        other => CaptureError::Wgc(other.to_string()),
    }
}

fn map_control_error(e: CaptureControlError<CaptureError>) -> CaptureError {
    match e {
        CaptureControlError::GraphicsCaptureApiError(api) => map_api_error(api),
        CaptureControlError::StoppedHandlerError(e) => e,
        other => CaptureError::Wgc(other.to_string()),
    }
}

/// Capture a continuous stream of frames from a monitor, handing each to
/// `on_frame` as an owned NT shared handle ([`CapturedD3d11Frame`]).
///
/// `monitor_index` selects the monitor (one-based, per the Win32 display
/// enumeration); `None` captures the primary monitor. WGC captures at the
/// monitor's native size — `prefs.width`/`height` are advisory (the scale to the
/// configured size happens downstream), while `prefs.framerate` caps the delivery
/// rate via WGC's minimum update interval when it's below the monitor's refresh
/// rate.
///
/// `on_frame` returns [`ControlFlow::Continue`] to keep receiving frames or
/// [`ControlFlow::Break`] to stop the stream (a successful, deliberate end). It
/// runs on a capture thread owned by WGC, hence the `Send` bound.
///
/// Each frame's PTS is measured from `epoch`; pass the same epoch to the audio
/// capture so the two tracks share one clock and the muxer can align them.
///
/// Blocks until `on_frame` breaks, the `stop` flag is set (both return `Ok`), or
/// the session fails. The stop flag is polled off the frame path too, so an idle
/// (frameless) screen can still be stopped.
pub fn capture_stream<F>(
    monitor_index: Option<usize>,
    epoch: Instant,
    prefs: StreamPrefs,
    stop: Option<Arc<AtomicBool>>,
    on_frame: F,
) -> Result<(), CaptureError>
where
    F: FnMut(CapturedD3d11Frame) -> ControlFlow<()> + Send + 'static,
{
    let monitor = match monitor_index {
        Some(index) => Monitor::from_index(index),
        None => Monitor::primary(),
    }
    .map_err(|e| CaptureError::Wgc(format!("monitor selection: {e}")))?;
    tracing::info!(
        monitor = monitor.name().unwrap_or_default(),
        "starting WGC monitor capture"
    );
    let refresh = monitor.refresh_rate().unwrap_or(0);
    run_session(monitor, refresh, epoch, prefs, stop, on_frame)
}

/// Capture the active *game*, continuously: poll the foreground window until one
/// looks like a running game (fullscreen/borderless — see
/// [`super::game_window::fullscreen_game_window`]), capture it until it closes,
/// then go back to watching for the next one. Desktop content between games is
/// never captured.
///
/// Same callback/stop contract as [`capture_stream`]; a callback `Break` ends the
/// whole loop, not just the current game's session. Blocks until `on_frame` breaks
/// or the `stop` flag is set — while no game runs, it idles on the detector poll.
pub fn capture_game_stream<F>(
    epoch: Instant,
    prefs: StreamPrefs,
    stop: Option<Arc<AtomicBool>>,
    on_frame: F,
) -> Result<(), CaptureError>
where
    F: FnMut(CapturedD3d11Frame) -> ControlFlow<()> + Send + 'static,
{
    // The callback outlives any single game's session, so each session gets an
    // adapter borrowing it through a mutex (sessions never overlap; the lock is
    // uncontended). `finished` distinguishes the callback's own Break from a
    // session that ended because the game closed.
    let on_frame = Arc::new(Mutex::new(on_frame));
    let finished = Arc::new(AtomicBool::new(false));
    let mut failures: u32 = 0;
    tracing::info!("game-only capture: watching for a fullscreen game");

    loop {
        if stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Relaxed))
            || finished.load(Ordering::Relaxed)
        {
            return Ok(());
        }

        let Some(window) = super::game_window::fullscreen_game_window() else {
            std::thread::sleep(GAME_POLL);
            continue;
        };
        // Process name only — window titles carry documents/URLs/chat context, and
        // leaking those into logs would undercut the point of game-only capture.
        tracing::info!(
            process = window.process_name().unwrap_or_default(),
            "fullscreen game detected; capturing it"
        );
        let refresh = window
            .monitor()
            .and_then(|m| m.refresh_rate().ok())
            .unwrap_or(0);

        let session_cb = {
            let on_frame = on_frame.clone();
            let finished = finished.clone();
            move |frame: CapturedD3d11Frame| {
                let mut on_frame = on_frame.lock().unwrap_or_else(|p| p.into_inner());
                match on_frame(frame) {
                    ControlFlow::Break(()) => {
                        finished.store(true, Ordering::Relaxed);
                        ControlFlow::Break(())
                    }
                    action => action,
                }
            }
        };
        // The expected end — the game closed (its item died, surfacing as the
        // stream-end error) — returns to the detector. Anything else (setup or
        // device failures) gets a small retry budget for races like a window
        // closing mid-setup, then propagates so a broken backend can't spin
        // silently forever.
        match run_session(window, refresh, epoch, prefs, stop.clone(), session_cb) {
            Ok(()) => {
                failures = 0;
            }
            Err(CaptureError::Wgc(ref msg)) if msg == STREAM_END_ERROR => {
                failures = 0;
                tracing::info!("game capture session ended; watching for the next game");
                std::thread::sleep(GAME_POLL);
            }
            Err(e) => {
                failures += 1;
                if failures >= SESSION_FAILURE_BUDGET {
                    return Err(e);
                }
                tracing::warn!(error = %e, attempt = failures, "game capture session failed; retrying detection");
                std::thread::sleep(GAME_POLL);
            }
        }
    }
}

/// One WGC session over `item` (a monitor or a window): the shared body of
/// [`capture_stream`] and [`capture_game_stream`]. `refresh` is the source
/// monitor's refresh rate (0 = unknown), used to decide whether `prefs.framerate`
/// caps delivery via WGC's minimum update interval.
fn run_session<T, F>(
    item: T,
    refresh: u32,
    epoch: Instant,
    prefs: StreamPrefs,
    stop: Option<Arc<AtomicBool>>,
    on_frame: F,
) -> Result<(), CaptureError>
where
    T: TryInto<windows_capture::settings::GraphicsCaptureItemType> + Send + 'static,
    F: FnMut(CapturedD3d11Frame) -> ControlFlow<()> + Send + 'static,
{
    // Cap delivery at the configured framerate when it's below the source's
    // refresh rate; otherwise let WGC deliver at its native cadence.
    let min_interval = if prefs.framerate > 0 && (refresh == 0 || prefs.framerate < refresh) {
        MinimumUpdateIntervalSettings::Custom(Duration::from_secs(1) / prefs.framerate)
    } else {
        MinimumUpdateIntervalSettings::Default
    };
    tracing::info!(
        refresh,
        framerate = prefs.framerate,
        capped = matches!(min_interval, MinimumUpdateIntervalSettings::Custom(_)),
        "starting WGC session"
    );

    let success = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));

    let settings = Settings::new(
        item,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        min_interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        HandlerFlags {
            on_frame,
            epoch,
            stop: stop.clone(),
            success: success.clone(),
            stopped: stopped.clone(),
        },
    );

    let control = Handler::start_free_threaded(settings).map_err(map_api_error)?;

    // Cooperative-stop watchdog (the Linux backend's timer equivalent): WGC only
    // calls the handler when a frame arrives, so a static screen would otherwise
    // never observe the flag.
    let result = loop {
        if control.is_finished() {
            break control.wait();
        }
        if stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Relaxed))
        {
            stopped.store(true, Ordering::Relaxed);
            break control.stop();
        }
        std::thread::sleep(STOP_POLL);
    };
    result.map_err(map_control_error)?;

    if success.load(Ordering::Relaxed) || stopped.load(Ordering::Relaxed) {
        Ok(())
    } else {
        Err(CaptureError::Wgc(STREAM_END_ERROR.to_owned()))
    }
}
