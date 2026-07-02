//! In-game save feedback for Windows: a transient on-screen badge plus a chime.
//!
//! Windows suppresses toast notifications while a fullscreen game has focus, so the
//! recorder needs its own confirmation channel. The badge is a layered, topmost,
//! click-through window — visible over borderless/flip-model fullscreen (the common
//! case on Windows 11) without injecting into the game, which anti-cheat would ban.
//! Over true exclusive fullscreen it can't show; the chime still lands.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET,
    DIB_RGB_COLORS, DT_CALCRECT, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DeleteDC,
    DeleteObject, DrawTextW, FF_DONTCARE, FW_SEMIBOLD, GetMonitorInfoW, HDC,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, OUT_DEFAULT_PRECIS, PROOF_QUALITY,
    SelectObject, SetBkMode, SetTextColor, TRANSPARENT, VARIABLE_PITCH,
};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetMessageW, HWND_TOPMOST, MB_ICONHAND, MB_OK, MSG, PostQuitMessage, RegisterClassW,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetTimer, SetWindowPos, ShowWindow,
    TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WM_DESTROY, WM_TIMER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

/// How long the badge stays on screen: long enough to catch mid-fight in the
/// corner of the eye, short enough to never feel like HUD clutter.
const BADGE_MS: u32 = 4000;
/// Timer that dismisses the badge.
const DISMISS_TIMER: usize = 1;
/// Timer that re-asserts the badge's topmost position (see `place_badge`).
const RAISE_TIMER: usize = 2;
/// How often the badge lifts itself back above a competing topmost window.
const RAISE_MS: u32 = 500;
/// Whole-badge opacity (SourceConstantAlpha): solid enough to read, soft enough
/// to feel like an overlay rather than a dialog.
const BADGE_ALPHA: u8 = 242;

/// The badge's left-edge color: the one visual bit of outcome signal.
#[derive(Clone, Copy)]
pub enum Accent {
    Success,
    Failure,
}

impl Accent {
    /// 0xAARRGGBB as stored in the premultiplied top-down DIB (alpha fixed at FF).
    fn argb(self) -> u32 {
        match self {
            Accent::Success => 0xFF2E_CC71,
            Accent::Failure => 0xFFE7_4C3C,
        }
    }
}

/// The clip-saved chime (see `assets/`): generated two-note pling, mono 16-bit WAV.
static CHIME: &[u8] = include_bytes!("../assets/clip-saved.wav");

/// The audible half of the feedback: the chime on success, the system error
/// beep on failure — the confirmation channel that still works over exclusive
/// fullscreen, where the badge can't show.
pub fn play(accent: Accent) {
    match accent {
        Accent::Success => play_chime(),
        Accent::Failure => {
            // SAFETY: trivially safe FFI.
            let _ = unsafe { MessageBeep(MB_ICONHAND) };
        }
    }
}

/// Play the save chime (async, from memory). Falls back to the system beep if
/// winmm refuses, so a save never confirms silently.
fn play_chime() {
    // SAFETY: trivially safe FFI; the buffer is 'static, outliving async playback.
    let ok = unsafe {
        PlaySoundW(
            PCWSTR(CHIME.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
    };
    if !ok.as_bool() {
        // SAFETY: trivially safe FFI.
        let _ = unsafe { MessageBeep(MB_OK) };
    }
}

/// Show the badge near the top-right of the monitor the foreground window (the game)
/// is on. Fire-and-forget: the badge lives on its own thread with its own message
/// loop and destroys itself after [`BADGE_MS`].
pub fn show(accent: Accent, text: &str) {
    let text = text.to_owned();
    let spawned = std::thread::Builder::new()
        .name("rewynd-overlay".to_owned())
        .spawn(move || {
            if let Err(e) = show_badge(accent, &text) {
                tracing::warn!(error = %e, "could not show the in-game badge");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not spawn the overlay thread");
    }
}

fn show_badge(accent: Accent, text: &str) -> windows::core::Result<()> {
    // SAFETY: trivially safe FFI (module handle of our own process).
    let instance = unsafe { GetModuleHandleW(None)? };
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(badge_proc),
            hInstance: instance.into(),
            lpszClassName: badge_class(),
            ..Default::default()
        };
        // SAFETY: trivially safe FFI; 0 (failure) surfaces as a CreateWindowExW
        // error below.
        let _ = unsafe { RegisterClassW(&wc) };
    });

    // The monitor the game is on; falls back to primary with no foreground window.
    // SAFETY: trivially safe FFI.
    let monitor = unsafe { MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a properly sized MONITORINFO.
    unsafe { GetMonitorInfoW(monitor, &mut info) }.ok()?;
    let (mut dpi_x, mut dpi_y) = (96_u32, 96_u32);
    // SAFETY: valid monitor handle and out-pointers.
    let _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };

    let hwnd = build_badge(accent, text, info.rcMonitor, dpi_x)?;

    // Pump until the WM_TIMER -> DestroyWindow -> PostQuitMessage chain ends us.
    let mut msg = MSG::default();
    // SAFETY: FFI; drains this thread's own queue.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        // SAFETY: FFI on a message this thread owns.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    let _ = hwnd; // destroyed by badge_proc
    Ok(())
}

/// Measure the text and paint + place the badge window. Split into layers so every
/// early error return still unwinds the GDI objects created before it.
fn build_badge(accent: Accent, text: &str, monitor: RECT, dpi: u32) -> windows::core::Result<HWND> {
    let scale = |px: i32| px * dpi as i32 / 96;
    // SAFETY: GDI resource creation with paired cleanup before every return.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err(windows::core::Error::from_thread());
        }
        let font = CreateFontW(
            -scale(16),
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            // Grayscale antialiasing: ClearType writes junk into the alpha channel
            // of a layered window's surface.
            PROOF_QUALITY,
            (VARIABLE_PITCH.0 | FF_DONTCARE.0).into(),
            w!("Segoe UI"),
        );
        let old_font = SelectObject(dc, font.into());
        let placed = paint_badge(dc, accent, text, monitor, scale);
        SelectObject(dc, old_font);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);
        placed
    }
}

/// Inner layer of [`build_badge`]: the DIB the badge is drawn into. The caller owns
/// the DC and the font already selected into it.
fn paint_badge(
    dc: HDC,
    accent: Accent,
    text: &str,
    monitor: RECT,
    scale: impl Fn(i32) -> i32,
) -> windows::core::Result<HWND> {
    // SAFETY: GDI painting into a DIB owned (and cleaned up) by this function.
    unsafe {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut measure = RECT::default();
        DrawTextW(
            dc,
            &mut wide,
            &mut measure,
            DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT,
        );
        let bar = scale(4);
        let (pad_x, pad_y) = (scale(14), scale(12));
        let text_w = (measure.right - measure.left).min(scale(560));
        let text_h = measure.bottom - measure.top;
        let (w, h) = (bar + pad_x + text_w + pad_x, text_h + 2 * pad_y);

        // 32-bit top-down DIB.
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let old_bitmap = SelectObject(dc, bitmap.into());

        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (w * h) as usize);
        pixels.fill(0xFF14_161A); // near-black panel
        for row in pixels.chunks_exact_mut(w as usize) {
            row[..bar as usize].fill(accent.argb());
        }

        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00F5_F5F5));
        let mut text_rect = RECT {
            left: bar + pad_x,
            top: pad_y,
            right: bar + pad_x + text_w,
            bottom: pad_y + text_h,
        };
        DrawTextW(
            dc,
            &mut wide,
            &mut text_rect,
            DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        // GDI text output zeroes the alpha of the pixels it touches; the badge is
        // fully opaque, so force alpha back on everywhere.
        for px in pixels.iter_mut() {
            *px |= 0xFF00_0000;
        }

        let pos = POINT {
            x: monitor.right - w - scale(24),
            y: monitor.top + scale(24),
        };
        let shown = place_badge(dc, pos, SIZE { cx: w, cy: h });

        // The layered surface owns a copy of the pixels now; the GDI sources can go.
        SelectObject(dc, old_bitmap);
        let _ = DeleteObject(bitmap.into());
        shown
    }
}

/// Innermost layer: the layered window itself, fed from the painted DC. A window
/// that fails to take the surface is destroyed on the spot, not leaked.
fn place_badge(dc: HDC, pos: POINT, size: SIZE) -> windows::core::Result<HWND> {
    // SAFETY: window creation on this thread, destroyed by badge_proc (or here on error).
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            badge_class(),
            PCWSTR::null(),
            WS_POPUP,
            pos.x,
            pos.y,
            size.cx,
            size.cy,
            None,
            None,
            GetModuleHandleW(None).ok().map(|m| m.into()),
            None,
        )?;
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            SourceConstantAlpha: BADGE_ALPHA,
            AlphaFormat: AC_SRC_ALPHA as u8,
            ..Default::default()
        };
        if let Err(e) = UpdateLayeredWindow(
            hwnd,
            None,
            Some(&pos),
            Some(&size),
            Some(dc),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        ) {
            let _ = DestroyWindow(hwnd);
            return Err(e);
        }
        // A badge with no working timer would stay on screen (and pump) forever.
        if SetTimer(Some(hwnd), DISMISS_TIMER, BADGE_MS, None) == 0 {
            let e = windows::core::Error::from_thread();
            let _ = DestroyWindow(hwnd);
            return Err(e);
        }
        // Best-effort: a game (or its anti-cheat) re-asserting its own topmost
        // window would bury the badge; this timer keeps lifting it back.
        let _ = SetTimer(Some(hwnd), RAISE_TIMER, RAISE_MS, None);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok(hwnd)
    }
}

fn badge_class() -> PCWSTR {
    w!("rewynd.overlay")
}

unsafe extern "system" fn badge_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == RAISE_TIMER => {
            // SAFETY: our own live window; no move/size/activation, only z-order.
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
            };
            LRESULT(0)
        }
        WM_TIMER => {
            // SAFETY: our own live window (messages stop after WM_DESTROY).
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: trivially safe FFI; ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
