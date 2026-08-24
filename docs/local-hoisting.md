# Local cross-process hoisting

Weld has a same-machine validation transport for hoisting client surfaces
between two sibling compositor processes. It exists to exercise the same
client-adapter, input, configure, scale, reclaim, and buffer-lifetime contracts
that a later network/codec transport must implement.

## Runtime boundary

The source remains the authoritative Wayland compositor for the real client.
The destination registers a Relocated `weld-client` adapter and presents the
transported toplevels and popups through ordinary window admission.

The local connection has these properties:

- Unix `SOCK_SEQPACKET` preserves one Postcard record per message.
- Linux `SCM_RIGHTS` carries DMA-BUF plane descriptors beside the commit that
  describes them.
- Both endpoints verify `SO_PEERCRED` against Weld's effective UID and validate
  the opposite Source/Destination role on every packet.
- One stable epoll descriptor wakes Smithay's calloop. Writable readiness is
  enabled only while a send queue is nonempty.
- The first committed use imports an allocation. Later uses name the stable
  buffer without resending its descriptors. Buffer destruction or final
  session unmap retires the destination import.
- Per-commit release remains separate from allocation retirement. Source
  Wayland release occurs only after the destination's final renderer lease is
  dropped.
- Copied SHM content is unsupported. The connection fails and source layout is
  restored instead of silently copying pixels across processes.

Destination input is already addressed to the transported surface and enters
the source's client runtime without the destination's compositor-global
coordinates. Configure, focus, close, and preferred-scale requests re-enter the
normal source adapter. On disconnect, Weld releases every remotely held key,
button, gesture, and finger-scroll sequence before restoring the source.

## Run the validation pair

From a graphical development shell, run:

```sh
scripts/run-local-hoist
```

The script starts two nested sibling Weld instances with distinct Wayland
sockets. The source launches Foot. Focus Foot in the source window and press
`Super+H`; the source should retain a Reclaim placeholder and the live client
should appear in the destination window.

Validate the following before treating a change to this boundary as complete:

1. Type, move the pointer, click, and scroll in the destination presentation.
2. Resize the destination repeatedly; client content should settle without
   retained file-descriptor or imported-image growth.
3. Move the destination presentation between differently scaled outputs. The
   real client should follow destination scale while the source placeholder's
   output does not control it.
4. Open client-owned popups and related toplevels; they should follow the
   transported owner and preserve their roles.
5. Reclaim from the source. The client must first commit the placeholder-sized
   configure, then remap into its preserved source slot.
6. Hoist again, hold input in the destination, and terminate the destination.
   The source must restore without a stuck key, pointer grab, gesture, focus,
   or compositor exit.

Logs are written to:

- `target/validation/weld-hoist-source.log`
- `target/validation/weld-hoist-destination.log`

The equivalent distribution flags are `--hoist-listen PATH` on the source and
`--hoist-connect PATH` on the destination. `--wayland-socket NAME` allows
multiple Weld instances in one runtime directory.

## Current constraints

- Exactly one peer is admitted during startup; there is no live listener,
  reconnect, discovery, or multi-peer policy yet.
- Both processes must run as the same UID and use GPUs capable of importing the
  advertised DMA-BUF format/modifier pair. A cross-GPU codec path is future
  work.
- The supported topology is sibling compositors. Do not launch the destination
  inside the source's Wayland session.
- The Postcard records are intentionally pre-1.0 and require matching Weld
  builds. Compatibility negotiation is deferred until the protocol stabilizes.
- There is no encoder, decoder, network transport, clipboard/DnD transfer, or
  filesystem path mediation in this binding.
