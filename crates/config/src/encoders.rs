//! Encoder capability probe and backend selection — the GPU-free half.
//!
//! The recorder enumerates Vulkan adapters (in the GPU crate) and serialises them here as an
//! [`EncoderProbe`]; the `--probe-encoders` subcommand prints it as JSON for the settings GUI,
//! which is deliberately wgpu-free (ADR 0006) and so can't enumerate adapters itself.
//! [`choose_encoder`] then resolves the user's [`EncoderPreference`] against those adapters.

use serde::{Deserialize, Serialize};

use crate::schema::EncoderPreference;

/// Bumped when the probe JSON shape changes incompatibly; the reader rejects other versions.
pub const ENCODER_PROBE_VERSION: u32 = 1;

/// The recorder's view of the machine's encoders, as published to the GUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderProbe {
    /// Schema version; see [`ENCODER_PROBE_VERSION`].
    pub version: u32,
    /// Every Vulkan adapter found, encode-capable or not.
    pub adapters: Vec<ProbeAdapter>,
}

/// One adapter's encode capability, flattened for the GUI (no GPU types cross the boundary).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeAdapter {
    /// Adapter name; also the key config uses to pin a device (`encoder = "gpu:<name>"`).
    pub name: String,
    /// Adapter class: `"discrete"`, `"integrated"`, `"virtual"`, `"cpu"`, or `"other"`.
    pub device_type: String,
    /// Whether this adapter can encode H.264 via Vulkan Video.
    pub h264_encode: bool,
    /// Maximum encode width (0 when `!h264_encode`).
    pub max_width: u32,
    /// Maximum encode height (0 when `!h264_encode`).
    pub max_height: u32,
}

impl EncoderProbe {
    /// Wrap adapters with the current schema version.
    #[must_use]
    pub fn new(adapters: Vec<ProbeAdapter>) -> Self {
        Self {
            version: ENCODER_PROBE_VERSION,
            adapters,
        }
    }

    /// Serialise to the JSON the `--probe-encoders` subcommand prints.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| format!("{{\"version\":{ENCODER_PROBE_VERSION},\"adapters\":[]}}"))
    }

    /// Parse the subcommand's JSON, rejecting an unrecognised schema version.
    ///
    /// # Errors
    /// Malformed JSON or a version other than [`ENCODER_PROBE_VERSION`].
    pub fn from_json(s: &str) -> Result<Self, String> {
        let probe: Self = serde_json::from_str(s).map_err(|e| e.to_string())?;
        if probe.version != ENCODER_PROBE_VERSION {
            return Err(format!(
                "unsupported encoder probe version {} (expected {ENCODER_PROBE_VERSION})",
                probe.version
            ));
        }
        Ok(probe)
    }
}

/// The backend the recorder will actually use — the resolved form of [`EncoderPreference`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderChoice {
    /// The named GPU (Vulkan Video H.264).
    Gpu(String),
    /// The software (CPU) encoder.
    Cpu,
}

impl EncoderChoice {
    /// The value written to `status.json`'s `encoder` field (`"gpu:<name>"` / `"cpu"`).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Gpu(name) => format!("gpu:{name}"),
            Self::Cpu => "cpu".to_owned(),
        }
    }
}

/// Resolve the effective backend from the user's preference and the probed adapters.
///
/// Returns the choice plus an optional user-facing warning when the resolved backend differs
/// from what the preference asked for (a pinned GPU that vanished or can't encode, or auto
/// finding no encode-capable GPU). Auto prefers the first encode-capable adapter, else CPU.
#[must_use]
pub fn choose_encoder(
    pref: &EncoderPreference,
    adapters: &[ProbeAdapter],
) -> (EncoderChoice, Option<String>) {
    match pref {
        EncoderPreference::Cpu => (EncoderChoice::Cpu, None),
        EncoderPreference::Auto => match adapters.iter().find(|a| a.h264_encode) {
            Some(a) => (EncoderChoice::Gpu(a.name.clone()), None),
            None => (
                EncoderChoice::Cpu,
                Some("No GPU can encode H.264; recording on the CPU.".to_owned()),
            ),
        },
        EncoderPreference::Gpu(name) => match adapters.iter().find(|a| &a.name == name) {
            Some(a) if a.h264_encode => (EncoderChoice::Gpu(name.clone()), None),
            Some(_) => fallback(
                adapters,
                &format!("The selected GPU {name:?} can't encode H.264"),
            ),
            None => fallback(adapters, &format!("The selected GPU {name:?} wasn't found")),
        },
    }
}

/// Best available fallback (first encode-capable GPU, else CPU), phrased around `reason`.
fn fallback(adapters: &[ProbeAdapter], reason: &str) -> (EncoderChoice, Option<String>) {
    match adapters.iter().find(|a| a.h264_encode) {
        Some(a) => (
            EncoderChoice::Gpu(a.name.clone()),
            Some(format!("{reason}; using {:?} instead.", a.name)),
        ),
        None => (
            EncoderChoice::Cpu,
            Some(format!("{reason}; using the CPU encoder.")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(name: &str, h264: bool) -> ProbeAdapter {
        ProbeAdapter {
            name: name.to_owned(),
            device_type: "discrete".to_owned(),
            h264_encode: h264,
            max_width: if h264 { 4096 } else { 0 },
            max_height: if h264 { 4096 } else { 0 },
        }
    }

    #[test]
    fn probe_json_round_trips() {
        let probe = EncoderProbe::new(vec![adapter("RTX 3080 Ti", true), adapter("iGPU", false)]);
        let parsed = EncoderProbe::from_json(&probe.to_json()).expect("parses");
        assert_eq!(parsed, probe);
    }

    #[test]
    fn probe_json_rejects_bad_version() {
        let json = r#"{"version":999,"adapters":[]}"#;
        assert!(EncoderProbe::from_json(json).is_err());
        assert!(EncoderProbe::from_json("not json").is_err());
    }

    #[test]
    fn auto_prefers_encode_gpu() {
        let adapters = [adapter("iGPU", false), adapter("RTX", true)];
        let (choice, warn) = choose_encoder(&EncoderPreference::Auto, &adapters);
        assert_eq!(choice, EncoderChoice::Gpu("RTX".to_owned()));
        assert!(warn.is_none());
    }

    #[test]
    fn auto_falls_back_to_cpu_and_warns() {
        let adapters = [adapter("iGPU", false)];
        let (choice, warn) = choose_encoder(&EncoderPreference::Auto, &adapters);
        assert_eq!(choice, EncoderChoice::Cpu);
        assert!(warn.is_some());
    }

    #[test]
    fn cpu_preference_is_honoured_without_warning() {
        let adapters = [adapter("RTX", true)];
        let (choice, warn) = choose_encoder(&EncoderPreference::Cpu, &adapters);
        assert_eq!(choice, EncoderChoice::Cpu);
        assert!(warn.is_none());
    }

    #[test]
    fn pinned_gpu_present_and_capable() {
        let adapters = [adapter("RTX", true)];
        let (choice, warn) = choose_encoder(&EncoderPreference::Gpu("RTX".to_owned()), &adapters);
        assert_eq!(choice, EncoderChoice::Gpu("RTX".to_owned()));
        assert!(warn.is_none());
    }

    #[test]
    fn pinned_gpu_missing_falls_back_to_other_gpu() {
        let adapters = [adapter("RTX", true)];
        let (choice, warn) = choose_encoder(&EncoderPreference::Gpu("Arc".to_owned()), &adapters);
        assert_eq!(choice, EncoderChoice::Gpu("RTX".to_owned()));
        assert!(warn.is_some());
    }

    #[test]
    fn pinned_gpu_incapable_falls_back_to_cpu() {
        let adapters = [adapter("iGPU", false)];
        let (choice, warn) = choose_encoder(&EncoderPreference::Gpu("iGPU".to_owned()), &adapters);
        assert_eq!(choice, EncoderChoice::Cpu);
        assert!(warn.is_some());
    }

    #[test]
    fn choice_label_matches_config_form() {
        assert_eq!(EncoderChoice::Cpu.label(), "cpu");
        assert_eq!(EncoderChoice::Gpu("RTX".to_owned()).label(), "gpu:RTX");
    }
}
