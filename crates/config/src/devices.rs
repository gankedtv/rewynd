//! Audio-device discovery for the settings UI (Windows). A twin of the small COM
//! walk the capture backend uses to *resolve* a configured name — kept separate so
//! the GPU-free settings app never links the capture stack.

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, STGM_READ,
};
use windows::core::GUID;

/// `PKEY_Device_FriendlyName`: the endpoint name the sound settings show.
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

/// The friendly names of all active capture (input) endpoints, for the microphone
/// picker. Best-effort: an unreadable device is skipped, a COM failure yields an
/// empty list (the picker then falls back to free text).
#[must_use]
pub fn list_audio_inputs() -> Vec<String> {
    // SAFETY: FFI; paired with `CoUninitialize` below. S_FALSE (already
    // initialized on this thread) is fine.
    if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
        return Vec::new();
    }
    let names = list_inner();
    // SAFETY: FFI; pairs the successful init.
    unsafe { CoUninitialize() };
    names
}

fn list_inner() -> Vec<String> {
    // SAFETY: FFI (all calls below); indices stay within the collection's count.
    unsafe {
        let Ok(enumerator): windows::core::Result<IMMDeviceEnumerator> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return Vec::new();
        };
        let Ok(devices) = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        let Ok(count) = devices.GetCount() else {
            return Vec::new();
        };
        (0..count)
            .filter_map(|i| {
                let device = devices.Item(i).ok()?;
                let store = device.OpenPropertyStore(STGM_READ).ok()?;
                let value = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME).ok()?;
                let name = value.to_string();
                (!name.is_empty()).then_some(name)
            })
            .collect()
    }
}
