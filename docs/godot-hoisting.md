# Godot live-window tracer

Godot can receive live AV1 window hierarchies from headless Weld on Linux or Android.
This is a bounded multi-window viewer, with desktop input and a first
[right-hand XR pointer](godot-xr.md#right-hand-input). The
[XR scene](godot-xr.md) can present the same stream on a headset. It uses
the same `weld-hoist-iroh` transport, `weld-hoist-encoded` scheduling and
`weld-media::decode::DecodePool` as the compositor receiver. No new wire protocol,
application commit ACK, raw-frame upload or CPU pixel readback is introduced.

## Run

Build/export the current debug APK and install without clearing app data:

```sh
apps/weld-vr/scripts/build-gdextension android
mkdir -p apps/weld-vr/build
godot --headless --path apps/weld-vr --export-debug 'Android Phone' \
  "$PWD/apps/weld-vr/build/weld-vr-debug.apk"
adb -s DEVICE install -r apps/weld-vr/build/weld-vr-debug.apk
scripts/run-godot-hoist --serial DEVICE
```

Use an ARM64/API28+ USB-debugging device. This uses development signing and
`run-as`, not release enrollment. Exactly one authorized device can omit
`--serial`. `--desktop` runs the same viewer on the laptop. Default: one foot/htop
window at 960x640 for 120 seconds; `--seconds` changes the test timer.
`--app blender` exercises its main window, Preferences and menus/tooltips.
On desktop, click the image to focus it, then use mouse buttons, wheel and
physical keyboard keys. The source uses compositor-owned explicit repeats,
with emulated repeats for legacy clients. XR uses the right-hand controller
pointer; the flat phone viewer remains view-only.
Window creation, removal and replacement share the same connection. Flat
independent windows currently tile side by side; flat mode has no shell close or
move controls yet. XR has local close/drag controls; see
[XR window layout](godot-xr.md#window-layout-and-decoration).

`--bitrate-mbps 8|16|24` selects the shared AV1 encoder target, default 16 Mbps
for this demo. The same target is retained across diagnostic source restarts.
It is not a per-window rate, actual bandwidth cap or change to codec limits.

Iroh N0 contacts Internet discovery/relay services. No host network interfaces,
routes or existing desktop apps are changed. The launcher reuses the tested
headless launcher's process-group supervisor. It owns/stops its source and demo
children, and restarts/stops only `com.example.weldvr` on the phone. Logs go to
fresh `target/validation/godot-hoist-*` directories; phone log snapshots are
bounded and restricted to the recorded viewer PID. Core/video dumps are disabled.

`--restart-after 20 --seconds 75` restarts the source **and its demo apps** once,
keeping the viewer process alive and the source identity unchanged. It tests
rediscovery and new connection generations, not preservation of running apps
through a source-process restart.

## Azahar stereo experiment

`scripts/run-azahar-xr --show-manager --seconds 180` builds/deploys the Pico
viewer and launches the configured game. Supply `--rom PATH` to override the
local default. Configure Azahar for OpenGL, Separate Windows, full-width
side-by-side stereo, nonzero 3D depth, 2x resolution and Single Window Mode off.
The launcher refuses a competing Azahar process and does not modify ROMs or
save states. With `--show-manager`, load slot 1 manually through the emulator's
Emulation menu; save-state loading is not automated.

Generic bounded app-ID/title metadata now crosses the hoist protocol. The
unreleased development revision is reset to 1; peers must use matching builds,
not merely matching revision numbers. Identical labels are deduplicated and pending updates
coalesced rather than retransmitted with every video frame. These labels are
presentation hints, not authenticated application identity.

The launcher supplies local selection rules for Azahar's Primary and Secondary
windows: a 1600x480 packed stereo upper screen and a separate 640x480 mono
touchscreen. Two eye-specific native OpenXR layers sample the same decoded
upper-screen image; no second decoder or CPU pixel copy is introduced. This is
explicit packed-stereo interpretation, not a new Wayland stereo extension.
Attached content retains its owner's placement and close/drag controls. The
shared bitrate target defaults to 16 Mbps across all windows.

The one-shot rules file is JSON with an ordered `rules` array. Each entry names
`app_id`, `title_suffix`, `stereo`, `width`, `height`, `slot` and an optional
`bitrate` object containing `group` and `role`. Rust deserializes through Serde
into validated rules; the old positional text format and header are gone.
First-match selection and the manager catch-all-last ordering are unchanged.
The 4096-byte/eight-rule bounds and label, extent, stereo and slot checks remain.
Tests exercise the actual Python producer against the Rust consumer, matching
and Weld-owned limits rather than Serde round trips.

The launcher assigns the upper screen, touchscreen and optional manager to one
explicit bitrate group with primary, companion and utility roles. Interaction
boosts their shared entitlement; it does not transfer the upper screen's share
to the touchscreen. The allocator knows no emulator names. See
[shared encoder targets](shared-bitrate-targets.md#allocation-and-churn).
The 2026-09-20 three-minute 8 Mbps Pico run `godot-hoist-_00kly0n` completed
without disconnecting and confirmed the transported group/role hints. Tests
cover stable targets across member focus changes, authorization, reset, codec
bounds and frozen jobs; the run did not establish subjective focus-switch quality.

Stream selection is independent of mapped/presentable state: a selected window
must keep receiving frames before its first image and after temporary unmapping.
Refresh-rate updates previously reused mapped visibility and could pause a
window before the frame needed to make it visible. The regression test exercises
75/80/85/89/90 Hz startup updates, excluded windows and selected descendants.
Pico run `godot-hoist-6d3ud0qm` on 2026-09-20 kept all three selected Azahar
windows active for the bounded 75-second test; the capture showed both game
screens and decoding continued through the run. The full Rust suite passed
95 tests with strict Clippy. Startup rate samples come from the OpenXR runtime,
not Godot rendering FPS; they do not establish physical panel VRR.

The isolated source keeps access to host PipeWire/PulseAudio runtime sockets
while using a private Wayland runtime. Audio stays on the laptop; it is not
streamed to the headset, and explicit audio environment overrides are preserved.

On 2026-09-19 a three-minute Pico AV1 run (`godot-hoist-5jafw2uo`) decoded about
20,000 frames across five layers without disconnecting. Four adjacent output
swaps were handled by the bounded receiver reorder slots. Godot render-target
cleanup warnings remain in the logs; this does not qualify all stereo lifecycle
or performance behavior.

## Identity and permissions

Laptop state defaults to `${XDG_DATA_HOME:-$HOME/.local/share}/weld/godot-hoist`
(override with `--state-dir`), outside build outputs. The phone creates its own
key in `user://weld-device`, with `public.identity` and `source.profile`. Only
public identity/profile data crosses ADB; the launcher never reads/copies keys.
Files are mode 0600 in a mode-0700 child directory. The launcher refuses silent
re-enrollment to a different pinned source.

The APK declares `INTERNET`, granted without a runtime dialog. Private app data
needs no broad storage permission. Physical testing exposed SELinux denial of
our hard-link-based atomic publisher (`denied { link }`, `untrusted_app_34`,
`app_data_file`). The shared publisher now uses `renameat2(RENAME_NOREPLACE)`:
write/fsync a temporary, atomically publish without overwrite, fsync directory.
Unsupported filesystems fail closed. No SELinux or storage permissions changed.
See [Android app-specific storage guidance](https://developer.android.com/training/data-storage/app-specific).

The Android development viewer explicitly selects `IrohDnsPolicy::Public`.
It configures Google IPv4/IPv6 DNS over UDP/TCP, with the pinned Iroh DNS
implementation's public fallback tier (including Cloudflare and Quad9). This
avoids Iroh's system-DNS probe, which otherwise catches an uninitialized
`ndk-context` panic inside this GDExtension. It **does not honor Android Private
DNS or VPN-provided resolver settings**. This policy only causes DNS traffic for
N0, not Direct connections. The launcher prints the policy; desktop keeps the
unchanged system-aware resolver. Resolver selection is local, never read from a
peer profile. Android platform-resolver integration is deferred; public DNS is
a development limitation, not a production default recommendation.

The policy uses the API shared by `iroh-dns` 1.1 (compositor lock) and 1.3
(standalone shell lock). One narrowly documented deprecation allowance avoids
changing either working dependency graph just to use 1.3's replacement API.

The [shared Iroh identity APIs](iroh-hoisting.md) own key/profile formats and
verified storage. N0 can rediscover a stable identity after rebinding; saved
Direct address hints alone cannot. Cold launch automatically connects to its
saved profile, or displays its public identity and waits for pairing. One profile
is pinned for that producer lifetime; edits require a new start.

## Pipeline and limits

- A coordinator thread owns the shared receiver registration and non-Send client
  leases. Transport, codec completion and returned frame credits unpark it
  independently of Godot's frame cadence. It registers the adapter with
  `ClientRuntime`, including event validation, effects, route aliases, cursor
  feedback and retirement servicing. No new unbounded media queue exists.
- The coordinator exclusively owns the window inventory. Godot clones an
  immutable topology snapshot and consumes bounded per-window handoffs; native
  imports and rendering never hold an inventory lock. Topology and layer epochs
  reject obsolete publications after layout or lifecycle changes. Small
  publication/mailbox locks remain; this is not a lock-free pipeline.
- Android still uses `weld-media-android`: FFmpeg/`ffmpeg-next`, `ndk_codec=1`,
  MediaCodec to acquired native ImageReader buffers. Linux uses existing
  FFmpeg/VA-API. Context construction/calls/destruction stay on pool workers.
- Atomic commit publication creates a one-shot client-lease payload. Only the
  replaced layer transfers its frame into its presentation mailbox. The emptied
  destination-owned client lease cannot free that native allocation; GPU fence
  retirement owns it. Retained commits can update crop without new pixels.
- The existing EGLImage presenter retains texture and material in Rust. Texture
  object size is immutable during a session; native import defines actual storage.
  Panel aspect follows logical content dimensions. Per-request visible extent
  intersects codec crop and content view, excluding AV1 padding, including odd
  1280x833 content in larger coded storage. Unmap/destroy/disconnect clear the view.
- Eight surfaces, eight retained layers and eight active media streams per
  connection; excess inventory fails admission rather than allocating unbounded views.
- Sixteen generations, four jobs, depth two, up to two workers in the existing
  shared decode pool. Affinity keeps a stream's generations on one worker.
- Decoder completion is cooperatively polled, allowing another bounded input
  submission while older output is pending. Android retains the same acquired
  image, fence and fixed deadlines across polls, returning an unfinished image
  with its fence on teardown. Workers use a 1 ms active-work retry when no
  command arrives; idle workers block. Native FFmpeg calls and the VA-API
  completion path can still block. Queue depths and frame-age limits are unchanged.
- Seven frame credits per stream across generations, and 32 total per session,
  cover submitted jobs, completions, publication, mailboxes, GPU imports and
  retirement. Credits return after native release.
  Each Android reader permits eight acquired images. Old outputs may retain old
  readers after decoder retirement, also bounded by the frame credits.
- Visible extent is at most 2048 per dimension and 1920x1080 total pixels. These
  are tracer admission bounds, not probed hardware limits or a parser-enforced
  bound on every possible allocation described by encoded sequence headers.
- No future frame is required for the final output. Three seconds without output
  is an explicit low-delay error, not an empty completion or fabricated EOS.
- Godot refresh is forwarded through existing `SetPresentation`; source pacing
  still clamps it to the encoder ceiling. There is no ACK roundtrip.
- XR also sends bounded logical-size and preferred-scale requests for the
  independent mapped roots, based on stable headset presentation preferences.
  [Sizing and native composition](godot-xr.md#image-quality) preserve the same
  receive allocation ceiling and GPU frame-lifetime rules.
- Stop/pause cancels production and performs GPU cleanup. Relaunch the viewer
  and source after pause. Source relay re-admission after viewer disconnect
  remains one-shot: restart the source for another connection. Detach/rejoin
  preserving the same live apps remains shared relay lifecycle work.
- AV1 only is advertised. Phone input, window-management controls, adaptive capability budgets,
  automatic rotation recovery and Vulkan import remain separate work.

## Desktop input

The Rust `WeldVideoPlayer` node receives typed Godot keyboard/mouse events and
window notifications. Its main-thread input adapter owns physical observations,
focus reconciliation and typed cursor presentation; GDScript has no input
handlers, hold dictionaries, or cursor-data format. The scene only supplies its
displayed `Control` and desktop-input setting. Normal `godot` bindings are
enabled without changing dependency versions. Fixture playback, Android, editor
and XR presentation do not enable this desktop input source. A freed input view
resets and disables the input source safely.

Physical observations remain separate from the playback mailbox's accepted and
suppressed holds: even a rejected press may need release reconciliation. Only
owned scalar values cross to the coordinator. A capacity-128 input mailbox wakes
that thread independently of video. Adjacent motion coalesces only with an
identical route/generation. Buttons, keys, focus and wheel events retain order.
Overflow discards the pending batch and schedules a non-droppable focus reset;
held controls cannot repeat/re-press until released. Focus regain reconciles
known controls that Godot reports released while the viewer was unfocused.

Input metadata travels with the displayed Frame or retained View update,
without cloning native leases. Crop and effective logical input extent come
from one geometry calculation. Hit testing excludes letterboxing and uses the
window origin and displayed root's input regions (at most 1024 regions). A
pending native bind discards motion and new positioned button presses rather
than queuing a retry. Keys and releases bypass that gate; discrete wheel ticks
use the last admitted hover/capture route without reading pending geometry.
Unmap/destruction/disconnection invalidates the input generation immediately.
ClientRuntime retains press routes through release, including drags outside the
image. Input times use one monotonic process clock with normal u32 Wayland
millisecond wrapping.
A click in the letterbox deliberately clears remote focus and releases held
input. Exceeding the input-region bound is a fail-closed tracer error that stops
the session, not a partial hit-test policy.

The Rust node handles events before Godot GUI navigation, so Tab, arrows, Enter
and Escape can reach Blender. Wheel presses become signed v120/continuous
scroll frames; wheel releases are ignored. Godot echo events become explicit
repeats for known held keys, with no receiver repeat timer. Cursor feedback
uses a latest-value mailbox and is applied on the main thread: named shapes,
hidden state and bounded RGBA images. Unsupported named shapes use an arrow.

All Controls in the flat video-only scene, including its outer `Flat` root,
must use `MOUSE_FILTER_IGNORE`. Rust handles input before GUI navigation.
Godot's default cursor setter refreshes through an internal mouse event; a
hovered GUI Control can replace that shape with its own arrow. Repeating the
same default shape does not refresh it again, so the wrong arrow can persist.
See Godot 4.7's
[default cursor setter](https://github.com/godotengine/godot/blob/a13da4feb/core/input/input.cpp#L1489)
and [GUI cursor selection](https://github.com/godotengine/godot/blob/a13da4feb/scene/main/viewport.cpp#L2102).
If interactive shell controls are added later, their cursor ownership must be
scoped separately from the remote image, not inherited from this video-only
scene. The viewer logs the first non-default named remote cursor once per
process (`WELD_REMOTE_CURSOR`) to distinguish delivery from presentation.

Run `apps/weld-vr/scripts/check-gdextension --desktop-cursor` on a real Wayland
desktop for the cursor regression. It uses a disposable project without a
network connection, checks displayed arrow/resize/text/crosshair shapes,
reproduces the old outer-Control override as a negative control, restores the
real configuration and checks Rust stop restores a visible arrow. This tests
Godot cursor selection, not end-to-end delivery from an application.

On 2026-09-13 this cursor regression passed on Sway with Godot 4.7.1,
Compatibility/GLES and the Wayland display driver: arrow, horizontal/vertical
resize, text and crosshair, including the negative control and stop cleanup.
An initial XWayland run passed as well; the launcher now explicitly selects
Wayland to match `run-godot-hoist --desktop`. The subsequent Blender run
`godot-hoist-6fpqafxu` logged `WELD_REMOTE_CURSOR first_named=EwResize`, confirming
that a non-default request traversed the transport into Rust presentation.
The user confirmed that Blender's cursor now changes as expected in that run.

This does not implement relative mouse locking, pointer warping, touch, IME,
virtual keyboards or host-keyboard capture. XR controller input is described
in [XR presentation](godot-xr.md#right-hand-input).
Blender interactions requiring cursor wrapping/locking still have that limit.

The earlier single-window desktop input was manually exercised on 2026-09-13 with the Godot/Blender
launcher (`godot-hoist-wclrwz_4`); the user reported that it works. The viewer
reached 946 decoded / 922 presented / 23 superseded frames before a second
Blender toplevel hit the existing single-stream admission limit and ended the
connection. This is not multi-window qualification. The source log did not
contain the focused-keyboard version diagnostic, so this run does not establish
Blender's bound keyboard protocol version.

After moving the Godot event adapter into Rust, the 2026-09-13 desktop
Blender run (`godot-hoist-86v593rb`) completed its 120-second interval and
cleanup. The user completed the requested mouse, keyboard and focus-switch
checks. The viewer reached 1989 decoded / 1946 presented / 43 superseded
frames; the host reported keyboard v9 with emulated repeats. This remains a
single-window test, not XR input validation.

The typed Rust adapter also passes 18 focused Rust tests, strict Clippy,
Linux/Android debug builds and `check-gdextension --android-export`. The engine
smoke check verifies inactive input gating and focus notifications, while the
live run exercises the callbacks with an active stream. The export check does
not deploy to a headset or qualify Android input.

## Physical evidence: 2026-09-12

Pixel 8 Pro/API37, Godot Compatibility/GLES, headless foot/htop over Iroh N0:

- First successful 45-second run: 28 decoded / 28 presented. This was htop's update
  cadence, not a 60fps video workload.
- Second run reused identity/pairing; screenshot confirmed the live terminal.
- A 75-second run restarted source/apps at 20 seconds. The same phone PID rejoined
  and continued to 46 decoded / 45 presented / one superseded decoded output.
- Android reports selected `c2.google.av1.decoder` as hardware=true,
  software=false, vendor=true, alias=false. The existing metadata probe was rerun;
  the component name alone is not a reliable hardware classification.
- Mali-side `dmabuf` filesystem `getattr` SELinux denials remain during successful
  presentation. They did not stop these runs; performance impact is not established.

These validate native live-window presentation and source-restart reconnect,
not 60fps qualification, latency measurement, Pico or complete WM behavior.

On 2026-09-13, after the DNS-policy amendment, another 45-second Pixel run
completed at 28 decoded / 28 presented with no uninitialized-DNS-context panic
in the captured app log. A separately exported single-AU diagnostic produced
one decoded / one presented frame without future input or EOS. The same
desktop fixture passed 1/1 then 120/120 twice; a live desktop run also passed.
See [regression details](godot-native-video.md#live-receiver-regression-checks-2026-09-13).
