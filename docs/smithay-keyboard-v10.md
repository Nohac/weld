# Keyboard v10 protocol foundation

This is a partial local backport of
[Smithay PR 1773](https://github.com/Smithay/smithay/pull/1773), pinned to
`20d0e7fddd3038c0ee7bbfb6d7a7f65810f104e6`, onto upstream
`0ff00983b6007257a7a161a4fe8b14a778e2ac8f`.
The upstream refresh preceding it passed the user's end-to-end Iroh check.

## Included

- Advertise `wl_seat` version 10 and carry the PR's logical `KeyEvent` through
  keyboard targets/grabs. Physical backend `KeyState` remains press/release.
- Allow explicit `Repeated` through `KeyboardHandle::input` and
  `input_from_source`, retaining existing physical callers via `Into<KeyEvent>`.
- Preserve the current `InputTime` API, source ownership, forwarded-key sets,
  XKB state, and focus-enter bookkeeping. Repeats change none of those sets.
- Reject source repeats unless that source holds a forwarded, repeatable key
  and the configured repeat rate is zero. Valid repeats still pass the filter.
- Keep split `input_intercept` press/release-only. Low-level `input_forward`
  checks seat-level repeat validity but does not establish source ownership.
- Check repeat eligibility under the existing keyboard-state lock, releasing
  the temporary XKB guard before any callback. The Wayland target checks only
  the bound resource version, avoiding recursive keyboard-state locking.
- Suppress `Repeated` for pre-v10 keyboard resources and input-method grabs;
  ignore it in X11 pending-enter bookkeeping. No fake release/press fallback.

The eligibility gate also applies to non-Wayland keyboard targets. Direct
`KeyboardTarget::key` callers must uphold the documented preconditions just
as they must uphold focus and held-state requirements for other key events.

## Deliberately not imported

The PR's repeat timer treats a rate in keys/second as a millisecond duration,
does not guard zero rates or non-repeatable keys, and predates the current
multi-source keyboard APIs. It also removes forwarded-key tracking and emits
the repeated pseudo-state to recipients without checking their version.
The local port does not include that scheduler or its Anvil input-loop rewrite.

Neither keyboard creation nor `change_repeat_info` forcibly overrides the
configured rate. The ordinary committed Weld setting remains 25 keys/second:
client-generated repeat continues, and explicit compositor repeats are rejected.
The separate temporary rate-zero diagnostic disables client repeat. Under that
diagnostic, legacy keyboards and input-method grabs have no repeat at all.

This foundation does **not** restore hold-to-repeat in Weld's diagnostic mode:
there is no repeat generator, receiver-authority policy, or new hoist input
message yet. Those belong to the next input batch. It must choose the repeat
owner, forward explicit repeats, cancel them on release/focus/session changes,
and define the legacy-client fallback without reintroducing duplicate keys.

## Validation scope

Use the root workspace lock and existing target directory:

```text
cargo check -p smithay --lib --examples --offline
cargo clippy -p smithay --lib --examples --offline -- -D warnings
cargo check -p weld-core --all-targets --features test-support --offline
cargo test -p weld-core --features test-support --lib --offline
cargo test -p weld-client --lib --offline
cargo clippy -p weld-core --all-targets --features test-support --offline -- -D warnings
cargo check --workspace --all-targets --features weld-core/test-support --offline
cargo build --offline
```

The enabled examples are `seat` and `compositor`; `minimal`, `vulkan`, and
`buffer_test` require unselected backends. Anvil's keyboard-target signature
is updated but its separate application build is unverified. No tests of
Smithay's own machinery were added, per the project's testing rule.

Before the port, root `cargo check -p smithay --all-targets --offline` failed
on unresolved `criterion` imports in upstream benchmarks. Explicit feature
selection for that excluded dependency is rejected by Cargo. The standalone
manifest check using Weld's features also failed offline resolving the optional
`winit =0.31.0-beta.3`; it uses a separate vendor lockfile, not Weld's lock.
These failures do not establish coverage of the skipped targets.

The user reported a successful end-to-end Iroh run after this port, with
everything behaving as expected. This is a smoke-test result, not separate
verification of every modifier, popup-grab, or disconnect edge case.
End-to-end repeat behavior still requires the next batch and is not established
by this run or the compile checks.
