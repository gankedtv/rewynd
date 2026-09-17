# ADR 0022: Windows system audio through process loopback

## Status

Accepted (issue #215).

## Context

Windows system audio was a WASAPI loopback on the *console default* render endpoint,
resolved once at startup (ADR 0002's WASAPI choice; #210 added a second loopback on the
communications default and a picker for an explicit output). A set of users still got
clips without system sound, some even after picking their output by hand, while the
same builds worked elsewhere. No logs exist from those machines: the installed recorder
has no console and writes no log file.

An endpoint loopback hears exactly one endpoint's mix, and only what reaches it. The
code allowed three ways for that to be silent without an error:

- **Routing.** Audio that plays anywhere else never reaches the captured endpoint: the
  default changed after the recorder started (a headset powering on, Bluetooth, HDMI
  audio waking), a per-app output in the volume mixer, the game's own device setting,
  a virtual mixer (SteelSeries Sonar, Nahimic, VoiceMeeter) sitting in front of the
  hardware, a Bluetooth headset switching to its hands-free profile when the mic opens.
  WASAPI streams do not follow default-device changes; only apps that re-open on
  `IMMNotificationClient::OnDefaultDeviceChanged` do.
- **Stream death.** Any WASAPI error (`AUDCLNT_E_DEVICE_INVALIDATED` on unplug or
  default change, `E_ACCESSDENIED` under the microphone privacy policy) ended the capture
  thread for the rest of the session. The toast that reported it is hidden by Windows'
  automatic Do Not Disturb while a game is fullscreen.
- **Engine-side conversion.** `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` has been seen to
  deliver all-zero samples with a successful `Initialize` (cpal #1200, Windows 11 24H2
  communications-class endpoints; loopback on ARM64 delivering no packets at all).

## Decision

**System audio defaults to process loopback.** `ActivateAudioInterfaceAsync` on the
`VAD\Process_Loopback` virtual device with `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE`
on rewynd's own pid captures every process's render streams *before* they reach an
endpoint, whichever endpoint each plays to. That removes the routing class entirely and
has no device to be invalidated; the format is the one asked for (48 kHz stereo f32),
converted per stream by the engine without `AUTOCONVERTPCM`. Windows 10 2004 or later;
on older builds the activation fails and the capture falls back to the console
default's endpoint loopback, with the communications-endpoint extra of #210 next to it
(`process_loopback_supported()` decides that in the recorder). An explicit output pick
keeps the endpoint loopback on that device: the user asked for one output.

The stream is polled, not event-driven, the same loop as the endpoint path; the
process-loopback client delivers no packets while nothing renders, which the mixer
zero-fills as before.

**A failed capture is reopened, not abandoned.** The shared audio pipeline retries a
capture that errors, with a backoff from one to thirty seconds, until shutdown. The
first failure of an outage logs at the source's severity and reports "lost" to the
platform; the first audio a reopened stream delivers reports "restored", so a tray or
toast that said the sound was gone gets corrected. Later failures of the same outage log
at debug; each reopened stream announces itself at info. This covers every platform's
capture, since a background recorder that runs from login has to outlive a device
coming and going.

The support decision is made once per process by a trial activation and then fixed
(`process_loopback_supported()`), so the recorder's plan (whether to add the
communications capture) and the capture thread's path always agree: on a supported box
a later activation failure is retried on the same path, never a silent switch to an
endpoint loopback without the extra.

**The activation blob is never dropped as a `PROPVARIANT`.** The `windows` crate clears
a `PROPVARIANT` on drop, and clearing a `VT_BLOB` frees its data pointer, which here is
the activation parameters on the stack: a heap corruption that took an afternoon to find.
The blob is held in `ManuallyDrop`.

## Consequences

- Process loopback also captures audio the user is not hearing: an app rendering to a
  muted or disconnected output is in the clip. For a game clip that is the better
  failure than silence.
- The communications-endpoint capture only runs on the fallback path, so the doubled-mix
  concern of #213 cannot arise where process loopback works.
- Clip levels follow per-app volume, not the master volume: process loopback taps the
  streams before the endpoint's volume. Endpoint loopback was already before the master
  volume on most devices.
- The retry loop applies to the microphone too: a mic plugged in after startup is picked
  up at the next attempt instead of never.
- The installed recorder now writes a log file as well (`RotatingLog`: three files of at
  most 2 MB under the platform's data dir), with a per-minute peak-level line per capture,
  so the next "no system sound" report carries evidence instead of a guess.
