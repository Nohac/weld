# Provenance and scope

The DRM display/configuration, surface descriptor and mapped-image lifetime
patterns are adapted from cros-libva 0.0.12, previously vendored in this repository.
The upstream BSD-3-Clause copyright notice and license are preserved in LICENSE
and in the adapted modules. Weld's existing VPP submission and DMA-BUF validation
code has moved into this utility alongside those patterns.

Upstream: https://github.com/chromeos/cros-libva
Upstream main inspected for the libva 2.23 changes: `59384456ac2ae78c0c3e5515f41ef1efd9b802cf`.
The VP9 additions exist upstream; this crate neither copies nor generates codec
picture-parameter structures. Its bindings are generated only for the calls and
records used by Weld, against installed libva >= 1.23 headers (libva package 2.23).

This is a maintained Weld utility, not an upstream subtree or general libva
wrapper. Raw ABI types stay private. FFmpeg owns codec implementation, ordinary
CPU uploads and decoded-frame mapping. This crate owns explicit render-node
queries, modifier-constrained XRGB conversion, owned DMA-BUF records and the
image reads/reference uploads still used by diagnostics and validation.
