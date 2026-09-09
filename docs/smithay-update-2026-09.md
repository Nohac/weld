# Smithay refresh: September 9, 2026

Updated the full vendored subtree from
`5fb12b87407b3680135c45d94214c5f1b1d0fbea` to
`0ff00983b6007257a7a161a4fe8b14a778e2ac8f` (37 upstream commits).
[Provenance](../vendor/smithay.upstream) pins the exact revision. Keep updates
in the existing squashed-subtree history, separate from Weld's API adapters.

The only deviations from that upstream tree remain the two existing patches:
omitting Smithay's nested workspace table and retaining
`DrmDeviceFd::new_unprivileged` for render-node explicit synchronization.

## Integration changes

Smithay now uses microsecond `InputTime` values. Weld's backend-neutral input
and transport timestamps remain milliseconds: libinput ingress uses `.millis()`
and seat delivery uses `InputTime::from_millis`. This does not redesign input
timing, scheduling, repeat ownership, or the wire protocol.

Smithay's `WlSurface` pointer target also requires `PointerConstraintsHandler`.
Weld supplies its default implementation without advertising a pointer
constraints global. This is an API compatibility change, not pointer capture
support.

The lockfile reuses the already-locked `rand` 0.10 dependency and removes the
unused 0.9 dependency chain. Smithay's optional winit backend is not enabled;
Weld's own winit remains 0.30.13.

## Validation

The upstream file contents were compared against the imported revision; only
the two documented local patches differ. Existing Weld tests cover the policy
boundary; no tests were added that merely exercise Smithay wrappers.

Checks for this batch:

```text
cargo test -p weld-core --features test-support --lib --offline
cargo test -p weld-client --lib --offline
cargo clippy -p weld-core --all-targets --features test-support --offline -- -D warnings
cargo check --workspace --all-targets --features weld-core/test-support --offline
cargo build --offline
cargo fmt --all --check
git diff --check
```

Core and client tests pass (112 and 29 tests). All-target checks require
`test-support`; the missing `SurfaceId::for_test` failure without it was
reproduced before importing the update.

Real nested and DRM validation is still pending. Check keyboard/pointer input,
popup grabs and dismissal, drag-and-drop, hoist/reclaim, and a timed DRM VT
round trip before attributing runtime behavior to later input changes. See
[the integration validation notes](smithay-integration-validation.md) and
[Contributing](../CONTRIBUTING.md) for launch commands.

## Follow-ups, not part of this refresh

The upstream update changes synthesized keyboard release/fallback timestamps
from zero to `InputTime::now()` (monotonic clock). Weld's synthetic events still
use process-relative elapsed time, while backend events use backend timestamps.
Audit and unify the clock boundary in the input pass; this refresh preserves
Weld's existing conversions and must not be described as behaviorally inert.

Upstream's keyboard v10/compositor-repeat PR is not merged into this revision.
Port and review it separately against the new timestamp and multi-source
keyboard APIs. The temporary repeat-disable diagnostic is independent of this
update. Input processing isolation from video maintenance is also a separate
batch.
