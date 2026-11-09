# Configuration generation

`sway.json` is Weld's maintained definition of the currently implemented Sway
syntax subset and its Weld extensions. It generates syntax enums and records,
constrained scalar constructors, complete argument productions, and the mapping
from accepted key names to the existing physical Linux key identities.

```sh
direnv exec /home/jonas/Development/Rust python3 tools/config-codegen/generate.py
direnv exec /home/jonas/Development/Rust python3 scripts/check-config-generation
```

Generation uses Python's standard library and rustfmt. Normal Cargo builds use
the checked-in output and perform no generation or network access. Keep grammar
changes, generated output, native lowering, and tests in the same commit.

## Definition ownership

- `enums` defines accepted literal spellings and aliases.
- `scalars` defines validated string and integer types with private fields.
- `structs` defines typed syntax records, including modes and bindings.
- `productions` maps complete argument patterns to typed constructors. Captures
  name their type; a final `*` capture invokes that type's variable-length parser.
  Constructor expressions are Rust expressions checked by the ordinary build.
- `streams` defines chained output properties. It generates canonical kind enums,
  aliases, supported typed payloads, and exact consumption of unsupported settings,
  including optional prefixes and coordinate pairs. Execution support remains
  explicit; consuming arguments does not implement a setting.
- `physical_keys` is an exhaustive mapping for the supported key vocabulary.
  Symbolic XKB binding behavior remains a separate feature; generation preserves
  the currently supported physical-position semantics.
- `references` associates adapted definitions with the pinned i3 parser states.

The `lowering` module owns native conversion. Font descriptions use Pango once;
key-to-physical-code conversion is generated from the same definitions.

The hand-written frontend owns source/block assembly and compound lexical forms
such as shell text, workspace names, chords, colors, and resize alternatives.
These helpers construct typed syntax. Neither the lowering adapter nor weldwm
accepts raw argument arrays.

## Initial i3 import

`upstream/sources.json` pins the existing i3 behavioral-reference revision and
checksums the two parser specifications and BSD license. `import_i3.py` converts
every state and production into `i3.json`, retaining patterns, transitions, and
callback names as data. Its explicit `--seed` option seeds directly reusable
direction alternatives during the initial bootstrap. The remaining supported
productions have been adapted into `sway.json`; i3 callbacks are not executed or
treated as native Weld operations.

`coverage.json` accounts for every imported state. `adapted` means a definition
uses that state as a reference, potentially for only the existing subset.
`reference-only` means it has no generated implementation. This inventory is
not a claim of complete i3 or Sway compatibility.

Weld owns the grammar after this import. Future upstream changes require an
explicit comparison and review; builds never fetch or follow upstream grammar.
Retain `upstream/LICENSE` and the source attribution with the imported material.
