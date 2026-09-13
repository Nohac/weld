# Godot live-window tracer

Godot can receive one live AV1 window from headless Weld on Linux or Android.
This is a bounded presentation slice, not the multi-window/input shell. The
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
`--app blender` is another single-root test, not support for its extra dialogs.
Remote input is not wired yet.
Closing that window and creating a replacement also requires a new connection;
the limit is one media-stream identity per connection, not merely one visible
window at a time. Blender dialogs/popups exceed this initial limit.

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
  independently of Godot's frame cadence. No new unbounded media queue exists.
- Android still uses `weld-media-android`: FFmpeg/`ffmpeg-next`, `ndk_codec=1`,
  MediaCodec to acquired native ImageReader buffers. Linux uses existing
  FFmpeg/VA-API. Context construction/calls/destruction stay on pool workers.
- Atomic commit publication creates a one-shot client-lease payload. Only the
  committed root transfers its frame into the presentation mailbox. The emptied
  destination-owned client lease cannot free that native allocation; GPU fence
  retirement owns it. Retained commits can update crop without new pixels.
- The existing EGLImage presenter retains texture and material in Rust. Texture
  object size is immutable during a session; native import defines actual storage.
  Panel aspect follows logical content dimensions. Per-request visible extent
  intersects codec crop and content view, excluding AV1 padding, including odd
  1280x833 content in larger coded storage. Unmap/destroy/disconnect clear the view.
- One media stream per connection. Additional window/layer streams fail visibly
  before native allocation, rather than silently decoding in the background.
- Two generations, four jobs, depth two, up to two workers. Affinity places one
  stream's generations on one worker; a third generation waits for retirement.
- Seven global frame credits cover submitted jobs, completions, publication,
  mailboxes, GPU imports and retirement. Credits return after native release.
  Each Android reader permits eight acquired images. Old outputs may retain old
  readers after decoder retirement, also bounded by the frame credits.
- Visible extent is at most 2048 per dimension and 1920x1080 total pixels. These
  are tracer admission bounds, not probed hardware limits or a parser-enforced
  bound on every possible allocation described by encoded sequence headers.
- No future frame is required for the final output. Three seconds without output
  is an explicit low-delay error, not an empty completion or fabricated EOS.
- Godot refresh is forwarded through existing `SetPresentation`; source pacing
  still clamps it to the encoder ceiling. There is no ACK roundtrip.
- Stop/pause cancels production and performs GPU cleanup. Relaunch the viewer
  and source after pause. Source relay re-admission after viewer disconnect
  remains one-shot: restart the source for another connection. Detach/rejoin
  preserving the same live apps remains shared relay lifecycle work.
- AV1 only is advertised. Input, multi-window layout, adaptive capability budgets,
  automatic rotation recovery and Vulkan import remain separate work.

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
