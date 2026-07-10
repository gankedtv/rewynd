//! Audio-input discovery for the settings' microphone picker. Windows walks the WASAPI capture
//! endpoints; Linux asks PipeWire (via `pw-dump`) for its audio sources; macOS walks the
//! CoreAudio devices with input streams. All are best-effort — a failure yields an empty list
//! and the picker falls back to a free-text device name. Kept out of the capture stack so the
//! GPU-free settings app never links it.

use std::fmt;

/// A selectable audio input for the microphone picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInput {
    /// The value stored in the config and matched by the capture backend: the WASAPI endpoint's
    /// friendly name on Windows, the PipeWire `node.name` on Linux, the CoreAudio device name
    /// on macOS.
    pub id: String,
    /// A human-friendly label shown in the picker (the friendly name on Windows, the PipeWire
    /// `node.description` on Linux, the device name on macOS).
    pub label: String,
}

impl fmt::Display for AudioInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

pub use imp::list_audio_inputs;

#[cfg(windows)]
mod imp {
    use super::AudioInput;
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
        STGM_READ,
    };
    use windows::core::GUID;

    /// `PKEY_Device_FriendlyName`: the endpoint name the sound settings show.
    const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 14,
    };

    /// All active capture (input) endpoints, for the microphone picker. Best-effort: an
    /// unreadable device is skipped, a COM failure yields an empty list.
    #[must_use]
    pub fn list_audio_inputs() -> Vec<AudioInput> {
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

    fn list_inner() -> Vec<AudioInput> {
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
                    // The friendly name is both what the picker shows and what the capture
                    // backend resolves, so the id and label are the same on Windows.
                    (!name.is_empty()).then(|| AudioInput {
                        id: name.clone(),
                        label: name,
                    })
                })
                .collect()
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::AudioInput;

    /// How long to wait for `pw-dump` before giving up. It normally returns in well under a
    /// second; the cap keeps a wedged PipeWire from hanging the settings window at startup.
    const PW_DUMP_TIMEOUT: Duration = Duration::from_secs(2);

    /// The PipeWire audio sources, for the microphone picker. Best-effort: if `pw-dump` is
    /// missing, fails, or does not finish within [`PW_DUMP_TIMEOUT`], the list is empty and the
    /// picker falls back to free text. The stored value is the `node.name` (what the capture
    /// backend matches via `target.object`); the label is the friendlier `node.description`.
    #[must_use]
    pub fn list_audio_inputs() -> Vec<AudioInput> {
        run_pw_dump(PW_DUMP_TIMEOUT).map_or_else(Vec::new, |json| parse_pw_dump(&json))
    }

    /// Run `pw-dump` with a bounded wait, returning its stdout on a clean exit or `None` on
    /// spawn failure, a non-zero exit, or the timeout. A reader thread drains stdout so a large
    /// dump can't dead-lock on a full pipe while we wait, and the child is killed on timeout.
    fn run_pw_dump(timeout: Duration) -> Option<Vec<u8>> {
        let mut child = Command::new("pw-dump")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = reader.join();
                    return None;
                }
            }
        };
        let buf = reader.join().ok()?;
        status.success().then_some(buf)
    }

    /// Extract the audio sources from a `pw-dump` JSON payload. Split from the process call so the
    /// parsing (the part with the branches) is testable without a live PipeWire session.
    fn parse_pw_dump(json: &[u8]) -> Vec<AudioInput> {
        let Ok(dump) = serde_json::from_slice::<serde_json::Value>(json) else {
            return Vec::new();
        };
        let Some(objects) = dump.as_array() else {
            return Vec::new();
        };
        let mut inputs: Vec<AudioInput> = Vec::new();
        for obj in objects {
            if obj.get("type").and_then(serde_json::Value::as_str)
                != Some("PipeWire:Interface:Node")
            {
                continue;
            }
            let Some(props) = obj.pointer("/info/props") else {
                continue;
            };
            // "Audio/Source" (and its "/Virtual" variants) are real capture endpoints; sink
            // monitors are "Audio/Sink" and are excluded, matching the mic-capture path.
            let class = props
                .get("media.class")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !class.starts_with("Audio/Source") {
                continue;
            }
            let Some(id) = props
                .get("node.name")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let label = props
                .get("node.description")
                .or_else(|| props.get("node.nick"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_owned();
            // A device can appear more than once in a dump (state churn); keep the first.
            if inputs.iter().any(|i| i.id == id) {
                continue;
            }
            inputs.push(AudioInput {
                id: id.to_owned(),
                label,
            });
        }
        inputs
    }

    #[cfg(test)]
    mod tests {
        use super::{AudioInput, list_audio_inputs, parse_pw_dump};

        #[test]
        fn parses_sources_labels_and_skips_non_sources() {
            let json = br#"[
                {"type":"PipeWire:Interface:Node","info":{"props":{
                    "media.class":"Audio/Source",
                    "node.name":"alsa_input.usb-mic",
                    "node.description":"USB Microphone"}}},
                {"type":"PipeWire:Interface:Node","info":{"props":{
                    "media.class":"Audio/Sink",
                    "node.name":"alsa_output.speakers",
                    "node.description":"Speakers"}}},
                {"type":"PipeWire:Interface:Node","info":{"props":{
                    "media.class":"Audio/Source/Virtual",
                    "node.name":"virtual.source",
                    "node.nick":"Nick Only"}}},
                {"type":"PipeWire:Interface:Node","info":{"props":{
                    "media.class":"Audio/Source",
                    "node.name":"bare.name"}}},
                {"type":"PipeWire:Interface:Node","info":{"props":{
                    "media.class":"Audio/Source",
                    "node.name":"alsa_input.usb-mic",
                    "node.description":"Duplicate"}}},
                {"type":"PipeWire:Interface:Client","info":{"props":{
                    "media.class":"Audio/Source","node.name":"not.a.node"}}}
            ]"#;
            let got = parse_pw_dump(json);
            assert_eq!(
                got,
                vec![
                    AudioInput {
                        id: "alsa_input.usb-mic".to_owned(),
                        label: "USB Microphone".to_owned(),
                    },
                    // Falls back to node.nick when there's no description.
                    AudioInput {
                        id: "virtual.source".to_owned(),
                        label: "Nick Only".to_owned(),
                    },
                    // Falls back to node.name when neither description nor nick is present.
                    AudioInput {
                        id: "bare.name".to_owned(),
                        label: "bare.name".to_owned(),
                    },
                ]
            );
        }

        #[test]
        fn malformed_payloads_yield_nothing() {
            assert!(parse_pw_dump(b"not json").is_empty());
            assert!(parse_pw_dump(b"{}").is_empty());
            assert!(parse_pw_dump(b"[]").is_empty());
            // A node with no props is skipped, not a panic.
            assert!(parse_pw_dump(br#"[{"type":"PipeWire:Interface:Node"}]"#).is_empty());
        }

        #[test]
        fn listing_never_panics() {
            // pw-dump may be absent in CI; the call must degrade to an empty list.
            let _ = list_audio_inputs();
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_char, c_void};

    use super::AudioInput;

    // Raw CoreAudio/CoreFoundation C bindings: the crate stays dependency-light (no objc2/cidre
    // here), and these are stable, documented ABI. Types per CoreAudio/AudioHardwareBase.h and
    // CoreFoundation/CFBase.h.
    type AudioObjectId = u32;
    type OsStatus = i32;
    /// `CFIndex`: a signed `long`.
    type CfIndex = isize;
    type CfStringRef = *const c_void;

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    /// A four-char property code, as the CoreAudio headers spell their constants.
    const fn fourcc(code: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*code)
    }

    // Constants from CoreAudio/AudioHardware.h + AudioHardwareBase.h (stable ABI fourccs).
    /// `kAudioObjectSystemObject`.
    const SYSTEM_OBJECT: AudioObjectId = 1;
    /// `kAudioHardwarePropertyDevices`.
    const PROPERTY_DEVICES: u32 = fourcc(b"dev#");
    /// `kAudioObjectPropertyName`.
    const PROPERTY_NAME: u32 = fourcc(b"lnam");
    /// `kAudioDevicePropertyStreams`.
    const PROPERTY_STREAMS: u32 = fourcc(b"stm#");
    /// `kAudioObjectPropertyScopeGlobal`.
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    /// `kAudioObjectPropertyScopeInput`.
    const SCOPE_INPUT: u32 = fourcc(b"inpt");
    /// `kAudioObjectPropertyElementMain`.
    const ELEMENT_MAIN: u32 = 0;
    /// `kCFStringEncodingUTF8` (CoreFoundation/CFString.h).
    const CF_ENCODING_UTF8: u32 = 0x0800_0100;
    const NO_ERR: OsStatus = 0;

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object: AudioObjectId,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            out_size: *mut u32,
        ) -> OsStatus;
        fn AudioObjectGetPropertyData(
            object: AudioObjectId,
            address: *const AudioObjectPropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            io_size: *mut u32,
            out_data: *mut c_void,
        ) -> OsStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetLength(string: CfStringRef) -> CfIndex;
        fn CFStringGetMaximumSizeForEncoding(length: CfIndex, encoding: u32) -> CfIndex;
        fn CFStringGetCString(
            string: CfStringRef,
            buffer: *mut c_char,
            buffer_size: CfIndex,
            encoding: u32,
        ) -> u8;
        fn CFRelease(cf: *const c_void);
    }

    fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            selector,
            scope,
            element: ELEMENT_MAIN,
        }
    }

    /// Byte size of `object`'s property, or `None` on any error.
    fn property_size(object: AudioObjectId, addr: &AudioObjectPropertyAddress) -> Option<u32> {
        let mut size = 0u32;
        // SAFETY: FFI; `addr` and `size` outlive the call, and no qualifier is passed.
        let status =
            unsafe { AudioObjectGetPropertyDataSize(object, addr, 0, std::ptr::null(), &mut size) };
        (status == NO_ERR).then_some(size)
    }

    /// Every audio device the hardware object knows (inputs and outputs alike).
    fn all_device_ids() -> Option<Vec<AudioObjectId>> {
        let addr = address(PROPERTY_DEVICES, SCOPE_GLOBAL);
        let size = property_size(SYSTEM_OBJECT, &addr)?;
        let count = size as usize / size_of::<AudioObjectId>();
        let mut ids = vec![0 as AudioObjectId; count];
        let mut io_size = (count * size_of::<AudioObjectId>()) as u32;
        // SAFETY: FFI; `ids` provides `io_size` writable bytes and the call reports how many
        // it actually filled.
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &addr,
                0,
                std::ptr::null(),
                &mut io_size,
                ids.as_mut_ptr().cast(),
            )
        };
        if status != NO_ERR {
            return None;
        }
        ids.truncate(io_size as usize / size_of::<AudioObjectId>());
        Some(ids)
    }

    /// Whether `device` exposes at least one input stream (i.e. can capture).
    fn has_input_streams(device: AudioObjectId) -> bool {
        property_size(device, &address(PROPERTY_STREAMS, SCOPE_INPUT)).is_some_and(|size| size > 0)
    }

    /// `device`'s human-readable name, or `None` on any error.
    fn device_name(device: AudioObjectId) -> Option<String> {
        let addr = address(PROPERTY_NAME, SCOPE_GLOBAL);
        let mut name: CfStringRef = std::ptr::null();
        let mut size = size_of::<CfStringRef>() as u32;
        // SAFETY: FFI; on success `name` holds a retained CFString we must release.
        let status = unsafe {
            AudioObjectGetPropertyData(
                device,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                (&raw mut name).cast(),
            )
        };
        if status != NO_ERR || name.is_null() {
            return None;
        }
        let text = cf_string_to_string(name);
        // SAFETY: `name` is the valid, retained CFString from the successful call above.
        unsafe { CFRelease(name) };
        text
    }

    /// A CFString's UTF-8 contents. The caller keeps its retain; this only reads.
    fn cf_string_to_string(string: CfStringRef) -> Option<String> {
        // SAFETY: FFI; `string` is a valid CFString for the whole function.
        let max = unsafe {
            CFStringGetMaximumSizeForEncoding(CFStringGetLength(string), CF_ENCODING_UTF8)
        };
        let max = usize::try_from(max).ok()?;
        let mut buf = vec![0u8; max + 1];
        // SAFETY: FFI; `buf` provides `buf.len()` writable bytes for the NUL-terminated copy.
        let ok = unsafe {
            CFStringGetCString(
                string,
                buf.as_mut_ptr().cast(),
                buf.len() as CfIndex,
                CF_ENCODING_UTF8,
            )
        };
        if ok == 0 {
            return None;
        }
        buf.truncate(buf.iter().position(|&b| b == 0)?);
        String::from_utf8(buf).ok()
    }

    /// The CoreAudio devices with input streams, for the microphone picker. Best-effort: an
    /// unreadable device is skipped, an enumeration failure yields an empty list, and the picker
    /// falls back to free text. The name is both the id and the label — the capture backend
    /// matches by device name, as on Windows.
    #[must_use]
    pub fn list_audio_inputs() -> Vec<AudioInput> {
        let Some(ids) = all_device_ids() else {
            tracing::warn!("could not enumerate CoreAudio devices");
            return Vec::new();
        };
        ids.into_iter()
            .filter(|&id| has_input_streams(id))
            .filter_map(device_name)
            .filter(|name| !name.is_empty())
            .map(|name| AudioInput {
                id: name.clone(),
                label: name,
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::{fourcc, list_audio_inputs};

        #[test]
        fn fourcc_matches_the_header_spelling() {
            assert_eq!(fourcc(b"dev#"), 0x6465_7623);
            assert_eq!(fourcc(b"lnam"), 0x6c6e_616d);
        }

        #[test]
        fn listing_never_panics_and_yields_named_inputs() {
            // The device set is machine-specific; only the invariants are asserted.
            for input in list_audio_inputs() {
                assert!(!input.id.is_empty());
                assert_eq!(input.id, input.label);
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use super::AudioInput;

    /// No enumeration on other platforms: the picker falls back to a free-text device name.
    #[must_use]
    pub fn list_audio_inputs() -> Vec<AudioInput> {
        Vec::new()
    }
}
