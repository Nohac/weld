# Application audio

Weld uses PipeWire for Linux application audio. PulseAudio applications work
through `pipewire-pulse`; a standalone PulseAudio server is not a supported
backend. WirePlumber supplies normal stream linking policy.

## Launch and capture

`weldctl launch --headless --control -- PROGRAM` creates a private virtual audio
output before starting the application. `--private-audio` requests the same
routing for an ordinary attributed launch. Private audio is incompatible with
`--shared-display`. A failed private-output setup rejects that launch before
starting the process. The rest of the desktop remains usable.

The presentation-free `weld --backend headless` assembly also supplies a private
session output to its startup command and subsequent launches. If PipeWire is
unavailable, it retains an unavailable private target in launch defaults, logs
the audio failure, and continues hosting video.

Native clients receive `PIPEWIRE_NODE` and `PIPEWIRE_PROPS`; Pulse-compatible
clients receive `PULSE_SINK` and `PULSE_PROP`. The output is never linked to
hardware. These are application routing defaults, not an audio security sandbox:
applications that select a different device explicitly, reuse an existing
process, discard their environment, or access hardware directly can bypass them.
Weld does not change the desktop's default device or volume.

Private outputs follow the session-lifetime attributed launch listener. This
allows delayed descendants to open audio after the original launcher exits.
They close with their owning Weld session; the existing 256-launch limit also
bounds private outputs. Capture/playback connections close when their last
consumer or mapping is released.

Recording audio is opt-in:

```sh
weldctl window record --control CONTROL_ID \
  --options '{"duration_ms":10000,"include_audio":true}' --json
```

`record_input` accepts the same option. `include_audio` and `audio_packet_count`
in recording information distinguish requested audio from actual stored packets.
A silent application with no playback streams can finish with zero audio packets.
Export adds one mixed stereo audio track when packets exist; snapshots and
contact sheets remain images. Explicit exports should use persistent disk-backed
paths, not `/tmp` or another tmpfs.

Audio belongs to an application, not a Wayland surface. An authorized window's
recording or stream can include sound shared with that application's other
windows. Matching prefers the output owned by the Wayland launch. Ordinary
attributed launches carry the same listener launch ID in a host-qualified
`weld.launch.id` property, inherited by native PipeWire and Pulse-compatible
clients and their audio helpers. Weld adds it before spawning, preserves existing
audio properties/routing, and replaces the tag for nested launches. PipeWire does
not infer this identity from the Wayland socket: Weld explicitly bridges the two.

Discovery reads the tag from the stream, or its owning client when absent on the
stream. A conflicting stream tag takes precedence over the client's tag. Missing
or mismatched launch tags remain unmatched; an attributed capture never broadens
to PID matching. Private-output capture still works for applications that omit
properties but obey their output routing.

Windows without launch attribution use an exact PID match from the stream or
its owning client. There is no `/proc` read or parent-process walk. Untagged helper
processes with a different PID, applications reusing an existing process, and
clients that discard the launch environment cannot be associated automatically.
These identifiers are association hints, not process-lifetime handles or security
credentials; names and application IDs are never broad capture fallbacks.

## Streaming

Linux encoded hoist sources capture the applications behind authorized mapped
surfaces. Multiple windows from the same attributed launch share one capture per
peer. Recording and streaming subscriptions to the same target share the actual
PipeWire capture and have independent bounded queues.

Iroh transports PCM audio in complete timestamped QUIC datagrams, independently
of reliable control and video streams. Local encoded transport uses its bounded
Unix control connection. Linux receivers play through PipeWire's normal output;
no microphone capture or reverse microphone forwarding is enabled.

The initial format is 48 kHz stereo signed 16-bit PCM: 192 kB/s (1.536 Mbit/s)
per audio stream before transport overhead. This implementation does not add an
Opus encoder or Android playback backend. Non-Linux receiver assemblies retain
video support but do not play these audio packets. Peers must be rebuilt together
for hoist protocol revision 2.

Capture timestamps use PipeWire's monotonic buffer/graph timing, mapped onto the
source media clock or each recording's origin. Recording export preserves those
offsets and fills gaps with silence. Live playback uses a bounded startup jitter
buffer (up to 40 ms), respects timestamp gaps, discards stale samples and limits
queued audio to 160 ms. Video retains its existing immediate presentation policy;
this is not a measured or guaranteed end-to-end lip-sync contract under variable
video/network latency.

## Ownership and limits

- `weld-audio` owns PipeWire workers, stream discovery, private outputs, shared
  capture subscriptions, and receiver playback. It has no Bevy or Smithay
  dependency. Audio callbacks run on ordinary worker loops, outside the real-time
  PipeWire thread. Native host wakeups service captured packets without requesting
  a Bevy redraw. Capture links are passive, and drained playback suspends
  after 200 ms; new packets wake it without an idle polling timer.
- `weld-media::audio` owns the bounded PCM packet contract. A packet is at most
  32 KiB, with complete stereo sample frames and a validated timestamp range.
- Hoist ports own capture/playback lifetime under mapped-surface authorization.
  Late datagrams cannot recreate playback after withdrawal. Sources limit capture
  targets to 16 per peer; each target discovers at most 16 playback streams.
- Each capture subscriber queues at most 32 packets. A stalled live consumer loses
  its own packets without blocking other consumers. A recording reports failure
  if its capture or storage cannot keep up, rather than silently hiding loss.
- `weld-recording` spools audio through the existing bounded storage worker and
  applies the shared 2 GiB clip budget. Export mixes streams on temporary file
  storage, keeping sample memory bounded. Existing recording retention and grant
  revocation apply to audio too.

## Validation

Run in the shared Rust development shell:

```sh
cargo test -p weldwm -p weld-audio --lib
cargo test -p weldwm -p weld-audio --test pipewire -- --ignored
cargo test -p weldwm -p weld-recording --test media -- --ignored
cargo test -p weldwm -p weld-hoist-iroh --lib direct_hosts_exchange_independent_control_and_media
scripts/check-window-recording
```

The PipeWire tests use a private output for generated tones. The launch-routing
test plays silence with both `pw-play` and `paplay` and inspects their actual
links and capture child streams by launch tag while rejecting another launch.
Other assertions cover exact PID/client discovery, fanout, timestamp progress,
consumer backpressure, recorded stream mixing and offsets, and audio datagram
delivery with a zero video receive budget.
