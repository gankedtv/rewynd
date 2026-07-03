//! Audio-input discovery for the settings' microphone picker. Windows walks the WASAPI capture
//! endpoints; Linux asks PipeWire (via `pw-dump`) for its audio sources. Both are best-effort — a
//! failure yields an empty list and the picker falls back to a free-text device name. Kept out of
//! the capture stack so the GPU-free settings app never links it.

use std::fmt;

/// A selectable audio input for the microphone picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioInput {
    /// The value stored in the config and matched by the capture backend: the WASAPI endpoint's
    /// friendly name on Windows, the PipeWire `node.name` on Linux.
    pub id: String,
    /// A human-friendly label shown in the picker (the friendly name on Windows, the PipeWire
    /// `node.description` on Linux).
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
    use std::process::Command;

    use super::AudioInput;

    /// The PipeWire audio sources, for the microphone picker. Best-effort: if `pw-dump` is
    /// missing or fails the list is empty and the picker falls back to free text. The stored
    /// value is the `node.name` (what the capture backend matches via `target.object`); the
    /// label is the friendlier `node.description`.
    #[must_use]
    pub fn list_audio_inputs() -> Vec<AudioInput> {
        let Ok(output) = Command::new("pw-dump").output() else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        parse_pw_dump(&output.stdout)
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

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::AudioInput;

    /// No enumeration on other platforms: the picker falls back to a free-text device name.
    #[must_use]
    pub fn list_audio_inputs() -> Vec<AudioInput> {
        Vec::new()
    }
}
