//! PipeWire stream setup + DMA-BUF format negotiation (PLAN §3.5).
//!
//! This is the crux of #4: we connect to the portal's PipeWire remote, offer an
//! `EnumFormat` that advertises DRM-modifier support (so the server hands us
//! DMA-BUF buffers instead of SHM copies), drive the two-pass modifier fixation
//! handshake, request DMA-BUF buffers via a `SPA_PARAM_Buffers` param, and then
//! log each arriving frame's DMA-BUF descriptor.
//!
//! Unlike the upstream `ashpd`/`pipewire-rs` examples (which use
//! `StreamFlags::MAP_BUFFERS` and end up with SHM), we deliberately:
//! - omit `MAP_BUFFERS`,
//! - attach a `FormatProperties::VideoModifier` choice with the
//!   `MANDATORY | DONT_FIXATE` flags (built by hand — the `property!` macro
//!   hardcodes empty flags),
//! - re-emit a single-modifier `EnumFormat` to fixate, then a `SPA_PARAM_Buffers`
//!   pinning `dataType = 1 << SPA_DATA_DmaBuf`.

use std::io::Cursor;

use pipewire as pw;
use pw::spa;
use pw::spa::param::ParamType;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::deserialize::PodDeserializer;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value, object, property};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

use crate::CaptureError;

/// `DRM_FORMAT_MOD_INVALID` — "let the driver pick" / implicit modifier.
/// `fourcc_mod_code(NONE, DRM_FORMAT_RESERVED)` = `0x00ffffffffffffff`.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// `enum spa_data_type` values (from `spa/buffer/buffer.h`). Used to build the
/// `SPA_PARAM_BUFFERS_dataType` flags choice.
const SPA_DATA_MEM_PTR: i32 = 1;
const SPA_DATA_MEM_FD: i32 = 2;
const SPA_DATA_DMA_BUF: i32 = 3;

/// `enum spa_param_buffers` indices (from `spa/param/buffers.h`). These are the
/// property keys inside a `SPA_TYPE_OBJECT_ParamBuffers` object.
const SPA_PARAM_BUFFERS_BLOCKS: u32 = 2;
const SPA_PARAM_BUFFERS_DATA_TYPE: u32 = 6;

/// How many DMA-BUF frames to log before quitting the probe.
const FRAMES_TO_LOG: u32 = 5;
/// Stop logging repeated non-DMA-BUF buffers after this many (avoids log spam).
const NON_DMABUF_LOG_LIMIT: u32 = 3;
/// Give up after this many non-DMA-BUF buffers (the dmabuf negotiation didn't take).
const NON_DMABUF_ABORT_AFTER: u32 = 30;
/// Bound on modifier-fixation round-trips, so a server that keeps re-proposing an
/// unfixated format can't livelock the negotiation.
const MAX_FIXATION_ATTEMPTS: u32 = 4;

/// A self-contained description of one captured DMA-BUF plane + its format.
///
/// This is the deliverable of #4: the import into a `wgpu::Texture` (turning this
/// into a [`crate::GpuFrame`]) is #5.
#[derive(Debug, Clone)]
pub struct DmabufFrame {
    /// The DMA-BUF file descriptor for the plane (borrowed from PipeWire; valid
    /// only for the duration of the buffer — do not close it here).
    pub fd: i64,
    /// DRM `fourcc` pixel format (e.g. `DRM_FORMAT_XRGB8888`).
    pub fourcc: u32,
    /// DRM format modifier (tiling/compression layout, or
    /// [`DRM_FORMAT_MOD_INVALID`] for an implicit/linear layout).
    pub modifier: u64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: i32,
    /// Byte offset of the plane within the DMA-BUF.
    pub offset: i32,
    /// Number of planes in the buffer (we only accept single-plane formats).
    pub num_planes: usize,
}

/// Map a SPA [`VideoFormat`] to its DRM `fourcc`, accounting for the SPA→DRM
/// byte-order swap (SPA names list bytes in memory order; DRM `fourcc` little-endian
/// codes spell the channels in the *opposite* order).
///
/// Returns `None` for formats we do not offer (multi-plane / non-packed-RGB).
fn spa_format_to_drm_fourcc(format: VideoFormat) -> Option<u32> {
    // DRM fourcc helper: 4 ASCII bytes, little-endian.
    const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
        (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
    }
    // SPA BGRx (B,G,R,x in memory) == DRM_FORMAT_XRGB8888 ('X','R','2','4' => 0x34325258)
    // SPA BGRA (B,G,R,A in memory) == DRM_FORMAT_ARGB8888 ('A','R','2','4' => 0x34325241)
    // SPA RGBx (R,G,B,x in memory) == DRM_FORMAT_XBGR8888 ('X','B','2','4' => 0x34324258)
    // SPA RGBA (R,G,B,A in memory) == DRM_FORMAT_ABGR8888 ('A','B','2','4' => 0x34324241)
    let code = match format {
        VideoFormat::BGRx => fourcc(b'X', b'R', b'2', b'4'),
        VideoFormat::BGRA => fourcc(b'A', b'R', b'2', b'4'),
        VideoFormat::RGBx => fourcc(b'X', b'B', b'2', b'4'),
        VideoFormat::RGBA => fourcc(b'A', b'B', b'2', b'4'),
        _ => return None,
    };
    Some(code)
}

/// Per-stream state threaded through the PipeWire callbacks.
struct UserData {
    /// The most recently negotiated raw video format.
    format: VideoInfoRaw,
    /// Whether we have already fixated the modifier (so we only do the second
    /// pass once and only then emit the Buffers param).
    fixated: bool,
    /// Number of modifier-fixation round-trips attempted (bounded to avoid livelock).
    fixation_attempts: u32,
    /// Count of DMA-BUF frames logged so far.
    frames_logged: u32,
    /// Count of non-DMA-BUF (SHM/empty) buffers seen, to bound log spam and bail.
    non_dmabuf_seen: u32,
    /// Clone of the main loop so a callback can `quit()` after N frames.
    main_loop: pw::main_loop::MainLoopRc,
}

/// Serialize a pod [`Object`] to bytes (suitable for `Pod::from_bytes`).
fn serialize_object(obj: Object) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("pod serialization cannot fail for in-memory buffer")
        .0
        .into_inner()
}

/// Build the by-hand `VideoModifier` property carrying a `SPA_CHOICE_Enum` of
/// supported modifiers, with the `MANDATORY | DONT_FIXATE` flags that the
/// `property!` macro cannot set.
///
/// When `fixate_to` is `Some(modifier)` the property is emitted *without*
/// `DONT_FIXATE` and as a plain (non-choice) `Long` pinning that single modifier —
/// the second pass of the fixation handshake.
fn video_modifier_property(modifiers: &[u64], fixate_to: Option<u64>) -> Property {
    if let Some(chosen) = fixate_to {
        // Pin exactly the modifier the server selected in pass 1; mandatory, fixated.
        Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY,
            value: Value::Long(chosen as i64),
        }
    } else {
        let alternatives: Vec<i64> = modifiers.iter().map(|&m| m as i64).collect();
        let default = alternatives
            .first()
            .copied()
            .unwrap_or(DRM_FORMAT_MOD_INVALID as i64);
        Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default,
                    alternatives,
                },
            ))),
        }
    }
}

/// Build an `EnumFormat` object advertising packed single-plane RGB with an
/// optional DRM-modifier property.
///
/// - `modifiers = Some(&[..])` → a DMA-BUF capable format with the modifier
///   property (mandatory). When `fixate` is set, the modifier is pinned (second
///   pass).
/// - `modifiers = None` → the SHM-fallback format with no modifier property at all.
fn build_enum_format(modifiers: Option<&[u64]>, fixate_to: Option<u64>) -> Object {
    // Common video properties shared by every variant we offer.
    let mut obj = object! {
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            // default + alternatives: single-plane packed RGB only (the #5 Vulkan
            // import is single-plane; do NOT offer NV12 / multi-plane here).
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width: 1920, height: 1080 },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 8192, height: 8192 }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 60, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 1000, denom: 1 }
        ),
    };

    // The modifier property must come AFTER the format properties in the object.
    if let Some(modifiers) = modifiers {
        obj.properties
            .push(video_modifier_property(modifiers, fixate_to));
    }

    obj
}

/// Build the `SPA_PARAM_Buffers` object requesting DMA-BUF (or, for the SHM
/// fallback, mem-fd / mem-ptr) single-block buffers.
fn build_buffers_param(dmabuf: bool) -> Object {
    let data_type_mask: i32 = if dmabuf {
        1 << SPA_DATA_DMA_BUF
    } else {
        (1 << SPA_DATA_MEM_FD) | (1 << SPA_DATA_MEM_PTR)
    };

    // `SPA_PARAM_BUFFERS_dataType` is a *flags* choice of Int (a bitmask of
    // allowed memory types). `SPA_PARAM_BUFFERS_blocks` = 1 (single plane).
    Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property {
                key: SPA_PARAM_BUFFERS_BLOCKS,
                flags: PropertyFlags::empty(),
                value: Value::Int(1),
            },
            Property {
                key: SPA_PARAM_BUFFERS_DATA_TYPE,
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: data_type_mask,
                        flags: vec![data_type_mask],
                    },
                ))),
            },
        ],
    }
}

/// Does the negotiated `Format` pod still carry an unfixated (`DONT_FIXATE`)
/// modifier property? If so we must run the second fixation pass.
fn modifier_needs_fixation(param: &Pod) -> bool {
    let Ok(obj) = param.as_object() else {
        return false;
    };
    let key = Id(FormatProperties::VideoModifier.as_raw());
    match obj.find_prop(key) {
        Some(prop) => prop
            .flags()
            .contains(pw::spa::pod::PodPropFlags::DONT_FIXATE),
        None => false,
    }
}

/// Extract the DRM modifiers the server actually proposed in its (unfixated)
/// `Format` pod. `VideoInfoRaw::modifier()` returns 0 for an unfixated choice, so
/// we must read the raw `VideoModifier` choice values and pin one of *those*.
fn modifier_choices(param: &Pod) -> Vec<u64> {
    // The value pod is server-controlled and libspa's choice parser does unchecked
    // length arithmetic, so a malformed pod can panic. A panic unwinding across the
    // PipeWire C callback boundary would abort the process — contain it to an empty list.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        modifier_choices_inner(param)
    }))
    .unwrap_or_else(|_| {
        tracing::warn!("panic parsing server modifier-choice pod; ignoring");
        Vec::new()
    })
}

fn modifier_choices_inner(param: &Pod) -> Vec<u64> {
    let Ok(obj) = param.as_object() else {
        return Vec::new();
    };
    let Some(prop) = obj.find_prop(Id(FormatProperties::VideoModifier.as_raw())) else {
        return Vec::new();
    };
    match PodDeserializer::deserialize_from::<Value>(prop.value().as_bytes()) {
        Ok((_, Value::Choice(ChoiceValue::Long(Choice(_, choice))))) => match choice {
            ChoiceEnum::Enum {
                default,
                alternatives,
            } => std::iter::once(default)
                .chain(alternatives)
                .map(|m| m as u64)
                .collect(),
            ChoiceEnum::Flags { default, flags } => std::iter::once(default)
                .chain(flags)
                .map(|m| m as u64)
                .collect(),
            ChoiceEnum::None(m)
            | ChoiceEnum::Range { default: m, .. }
            | ChoiceEnum::Step { default: m, .. } => vec![m as u64],
        },
        Ok((_, Value::Long(m))) => vec![m as u64],
        _ => Vec::new(),
    }
}

/// Run the diagnostic capture probe: connect to the PipeWire remote behind `fd`,
/// negotiate a DMA-BUF format on `node_id`, and log the first [`FRAMES_TO_LOG`]
/// DMA-BUF frames before returning.
///
/// Blocks (runs the PipeWire main loop) until enough frames are seen or the
/// stream errors. Must be called on a thread that owns the `fd`; keep the portal
/// `Session` (and any tokio runtime) alive for the whole call.
pub fn run_capture_probe(node_id: u32, fd: std::os::fd::OwnedFd) -> Result<(), CaptureError> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| CaptureError::PipeWire(format!("create main loop: {e}")))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|e| CaptureError::PipeWire(format!("create context: {e}")))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| CaptureError::PipeWire(format!("connect_fd to portal remote: {e}")))?;

    let stream = pw::stream::StreamRc::new(
        core,
        "rewynd-capture-probe",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| CaptureError::PipeWire(format!("create stream: {e}")))?;

    // Modifiers we advertise, explicit ones FIRST then DRM_FORMAT_MOD_INVALID as a
    // fallback. NVIDIA + KWin only allocates a DMA-BUF when it can intersect an
    // *explicit* modifier the GPU supports; offering INVALID alone yields empty
    // buffers. We query the GPU's supported modifiers for B8G8R8A8 via Vulkan
    // (VK_EXT_image_drm_format_modifier); the server picks one in fixation.
    let mut modifiers = super::query_drm_format_modifiers().unwrap_or_default();
    modifiers.push(DRM_FORMAT_MOD_INVALID);
    tracing::info!(
        count = modifiers.len(),
        "advertising DRM modifiers to PipeWire (explicit + INVALID fallback)"
    );

    let user_data = UserData {
        format: VideoInfoRaw::default(),
        fixated: false,
        fixation_attempts: 0,
        frames_logged: 0,
        non_dmabuf_seen: 0,
        main_loop: main_loop.clone(),
    };

    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(|_stream, _ud, old, new| {
            tracing::info!(?old, ?new, "stream state changed");
        })
        .param_changed({
            let modifiers = modifiers.clone();
            move |stream, ud, id, param| {
                let Some(param) = param else { return };
                if id != ParamType::Format.as_raw() {
                    return;
                }

                let (media_type, media_subtype) = match format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                    return;
                }

                if let Err(e) = ud.format.parse(param) {
                    tracing::error!(error = ?e, "failed to parse negotiated video format");
                    return;
                }

                let fmt = ud.format.format();
                let size = ud.format.size();
                let modifier = ud.format.modifier();
                tracing::info!(
                    format = ?fmt,
                    width = size.width,
                    height = size.height,
                    modifier = format_args!("{modifier:#018x}"),
                    "negotiated video format"
                );

                // Two-pass modifier fixation: if the server handed back a format
                // whose modifier property is still unfixated (DONT_FIXATE), we must
                // re-emit an EnumFormat pinning exactly one modifier and return; the
                // next param_changed will be fully fixated.
                if !ud.fixated && modifier_needs_fixation(param) {
                    ud.fixation_attempts += 1;
                    if ud.fixation_attempts > MAX_FIXATION_ATTEMPTS {
                        tracing::error!(
                            attempts = ud.fixation_attempts,
                            "modifier fixation did not converge; giving up"
                        );
                        ud.main_loop.quit();
                        return;
                    }
                    let offered = modifier_choices(param);
                    let offered_hex: Vec<String> =
                        offered.iter().map(|m| format!("{m:#018x}")).collect();
                    // Prefer an explicit modifier the server proposed; fall back to its
                    // first choice, then to INVALID. (#5 will intersect this with the set
                    // the Vulkan device can actually import.)
                    let chosen = offered
                        .iter()
                        .copied()
                        .find(|&m| m != DRM_FORMAT_MOD_INVALID)
                        .or_else(|| offered.first().copied())
                        .unwrap_or(DRM_FORMAT_MOD_INVALID);
                    tracing::info!(
                        ?offered_hex,
                        chosen = format_args!("{chosen:#018x}"),
                        "modifier unfixated; pinning a server-proposed modifier (pass 2)"
                    );
                    let fixed = serialize_object(build_enum_format(Some(&modifiers), Some(chosen)));
                    let Some(pod) = Pod::from_bytes(&fixed) else {
                        tracing::error!("failed to build fixated EnumFormat pod");
                        return;
                    };
                    if let Err(e) = stream.update_params(&mut [pod]) {
                        tracing::error!(error = %e, "update_params (fixate) failed");
                    }
                    return;
                }

                // Format is fixated. Decide whether we got a DMA-BUF-capable modifier
                // (anything other than "no modifier" implies the dmabuf path here) and
                // request the matching buffer memory type.
                ud.fixated = true;
                let want_dmabuf = ud
                    .format
                    .flags()
                    .contains(pw::spa::param::video::VideoFlags::MODIFIER);
                tracing::info!(
                    want_dmabuf,
                    "fixated; requesting Buffers param (dataType {})",
                    if want_dmabuf {
                        "DmaBuf"
                    } else {
                        "MemFd|MemPtr (SHM fallback)"
                    }
                );
                let buffers = serialize_object(build_buffers_param(want_dmabuf));
                let Some(pod) = Pod::from_bytes(&buffers) else {
                    tracing::error!("failed to build Buffers pod");
                    return;
                };
                if let Err(e) = stream.update_params(&mut [pod]) {
                    tracing::error!(error = %e, "update_params (buffers) failed");
                }
            }
        })
        .process(move |stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                tracing::warn!("process: out of buffers");
                return;
            };
            let datas = buffer.datas_mut();
            if datas.len() != 1 {
                tracing::warn!(
                    planes = datas.len(),
                    "rejecting multi-plane buffer (#4 is single-plane only)"
                );
                return;
            }

            let data = &mut datas[0];
            let data_type = data.type_();
            let size = ud.format.size();
            let fourcc = spa_format_to_drm_fourcc(ud.format.format());

            if data_type == pw::spa::buffer::DataType::DmaBuf {
                let chunk = data.chunk();
                // Skip empty/placeholder DMA-BUF buffers (size 0) — common at stream
                // start/suspend; counting them could meet the quota with no pixels.
                if chunk.size() == 0 {
                    tracing::debug!("skipping empty DMA-BUF buffer (chunk size 0)");
                    return;
                }
                let frame = DmabufFrame {
                    fd: i64::from(data.fd()),
                    fourcc: fourcc.unwrap_or(0),
                    modifier: ud.format.modifier(),
                    width: size.width,
                    height: size.height,
                    stride: chunk.stride(),
                    offset: chunk.offset() as i32,
                    num_planes: 1,
                };
                tracing::info!(
                    fd = frame.fd,
                    fourcc = format_args!("{:#010x}", frame.fourcc),
                    modifier = format_args!("{:#018x}", frame.modifier),
                    width = frame.width,
                    height = frame.height,
                    stride = frame.stride,
                    offset = frame.offset,
                    "DMA-BUF frame"
                );

                ud.frames_logged += 1;
                if ud.frames_logged >= FRAMES_TO_LOG {
                    tracing::info!(
                        frames = ud.frames_logged,
                        "logged enough DMA-BUF frames; quitting"
                    );
                    ud.main_loop.quit();
                }
            } else {
                // Not DMA-BUF: the negotiation fell back to SHM (or empty buffers).
                // Log the first few so the buffer type is clear, then bail rather
                // than spam thousands of lines / capture the screen indefinitely.
                ud.non_dmabuf_seen += 1;
                if ud.non_dmabuf_seen <= NON_DMABUF_LOG_LIMIT {
                    tracing::warn!(
                        ?data_type,
                        size = data.chunk().size(),
                        "received NON-dmabuf buffer; the dmabuf negotiation did not take"
                    );
                }
                if ud.non_dmabuf_seen >= NON_DMABUF_ABORT_AFTER {
                    tracing::error!(seen = ud.non_dmabuf_seen, "no DMA-BUF buffers; giving up");
                    ud.main_loop.quit();
                }
            }
        })
        .register()
        .map_err(|e| CaptureError::PipeWire(format!("register stream listener: {e}")))?;

    // Build the initial EnumFormat params: a DMA-BUF-capable format (with the
    // modifier choice) first, then a no-modifier SHM fallback. The server picks
    // the best it can satisfy.
    let dmabuf_format = serialize_object(build_enum_format(Some(&modifiers), None));
    let shm_format = serialize_object(build_enum_format(None, None));
    let mut params = [
        Pod::from_bytes(&dmabuf_format)
            .ok_or_else(|| CaptureError::PipeWire("invalid dmabuf EnumFormat pod".to_owned()))?,
        Pod::from_bytes(&shm_format)
            .ok_or_else(|| CaptureError::PipeWire("invalid SHM EnumFormat pod".to_owned()))?,
    ];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            // NB: AUTOCONNECT only — NO MAP_BUFFERS (that forces SHM).
            pw::stream::StreamFlags::AUTOCONNECT,
            &mut params,
        )
        .map_err(|e| CaptureError::PipeWire(format!("connect stream: {e}")))?;

    tracing::info!(node_id, "stream connected; entering main loop");
    main_loop.run();
    tracing::info!("main loop exited");

    Ok(())
}
