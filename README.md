<p align="center">
  <img src="docs/design/logo.svg" width="140" alt="rewynd logo">
</p>

<h1 align="center">rewynd</h1>

<p align="center">
  Instant replay for your gameplay. The last 60 seconds, always one hotkey away.
</p>

<p align="center">
  <a href="https://github.com/gankedtv/rewynd/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/gankedtv/rewynd/ci.yml?branch=main&label=CI" alt="CI status"></a>
  <a href="https://github.com/gankedtv/rewynd/releases"><img src="https://img.shields.io/github/v/release/gankedtv/rewynd?include_prereleases&label=release" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/gankedtv/rewynd" alt="License"></a>
</p>

rewynd is a lightweight, native clip recorder for **Linux, Windows and macOS**
(Apple Silicon, macOS 15+). It continuously
keeps the last 60 seconds (configurable) of the screen in a GPU-encoded ring buffer; on a hotkey it
flushes that buffer to an MP4. The frame stays on the GPU from capture through hardware
H.264 encode (zero-copy), so it is light enough to run while gaming.

See [`PLAN.md`](PLAN.md) for the full design and rationale, and
[`docs/adr/`](docs/adr) for architecture decisions.

## Install (beta)

rewynd is in **beta**. Builds are currently unsigned, so Windows SmartScreen (and
occasionally antivirus) may warn on first run: choose **More info**, then **Run
anyway**. Code signing via the SignPath Foundation is being set up; see the
[code signing policy](docs/code-signing-policy.md).

**Linux**, any distro, via the self-updating AppImage:

```sh
curl -fsSL https://raw.githubusercontent.com/gankedtv/rewynd/main/install.sh | sh
```

Or grab `rewynd.AppImage` from the [latest release](https://github.com/gankedtv/rewynd/releases),
`chmod +x rewynd.AppImage`, and run it.

**Windows**: download `rewynd-win-Installer.exe` from the
[latest release](https://github.com/gankedtv/rewynd/releases) and run it (per-user, no
admin rights needed). Prefer a one-click unattended install? `rewynd-win-Setup.exe` is
the same install without the window.

**macOS** (Apple Silicon, macOS 15+), via the same self-updating installer:

```sh
curl -fsSL https://raw.githubusercontent.com/gankedtv/rewynd/main/install.sh | sh
```

It installs `rewynd.app` to `~/Applications` and clears the quarantine flag, so the
unsigned build opens directly — launch it from Spotlight or Launchpad. Prefer to do it
by hand? Download `rewynd-osx-Portable.zip` from the
[latest release](https://github.com/gankedtv/rewynd/releases), unzip it, and move
`rewynd.app` to `~/Applications`; a manual download stays quarantined, so its first launch
needs **right-click → Open** (Gatekeeper offers no Open button on a plain double-click).
Either way macOS asks for Screen Recording permission on first run — grant it and
relaunch. Because unsigned builds get a fresh identity on every update, macOS asks for
that permission again after each update; a Developer ID signature (planned) fixes that.

The app checks for updates on launch; one click updates both binaries in place.

## What you get

- A background recorder that sits in the tray and stays out of the way while you play.
- A clip library and settings app: browse, trim, and manage your clips.
- A first-run wizard that sets up capture, the hotkey, and the replay length in a
  minute.
- Optional uploads to [ganked.tv](https://ganked.tv) or YouTube, only when you ask.
  Clips never leave your machine on their own.

## Logs

The recorder writes what it is doing to a small set of log files next to its data, capped at
2 MB each and three files in total, so they never grow beyond 6 MB:

- Windows: `%LOCALAPPDATA%ewyndogsewynd-recorder.log`
- Linux: `$XDG_DATA_HOME/rewynd/logs/rewynd-recorder.log` (usually `~/.local/share/rewynd/logs/`)
- macOS: `~/Library/Application Support/rewynd/logs/rewynd-recorder.log`

Attach that file to a bug report about clips with no sound, a game that is not picked up,
or a recorder that stops: it says which audio path ran, whether audio was flowing (a
`peak` line a minute), and what the capture saw.

## Workspace layout

| Crate | Role |
| --- | --- |
| [`rewynd-capture`](crates/capture) | `FrameSource` trait + per-platform GPU-resident capture (Linux: portal + PipeWire into DMABUF; Windows: WGC/DXGI into D3D11; macOS: ScreenCaptureKit into IOSurface). |
| [`rewynd-gpu`](crates/gpu) | Shared `wgpu` device/queue and the capture-import interop helpers. |
| [`rewynd-encode`](crates/encode) | Thin wrapper over `gpu-video` (NV12 `wgpu::Texture` to H.264) + RGBA-to-NV12 conversion. |
| [`rewynd-buffer`](crates/buffer) | Keyframe-aware ring buffer; the pure-Rust core, no GPU dependency. |
| [`rewynd-mux`](crates/mux) | H.264 Annex-B to MP4 with real PTS. |
| [`rewynd-upload`](crates/upload) | Upload clients: ganked.tv (API key) and YouTube (OAuth). |
| [`rewynd-recorder`](crates/app) | The background recorder: wires the pipeline, hotkey, config, tray. |
| [`rewynd`](crates/settings) | The GUI you launch: clip library + settings editor (iced). |

## Building

Requires a recent stable Rust (edition 2024), a C++ compiler (for `vk-mem`) and `cmake`
(the Opus encoder builds libopus from source and links it statically, so nothing needs
libopus at runtime). On Linux the capture crate additionally needs PipeWire dev headers
and libclang (for `pipewire-sys`/`libspa-sys` bindgen): on Debian/Ubuntu, `pkg-config
libpipewire-0.3-dev clang libclang-dev cmake`. On macOS (Apple Silicon, macOS 15+) the
Xcode Command Line Tools are the only other extra — capture and encode use the system
ScreenCaptureKit/VideoToolbox frameworks. In-app clip playback in the library decodes
through `ffmpeg`: the Linux and Windows installers bundle a copy beside the binaries,
while dev builds and macOS use whatever `PATH` provides (`brew install ffmpeg` on macOS).

```sh
cargo build
cargo test
cargo run -p rewynd            # the GUI: clip library + settings
cargo run -p rewynd-recorder   # the background recorder
```

The GPU stack (`wgpu` + `gpu-video`) is **pinned to a single source**; see
[`docs/adr/0001-wgpu-rev.md`](docs/adr/0001-wgpu-rev.md). Don't bump `wgpu` or
`gpu-video` without updating that ADR.

## License

GPL-3.0-or-later (see [`LICENSE`](LICENSE)). Third-party attribution notices are in
[`LICENSES/`](LICENSES).
