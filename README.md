# rewynd

A lightweight, native, cross-platform (**Linux + Windows**) "instant replay" clip
recorder for gameplay. It continuously keeps the **last 60 seconds** of the screen in a
GPU-encoded ring buffer; on a hotkey it flushes that buffer to an MP4. The frame stays
on the GPU from capture through hardware H.264 encode (zero-copy), so it's light enough
to run while gaming.

See [`PLAN.md`](PLAN.md) for the full design and rationale, and
[`docs/adr/`](docs/adr) for architecture decisions.

## Install (beta)

rewynd is in **beta** (`v1.0.0-beta.1`). Builds are unsigned, so Windows SmartScreen
(and occasionally antivirus) may warn on first run — choose **More info → Run anyway**.

**Linux** — any distro, via the self-updating AppImage:

```sh
curl -fsSL https://raw.githubusercontent.com/gankedtv/rewynd/main/install.sh | sh
```

Or grab `rewynd.AppImage` from the [latest release](https://github.com/gankedtv/rewynd/releases),
`chmod +x rewynd.AppImage`, and run it.

**Windows** — download `rewynd-win-Setup.exe` from the
[latest release](https://github.com/gankedtv/rewynd/releases) and run it (per-user, no admin).

The app checks for updates on launch; one click updates both binaries in place.

## Workspace layout

| Crate | Role |
| --- | --- |
| [`rewynd-capture`](crates/capture) | `FrameSource` trait + per-platform GPU-resident capture (Linux: portal + PipeWire → DMABUF; Windows: WGC/DXGI → D3D11). |
| [`rewynd-gpu`](crates/gpu) | Shared `wgpu` device/queue and the capture-import interop helpers. |
| [`rewynd-encode`](crates/encode) | Thin wrapper over `gpu-video` (NV12 `wgpu::Texture` → H.264) + RGBA→NV12. |
| [`rewynd-buffer`](crates/buffer) | Keyframe-aware ring buffer — the pure-Rust core, no GPU dependency. |
| [`rewynd-mux`](crates/mux) | H.264 Annex-B → MP4 with real PTS. |
| [`rewynd-upload`](crates/upload) | Upload clients: ganked.tv (API key) and YouTube (OAuth). |
| [`rewynd-recorder`](crates/app) | The background recorder: wires the pipeline, hotkey, config, tray. |
| [`rewynd`](crates/settings) | The GUI you launch: clip library + settings editor (iced). |

## Building

Requires a recent stable Rust (edition 2024) and a C++ compiler (for `vk-mem`). On
Linux the capture crate additionally needs PipeWire dev headers and libclang (for
`pipewire-sys`/`libspa-sys` bindgen): on Debian/Ubuntu, `pkg-config
libpipewire-0.3-dev clang libclang-dev`.

```sh
cargo build
cargo test
cargo run -p rewynd            # the GUI: clip library + settings
cargo run -p rewynd-recorder   # the background recorder
```

The GPU stack (`wgpu` + `gpu-video`) is **pinned to a single source** — see
[`docs/adr/0001-wgpu-rev.md`](docs/adr/0001-wgpu-rev.md). Don't bump `wgpu` or
`gpu-video` without updating that ADR.

## License

GPL-3.0-or-later (see [`LICENSE`](LICENSE)). Third-party attribution notices are in
[`LICENSES/`](LICENSES).
