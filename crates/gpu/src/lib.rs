//! Shared wgpu device/queue and capture-import helpers (PLAN §4.2, §6.1).
//!
//! [`GpuContext::new`] creates the wgpu device via `gpu-video`'s
//! `request_device_with_video_support`, so the device is shared with the encoder
//! (same wgpu source end-to-end — see docs/adr/0001-wgpu-rev.md). It enables the
//! external-memory features the capture-import path needs (DMA-BUF on Linux, the
//! D3D11 shared handle on Windows). DMA-BUF import lives in the `import` submodule.

use thiserror::Error;

#[cfg(target_os = "linux")]
mod import;
#[cfg(target_os = "windows")]
mod import_windows;

#[cfg(target_os = "linux")]
pub use import::DmabufImport;
#[cfg(target_os = "windows")]
pub use import_windows::D3d11HandleImport;

/// Errors from GPU setup.
#[derive(Debug, Error)]
pub enum GpuError {
    /// No Vulkan adapter at all — the machine can't run the capture pipeline (which needs
    /// Vulkan to import capture frames), software encoder or not.
    #[error("no Vulkan adapter found (is a Vulkan driver installed?)")]
    NoVulkanAdapter,
    /// Vulkan adapters exist, but none exposes H.264 video-encode support. The software
    /// (CPU) encoder is the fallback here.
    #[error("no Vulkan adapter supports H.264 video encode")]
    NoEncodeAdapter,
    /// The GPU pinned in config (`encoder = "gpu:<name>"`) is not present.
    #[error("the configured GPU {0:?} was not found")]
    AdapterUnavailable(String),
    /// The wgpu device could not be created.
    #[error("failed to create the shared wgpu device: {0}")]
    DeviceCreation(String),
    /// A captured GPU resource (DMA-BUF / shared handle) could not be imported as
    /// a [`wgpu::Texture`].
    #[error("failed to import external GPU memory: {0}")]
    Import(String),
}

/// One Vulkan adapter's H.264-encode capability, as probed at recorder start. Feeds the
/// backend selector and the GUI's "Recording method" picker.
#[derive(Debug, Clone)]
pub struct AdapterEncodeInfo {
    /// Human-readable adapter name (`wgpu::AdapterInfo::name`), also the key config uses to
    /// pin a device (`encoder = "gpu:<name>"`).
    pub name: String,
    /// Adapter class (discrete / integrated / …), for labelling and default preference.
    pub device_type: wgpu::DeviceType,
    /// Whether this adapter can encode H.264 via Vulkan Video.
    pub h264_encode: bool,
    /// Maximum encode width across the H.264 profiles (0 when `!h264_encode`).
    pub max_width: u32,
    /// Maximum encode height across the H.264 profiles (0 when `!h264_encode`).
    pub max_height: u32,
}

impl AdapterEncodeInfo {
    /// A stable lowercase device-class label, for serialisation and the GUI (the settings
    /// crate is wgpu-free, so it can't match on [`wgpu::DeviceType`] itself).
    #[must_use]
    pub fn device_kind(&self) -> &'static str {
        match self.device_type {
            wgpu::DeviceType::DiscreteGpu => "discrete",
            wgpu::DeviceType::IntegratedGpu => "integrated",
            wgpu::DeviceType::VirtualGpu => "virtual",
            wgpu::DeviceType::Cpu => "cpu",
            wgpu::DeviceType::Other => "other",
        }
    }
}

/// The wgpu device/queue shared across the pipeline and handed to `gpu-video`.
#[derive(Debug)]
pub struct GpuContext {
    /// The wgpu device, created on the Vulkan backend with interop features enabled.
    pub device: wgpu::Device,
    /// The queue paired with [`device`](GpuContext::device).
    pub queue: wgpu::Queue,
}

// The shared device is created through gpu-video's video-capable device path, which
// only exists where Vulkan does (Windows + non-Apple unixes); macOS is out of scope.
#[cfg(vulkan)]
impl GpuContext {
    /// Create the shared device on the first Vulkan adapter that can encode H.264, enabling
    /// whichever external-memory features the adapter advertises (so the capture-import path
    /// can import DMA-BUF / D3D11 memory zero-copy).
    pub async fn new() -> Result<Self, GpuError> {
        use gpu_video::VideoAdapterExt;

        let instance = vulkan_instance();
        let adapters = instance.enumerate_adapters(wgpu::Backends::VULKAN).await;
        if adapters.is_empty() {
            return Err(GpuError::NoVulkanAdapter);
        }
        let adapter = adapters
            .into_iter()
            .find(|adapter| {
                adapter
                    .video_adapter_info()
                    .is_some_and(|info| info.encode_capabilities.h264.is_some())
            })
            .ok_or(GpuError::NoEncodeAdapter)?;
        let (device, queue) = request_video_device(&adapter)?;
        Ok(Self { device, queue })
    }

    /// Create the shared device on the named adapter (config's `encoder = "gpu:<name>"`).
    /// Errors if the adapter is absent or can't encode H.264.
    pub async fn new_for_adapter(name: &str) -> Result<Self, GpuError> {
        use gpu_video::VideoAdapterExt;

        let instance = vulkan_instance();
        let adapter = instance
            .enumerate_adapters(wgpu::Backends::VULKAN)
            .await
            .into_iter()
            .find(|adapter| adapter.get_info().name == name)
            .ok_or_else(|| GpuError::AdapterUnavailable(name.to_owned()))?;
        if adapter
            .video_adapter_info()
            .is_none_or(|info| info.encode_capabilities.h264.is_none())
        {
            return Err(GpuError::NoEncodeAdapter);
        }
        let (device, queue) = request_video_device(&adapter)?;
        Ok(Self { device, queue })
    }

    /// Create a plain render device (no Vulkan Video) for the software-encoder path. Still
    /// needs the external-memory features (capture import) and `TEXTURE_FORMAT_NV12` (the
    /// RGBA→NV12 converter) — gpu-video's video device enabled the latter implicitly, so it
    /// must be requested explicitly here.
    pub async fn new_render_only() -> Result<Self, GpuError> {
        let instance = vulkan_instance();
        let adapter = instance
            .enumerate_adapters(wgpu::Backends::VULKAN)
            .await
            .into_iter()
            .next()
            .ok_or(GpuError::NoVulkanAdapter)?;
        let features = wgpu::Features::IMMEDIATES
            | wgpu::Features::TEXTURE_FORMAT_NV12
            | interop_features(&adapter);
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("rewynd render-only device"),
                required_features: features,
                required_limits: wgpu::Limits {
                    max_immediate_size: 4,
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .map_err(|e| GpuError::DeviceCreation(e.to_string()))?;
        Ok(Self { device, queue })
    }

    /// Enumerate every Vulkan adapter and its H.264-encode capability, for the backend
    /// selector and the GUI device picker. Best-effort: returns an empty list when Vulkan
    /// is unavailable, never errors.
    pub async fn probe_adapters() -> Vec<AdapterEncodeInfo> {
        use gpu_video::VideoAdapterExt;

        let instance = vulkan_instance();
        instance
            .enumerate_adapters(wgpu::Backends::VULKAN)
            .await
            .into_iter()
            .map(|adapter| {
                let info = adapter.get_info();
                let h264 = adapter
                    .video_adapter_info()
                    .and_then(|v| v.encode_capabilities.h264);
                let (max_width, max_height) = h264.as_ref().map_or((0, 0), |caps| {
                    [
                        &caps.baseline_profile,
                        &caps.main_profile,
                        &caps.high_profile,
                    ]
                    .into_iter()
                    .flatten()
                    .fold((0u32, 0u32), |(w, h), p| {
                        (w.max(p.max_width), h.max(p.max_height))
                    })
                });
                AdapterEncodeInfo {
                    name: info.name,
                    device_type: info.device_type,
                    h264_encode: h264.is_some(),
                    max_width,
                    max_height,
                }
            })
            .collect()
    }
}

/// The Vulkan-only wgpu instance the pipeline uses. Vulkan only: the encoder requires it and
/// we enumerate Vulkan adapters, so there's no reason to initialise other backends' drivers.
#[cfg(vulkan)]
fn vulkan_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

/// The external-memory features this adapter advertises, for zero-copy capture import.
#[cfg(vulkan)]
fn interop_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    let interop = wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF
        | wgpu::Features::VULKAN_EXTERNAL_MEMORY_FD
        | wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32;
    adapter.features() & interop
}

/// Create the shared device through gpu-video's video-capable path (encoder + import).
#[cfg(vulkan)]
fn request_video_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), GpuError> {
    use gpu_video::{VideoAdapterExt, parameters::VideoDeviceDescriptor};

    let features = wgpu::Features::IMMEDIATES | interop_features(adapter);
    adapter
        .request_device_with_video_support(&VideoDeviceDescriptor {
            wgpu_features: features,
            wgpu_limits: wgpu::Limits {
                max_immediate_size: 4,
                ..Default::default()
            },
            ..Default::default()
        })
        .map_err(|e| GpuError::DeviceCreation(e.to_string()))
}
