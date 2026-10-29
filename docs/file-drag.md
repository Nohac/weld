# Local file dragging

Offer files or directories using the selected desktop session's pointer:

```sh
weldctl drag ~/Pictures/example.png
weldctl --session weld-drm-test drag ./first.txt './second file.txt'
weldctl drag --cancel
```

The command returns when the native drag starts. Move over a target and click
the primary button to drop; holding that button while moving also works.
Escape, a secondary button, `--cancel`, loss of host focus, or session locking
cancels the offer. An unfinished drag expires after 60 seconds. The cursor
shows the copy operation while the drag is active.

Weld owns the source, so the command creates no window and may exit immediately.
The target receives a standard `text/uri-list` offer with the Copy action.
Paths are canonicalized and URI-escaped on the administrative worker. Existing
files and directories are accepted, up to 256 paths and a 64 KiB URI list.
The target application decides how to import or open them.

The native owner requires a local pointer position, rejects competing grabs,
and preserves keyboard focus while the pointer crosses application-specific
seat bindings. Pointer buttons belong to the drag; motion continues through
the desktop's hit testing. Ordinary pointer hover is restored after completion.
An eight-entry transfer queue and five-second nonblocking write deadlines keep
slow receivers off the host loop. The worker sleeps until work or shutdown.

This interface serves native Wayland data-device targets on the local machine.
The XWayland XDND bridge and cross-device file availability are separate
integration work, tracked with [XWayland](xwayland.md) and
[session services](session-service-boundary.md).

## Validation

`file_drag_tests` exercises real private Wayland connections: source-free offers,
URI transfer, independent client seats, preserved keyboard focus and cancellation
with a held button. The session-lock regression checks drag retirement and
rejection while locked. Unit tests cover URI preparation, cancellation of a
stalled transfer and native-control RPC dispatch.

For an end-to-end hidden nested run, launch `scripts/check-control-launch
--nested --no-build --file-drag-parent SOCKET --file-drag-owner OWNER` through
the parent session's `weldctl launch --headless --control --owner OWNER`.
The runner verifies the nested window's owner, PID and hidden state before
injecting input. It checks CLI admission, click-to-drop, Escape, explicit
cancellation and unchanged window inventory, then performs the ordinary
CLI/RPC/MCP regression and shuts down its owned host.
