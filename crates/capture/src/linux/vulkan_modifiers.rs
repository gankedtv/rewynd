//! Query the GPU's supported DRM format modifiers for `B8G8R8A8_UNORM`.
//!
//! KWin/NVIDIA only deliver DMA-BUF buffers over PipeWire when the screencast
//! negotiation advertises *explicit* DRM format modifiers, not just
//! `DRM_FORMAT_MOD_INVALID`. This module asks Vulkan (via
//! `VK_EXT_image_drm_format_modifier`, core-extension-style) which modifiers the
//! GPU can sample/transfer for our import format, so the negotiation in
//! `pipewire_capture` can offer them.
//!
//! This is a self-contained probe: it loads Vulkan, creates a minimal device-less
//! `VkInstance`, queries `vkGetPhysicalDeviceFormatProperties2` with a
//! `VkDrmFormatModifierPropertiesListEXT` chained into the result, and tears the
//! instance back down. No logical `VkDevice` is created.

use std::ffi::CStr;

use ash::vk;
use drm_fourcc::DrmModifier;

use crate::CaptureError;

/// `DRM_FORMAT_MOD_INVALID` — the "let the driver pick" sentinel. The caller
/// always advertises this itself as a fallback, so we exclude it from the
/// explicit-modifier list we return. `From` is not const, so this is a tiny
/// helper rather than a `const`.
fn mod_invalid() -> u64 {
    u64::from(DrmModifier::Invalid)
}

/// Query the DRM format modifiers the GPU supports for `VK_FORMAT_B8G8R8A8_UNORM`.
///
/// Returns the explicit modifiers (sorted, deduped, with `DRM_FORMAT_MOD_INVALID`
/// excluded) that the GPU can use as single-plane sampled/transfer-source images.
///
/// Returns `Ok(vec![])` (not an error) when no physical device advertises
/// `VK_EXT_image_drm_format_modifier`; the caller falls back to offering only
/// `DRM_FORMAT_MOD_INVALID`. Errors are reserved for genuine Vulkan failures
/// (no loader, instance creation failed, device enumeration failed).
pub fn query_drm_format_modifiers() -> Result<Vec<u64>, CaptureError> {
    // SAFETY: `Entry::load` dynamically loads the Vulkan loader. It is unsafe
    // because it dlopen's a library and transmutes function pointers; we hold the
    // returned `Entry` for the whole query so the loader stays mapped.
    let entry = unsafe { ash::Entry::load() }
        .map_err(|e| CaptureError::Vulkan(format!("failed to load Vulkan loader: {e}")))?;

    // Minimal application info; names are irrelevant. API 1.1 makes
    // `vkGetPhysicalDeviceFormatProperties2` core (no instance extensions needed
    // for the modifier query — the device-level extension is checked per-device).
    let app_name = c"rewynd-capture-modifier-probe";
    let app_info = vk::ApplicationInfo::default()
        .application_name(app_name)
        .application_version(0)
        .engine_name(app_name)
        .engine_version(0)
        .api_version(vk::API_VERSION_1_1);
    let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);

    // SAFETY: `create_info` is valid and outlives the call. We destroy the
    // returned instance before this function returns.
    let instance = unsafe { entry.create_instance(&create_info, None) }
        .map_err(|e| CaptureError::Vulkan(format!("failed to create VkInstance: {e}")))?;

    // Run the actual query in a helper so we can always destroy the instance,
    // even on the early-return / error paths.
    let result = query_with_instance(&instance);

    // SAFETY: `instance` was created above and is not used after this call; no
    // child objects (devices, etc.) were created from it.
    unsafe { instance.destroy_instance(None) };

    result
}

/// Inner query against a live (device-less) `VkInstance`. Split out so the caller
/// can guarantee `destroy_instance` runs on every path.
fn query_with_instance(instance: &ash::Instance) -> Result<Vec<u64>, CaptureError> {
    // SAFETY: `instance` is a valid, live instance for the duration of the call.
    let physical_devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| CaptureError::Vulkan(format!("failed to enumerate physical devices: {e}")))?;

    if physical_devices.is_empty() {
        tracing::warn!("no Vulkan physical devices found; cannot query DRM format modifiers");
        return Ok(Vec::new());
    }

    // The device extension name as a `&CStr`, e.g. "VK_EXT_image_drm_format_modifier".
    let ext_name: &CStr = ash::ext::image_drm_format_modifier::NAME;

    // Pick a suitable device: prefer a discrete GPU advertising the extension,
    // then any device advertising it.
    let mut chosen: Option<vk::PhysicalDevice> = None;
    let mut chosen_is_discrete = false;
    for &phys in &physical_devices {
        // SAFETY: `phys` is a valid handle from `enumerate_physical_devices`.
        let exts = match unsafe { instance.enumerate_device_extension_properties(phys) } {
            Ok(exts) => exts,
            Err(e) => {
                tracing::warn!(
                    "failed to enumerate device extensions for a physical device: {e}; skipping"
                );
                continue;
            }
        };
        let supports_modifier = exts
            .iter()
            .filter_map(|ext| ext.extension_name_as_c_str().ok())
            .any(|name| name == ext_name);
        if !supports_modifier {
            continue;
        }

        // SAFETY: `phys` is a valid handle.
        let props = unsafe { instance.get_physical_device_properties(phys) };
        let is_discrete = props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU;

        if is_discrete {
            // Discrete GPU with the extension: best choice, stop looking.
            chosen = Some(phys);
            chosen_is_discrete = true;
            break;
        } else if chosen.is_none() {
            // First non-discrete candidate; keep as fallback but keep searching
            // for a discrete one.
            chosen = Some(phys);
        }
    }

    let Some(phys) = chosen else {
        tracing::warn!(
            "no Vulkan physical device advertises {}; \
             screencast will fall back to DRM_FORMAT_MOD_INVALID",
            ext_name.to_string_lossy()
        );
        return Ok(Vec::new());
    };

    tracing::debug!(
        discrete = chosen_is_discrete,
        "querying DRM format modifiers from selected physical device"
    );

    let modifiers = query_modifiers_for_device(instance, phys);

    let mut out: Vec<u64> = modifiers
        .into_iter()
        .map(|p| p.drm_format_modifier)
        .filter(|&m| m != mod_invalid())
        .collect();
    out.sort_unstable();
    out.dedup();

    if out.is_empty() {
        tracing::warn!(
            "GPU reported no usable explicit DRM format modifiers for B8G8R8A8_UNORM; \
             screencast will fall back to DRM_FORMAT_MOD_INVALID"
        );
    } else {
        let hex: Vec<String> = out.iter().map(|m| format!("{m:#018x}")).collect();
        tracing::info!(
            count = out.len(),
            modifiers = ?hex,
            "GPU supports explicit DRM format modifiers for B8G8R8A8_UNORM"
        );
    }

    Ok(out)
}

/// Run the two-call `vkGetPhysicalDeviceFormatProperties2` idiom to collect the
/// single-plane, sampled/transfer-capable DRM modifiers for `B8G8R8A8_UNORM`.
fn query_modifiers_for_device(
    instance: &ash::Instance,
    phys: vk::PhysicalDevice,
) -> Vec<vk::DrmFormatModifierPropertiesEXT> {
    let format = vk::Format::B8G8R8A8_UNORM;

    // --- First call: get the count. ---
    // We build the chain with explicit raw pointers (rather than `push_next`,
    // whose borrow ties the chained struct's lifetime to the parent) so that the
    // two calls below use stable, independently-owned storage. `modifier_list` is
    // declared before `format_props` so it can be chained into `p_next` directly
    // in the initializer (keeping clippy's field-reassign lint happy).
    let mut modifier_list = vk::DrmFormatModifierPropertiesListEXT::default();
    // SAFETY: `p_next` points at `modifier_list`, which lives at least as long as
    // `format_props` is used in this call (both are stack-local and not moved
    // before the call returns).
    let mut format_props = vk::FormatProperties2 {
        p_next: std::ptr::from_mut(&mut modifier_list).cast::<std::ffi::c_void>(),
        ..Default::default()
    };

    // SAFETY: `phys` is valid; `format_props` (with the chained `modifier_list`)
    // is a valid, correctly-typed output struct. The driver writes
    // `drm_format_modifier_count` into `modifier_list`.
    unsafe {
        instance.get_physical_device_format_properties2(phys, format, &mut format_props);
    }

    let count = modifier_list.drm_format_modifier_count as usize;
    if count == 0 {
        return Vec::new();
    }

    // --- Second call: fill the array. ---
    // Allocate storage and point the (same-shaped) chain at it. We rebuild the
    // structs to keep pointer provenance unambiguous.
    let mut props: Vec<vk::DrmFormatModifierPropertiesEXT> =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); count];

    let mut modifier_list = vk::DrmFormatModifierPropertiesListEXT {
        drm_format_modifier_count: count as u32,
        p_drm_format_modifier_properties: props.as_mut_ptr(),
        ..Default::default()
    };

    // SAFETY: `modifier_list` outlives this call and points at `props`'s buffer,
    // which is large enough for `count` entries.
    let mut format_props = vk::FormatProperties2 {
        p_next: std::ptr::from_mut(&mut modifier_list).cast::<std::ffi::c_void>(),
        ..Default::default()
    };

    // SAFETY: same invariants as the first call; the driver now fills `props`
    // with up to `count` entries.
    unsafe {
        instance.get_physical_device_format_properties2(phys, format, &mut format_props);
    }

    // The driver may report fewer than `count` on the second call.
    let filled = modifier_list.drm_format_modifier_count as usize;
    props.truncate(filled.min(count));

    // We only import single-plane buffers, and need the modifier to support
    // sampling or transfer-as-source for the wgpu import path.
    let wanted_features =
        vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::TRANSFER_SRC;
    props
        .into_iter()
        .filter(|p| p.drm_format_modifier_plane_count == 1)
        .filter(|p| {
            // Accept if it supports *either* sampling or transfer-src (intersects
            // the wanted set), rather than requiring both.
            !(p.drm_format_modifier_tiling_features & wanted_features).is_empty()
        })
        .collect()
}
