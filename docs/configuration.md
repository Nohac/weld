# Typed configuration pipeline

Weld's graphical distribution uses one configuration path:

```text
Sway source
  -> source structure and ordered variable expansion
  -> generated typed Sway syntax
  -> native weldwm configuration
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
| `weld-sway-config` | Sway spelling, generated typed syntax, local validation, and the `lowering` module |
| `weld-input-config` | Keymap preferences, device selectors/preferences, modifiers, and physical shortcut identities |
| `weld-output` | Validated output scale and selector precedence |
| `weld-tile-config` | Tiling settings, selection/wrapping, explicit layout cycles, and resize operations |
| `weld-window-config` | Pointer focus, fullscreen/persistence, workspace navigation, and generic rule predicates/lifecycle |
| `weld-ssd-config` | Border style, renderer-independent colors, font preferences, and decoration defaults |
| `weldwm-config` | Cross-feature actions, modes, startup policy, and the assembled native configuration |

Runtime crates consume their feature contracts directly. Configuration crates
have no native-host or renderer dependency. Some offer an opt-in `ecs` feature
to derive Bevy resource/event traits for the same owned data; their normal
frontend dependency graph has no Bevy dependency. The complete weldwm model is
also independent of the Sway frontend and Pango.

The Sway lowering module uses Pango to interpret existing font-description syntax, but
returns ordinary family/style/weight/width data. Renderer font handles and GPU
resources are not part of the configuration IR. The native decoration layer
converts those preferences into its renderer-facing representation.

`weld-window::rules` compiles metadata predicates independently of the action type.
`src/config/rules.rs` installs weldwm's rule evaluation and cross-feature execution;
`weld-i3-quirks` remains the selected tree/workspace behavior implementation.
The combined IR has no i3 crate dependency. Layout toggles lower to explicit
cycles containing fixed layouts or remembered/alternate split choices. Numbered
workspace selection retains its leading-number matching semantics in the native
`NumberedName` selector.

## Parsing and generation

The public `weld_sway_config::parse` returns a typed document. Its internal
spelling tree retains the information needed for source-ordered variables,
quoting, blocks, and opaque shell payloads. Generated productions recognize
keywords and argument structure; consumers match enums rather than strings.

Numeric `bindcode` chords retain a validated XKB code until lowering converts
them to Linux physical identities. Named keys use the generated mapping; both
forms share duplicate checks, mode handling, and runtime press/release ownership.

A mode declaration produces a `ModeDefinition` with a typed header and bindings.
The bound `mode NAME` command produces a distinct command variant. `ModeName`,
border widths, radii, and gap values have generated private representations and
validated constructors. Unsupported binding flags remain diagnostics; they are
not silently converted into supported bindings.

Output property names, aliases, supported payloads, and unsupported argument
consumption are generated from the `streams` definitions. The handwritten block
assembler matches typed property results. Unknown property tails remain
uninterpreted, and recognized unsupported settings still warn without enabling
execution. This preserves a later supported scale on the same line.

Native key and pointer preferences remain declarative. XKB keymap compilation
happens in weldwm's preparation step. Rules preserve metadata patterns and
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

The Sway lowering module applies the existing compatibility defaults and declaration
precedence to produce a complete native candidate. Input/output selectors remain
unresolved so hotplug matching continues to work. Optional device preferences
retain the difference between omission and an explicit setting. Native settings
defaults have one feature-owned definition; runtime publication consumes it.

weldwm supplies the distribution-specific extension action type and validates
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
typed window-rule sequences. Lowering tests cover existing setting/action
translation. Runtime tests cover native preparation failure, mode changes,
reload, startup ordering, admission rules, workspaces, tiling, and decorations.

## Remaining coverage

The current capability audit and proposed implementation order are recorded in
[Configuration coverage and next steps](configuration-roadmap.md). Its planned
work is separate from the implemented behavior described above.
