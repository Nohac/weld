# Agent rules

- Read `CONTRIBUTING.md` before changing code or project structure.
- Treat `IDEA.md` as an initial design proposal and discussion starter, not an
  implementation checklist or settled source of truth.
- Start with the smallest coherent change. Do not create speculative crates,
  abstractions, protocols, backends, or UI systems merely because `IDEA.md`
  mentions them.
- Read the relevant skill under `.agents/skills/` before changing Smithay,
  Bevy ECS, rendering, or scene/UI code.

## Commits

- Make atomic commits once a coherent batch of changes is done.
- Keep commit messages short: subject line only, with a body only when it adds
  essential context.
- Never add generated-by or co-author attribution trailers.

## Code

- Attempt to add a test for changed behavior.
- Prefer integration tests for behavior across module boundaries and focused
  unit tests for small, isolated policy logic.
- Run the narrowest applicable checks before widening to the full suite.
- Never assume a warning or failure is pre-existing without verifying it.
- Do not build with the release profile unless asked or investigating
  performance.
- Avoid `panic!`, `unreachable!`, `.unwrap()`, unsafe code, and lint ignores in
  production paths.
- Prefer `#[expect(...)]` with a reason over `#[allow(...)]` when a lint must be
  disabled.
- Use descriptive names and top-level imports.
- Prefer [`TypeName`] links in Rust doc comments when rustdoc can resolve them.
- Update dependencies narrowly; do not perform blanket lockfile updates.
