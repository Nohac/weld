# Typed configuration pipeline

Weld's graphical distribution uses one configuration path:

```text
Sway source
  -> source structure and ordered variable expansion
  -> generated typed Sway syntax
  -> native Master configuration
  -> prepared runtime resources
  -> coordinated publication and startup effects
```

The migration covers the previously implemented configuration functionality.
Unsupported Sway capabilities retain explicit diagnostics. It does not add
includes, general command chains, new input/output settings, scratchpads, or
Sway IPC command execution. Existing physical-position `bindsym` behavior and
the documented i3 movement policies remain in effect.

## Ownership

| Crate | Contract |
| --- | --- |
| `weld-config` | Source locations, located values, warnings, and unsupported-feature policy |
| `weld-sway-config` | Sway spelling, expansion, generated typed syntax, and local syntax validation |
| `weld-sway-adapter` | Explicit lowering from typed Sway syntax into native configuration |
| `weld-input-config` | Keymap preferences, device selectors/preferences, modifiers, and physical shortcut identities |
| `weld-output` | Validated output scale and selector precedence |
| `weld-tile-config` | Tiling settings, axes/layouts, and configurable tiling operations |
| `weld-window-config` | Pointer focus policy, fullscreen, and persistence operations |
| `weld-ssd-config` | Border style, renderer-independent colors, font preferences, and decoration defaults |
| `weld-i3-config` | i3 selection/layout/workspace contracts and window-rule intent |
| `weld-master-config` | Cross-feature actions, modes, startup policy, and the assembled native configuration |

Runtime crates consume their feature contracts directly. Configuration crates
have no native-host or renderer dependency. Some offer an opt-in `ecs` feature
to derive Bevy resource/event traits for the same owned data; their normal
frontend dependency graph has no Bevy dependency. The complete Master model is
also independent of the Sway frontend and Pango.

The Sway adapter uses Pango to interpret existing font-description syntax, but
returns ordinary family/style/weight/width data. Renderer font handles and GPU
resources are not part of the configuration IR. The native decoration layer
converts those preferences into its renderer-facing representation.

## Parsing and generation

The public `weld_sway_config::parse` returns a typed document. Its internal
spelling tree retains the information needed for source-ordered variables,
quoting, blocks, and opaque shell payloads. Generated productions recognize
keywords and argument structure; consumers match enums rather than strings.

A mode declaration produces a `ModeDefinition` with a typed header and bindings.
The bound `mode NAME` command produces a distinct command variant. `ModeName`,
border widths, radii, and gap values have generated private representations and
validated constructors. Unsupported binding flags remain diagnostics; they are
not silently converted into supported bindings.

Native key and pointer preferences remain declarative. XKB keymap compilation
happens in Master's preparation step. Rules preserve metadata patterns and
ordered typed actions until preparation compiles their matchers.

Declarations and rule intents retain source filenames and physical line numbers.
Frontend byte ranges refer to the expanded spelling. Variable-definition
backtraces and a lossless formatter are outside this migration.

The local generation definitions, checked-in outputs, upstream provenance, and
maintenance commands are described in
[`tools/config-codegen`](../tools/config-codegen/README.md). The initial i3 import
is an audited reference, not an automatic claim that all imported grammar is
implemented. Ordinary builds require no generator or network access.

## Lowering and preparation

The Sway adapter applies the existing compatibility defaults and declaration
precedence to produce a complete native candidate. Input/output selectors remain
unresolved so hotplug matching continues to work. Optional device preferences
retain the difference between omission and an explicit setting. Native settings
defaults have one feature-owned definition; runtime publication consumes it.

Master supplies the distribution-specific extension action type and validates
modifier-only shortcut use. It then compiles keymaps and rule patterns and
prepares renderer-facing decoration data. Failed preparation leaves live state
and startup queues untouched.

Cursor settings and shortcut replacement are validated before the shared
publication system replaces settings and actions. Startup commands run after
successful publication; `exec_always` retains its reload policy. Device-specific
application remains the host's responsibility and can report hardware failures
after publication.

Recognized unsupported capabilities are handled through the selected warning or
rejection policy. Malformed supported values remain errors. Unsupported
directional forms such as `focus next` now produce compatibility warnings under
the warning policy instead of an invalid-direction error. This changes the
diagnostic classification, not execution support.

## Validation

`scripts/check-config-generation` verifies generated files, imported checksums,
generator rejection of invalid definitions, complete physical-key mappings,
and frontend/IR dependency isolation. `scripts/check-desktop-build-graph`
checks the shared runtime feature graph after contract extraction.

Parser tests cover constrained values, source locations, mode definitions,
quoted XKB lists, output-property skipping, shell payload preservation, and
typed window-rule sequences. Adapter tests cover existing setting/action
translation. Runtime tests cover native preparation failure, mode changes,
reload, startup ordering, admission rules, workspaces, tiling, and decorations.
