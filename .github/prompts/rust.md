# Rust standards

Read before writing or reviewing Rust, build tooling or dependencies. Apply these
standards to new and changed code; existing violations do not justify unrelated
rewrites. Use [the roadmap](../../docs/roadmap.md) for intended responsibilities
and [Development](../../docs/development.md) for current behaviour and validation.

## Design and ownership

- Use stable, idiomatic Rust and straightforward control flow. Search existing
  implementations and callers before adding code. Prefer improving, combining
  or deleting over introducing another implementation.
- Give each responsibility one authority. Keep shared contracts and pure policy
  in core, platform effects in the host/Windows crates and operational wording
  at CLI/platform boundaries. Follow the roadmap's agreed responsibility split;
  do not mistake current implementation for intended design.
- Add a module, trait, helper, mode, setting or dependency only for a current
  need. Similar syntax alone does not justify an abstraction.
- Borrow by default (`&str`, `&[T]`, `&T`). Reconsider ownership before cloning
  to satisfy the borrow checker. Use `Arc::clone`/`Rc::clone` explicitly.
- Keep application state owned by the runtime or service. Do not add global
  mutable state or a cache as a second authority. Use atomics, locks and channels
  for actual thread, signal or SCM callback requirements; single-threaded
  `Rc<RefCell<_>>` is not a substitute for necessary synchronisation.
- Use enums for closed sets and exhaustive matches on project enums. Ordinary
  flags can remain booleans. Prefer standard types such as `Duration` and `Path`;
  add identifier newtypes when they prevent a demonstrated mix-up.
- Keep items private, then `pub(crate)` where needed. Library APIs may be public
  for real cross-crate consumers or external contracts. Check consumers before
  removal. Derive traits only when useful; avoid `Default` that breaks invariants.
- Remove superseded paths, exports, settings and dependencies with their callers.
  Keep behaviour-preserving refactoring distinct from policy/schema changes.

## Correctness and failures

- Memory quantities are `u64` bytes. Distinguish whole-guest RAM, fixed RAM and
  device allocation. Use checked arithmetic, exact unit conversions and aligned,
  bounded targets. Saturate only for an intentional, documented floor or ceiling.
- Validate configuration, protocol data and persisted state at their boundaries.
  Preserve identity, freshness, replay and ordering checks. Zero is a byte value,
  not an implicit marker for uninitialised state.
- Preserve durable request/command ownership, convergence checks and host reserve
  protection. Distinguish rejection before execution from an unknown outcome;
  never silently replay uncertainty, discard unresolved state or clear a latch.
  Re-read changing state immediately before an effect and check its preconditions.
- Use `Result`, existing error enums and `thiserror`; preserve causes and operation
  context. Do not add another error framework without a concrete need.
- Do not introduce `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!`,
  `unimplemented!` or `dbg!` outside tests. Propagate or explicitly handle failure.
- Do not silently discard errors with `let _ = ...` or `.ok()`. Explain intentional
  best-effort handling at the site and preserve the primary failure. Never report
  incomplete work as successful. Preserve existing CLI exit and SCM status semantics.
- Prefer `.get()` for untrusted indices; direct indexing needs an established bound.

## Platform code

- Restrict `unsafe` to necessary OS/FFI boundaries. Keep it scoped and document
  pointer validity, lifetimes, callbacks and handle ownership with `SAFETY` comments.
  Keep policy and parsing safe; release owned handles on every path.
- Treat worker, callback and shutdown lifetimes explicitly. Preserve required
  synchronisation and prevent background work outliving the state it accesses.
- Keep Windows atomic publication and Linux durable checkpoint semantics distinct.
  Reuse actual shared rules without hiding platform differences.

## Lints, structure and dependencies

- Follow the lint policy actually configured in the manifests and gate. The local
  gate denies Clippy warnings; this workspace does not configure pedantic or
  restriction groups, an unsafe ban, or numeric function/file/argument caps.
- Add no `#[allow(...)]` or broad suppressions. For a genuinely inapplicable lint,
  use `#[expect(clippy::lint_name, reason = "...")]` on the narrowest item, never
  a module/crate. Explain correctness, not convenience; fix the cause otherwise.
- Keep functions and files cohesive. Split by responsibility when it helps;
  do not fragment code or bundle unrelated arguments to satisfy arbitrary limits.
  Prefer iterators when clearer than manual indexing.
- Use descriptive names and British English for new prose. Preserve API/protocol
  spelling and stable machine-readable fields. Comments explain contracts and
  non-obvious decisions; add API/module docs where needed and remove stale notes.
- Prefer existing dependencies or clear `std` code. A new crate must justify its
  maintenance, platform support, features and code replaced. Disable unused
  features and remove replaced code/dependencies in the same change.

## Tests and verification

- Requested behaviour defines correctness; tests provide evidence. Test observable
  behaviour at the owning entry point, not constants or implementation shape.
  Add focused coverage for changed logic and a regression test for a testable bug.
- Cover relevant arithmetic, protocol, state, persistence and failure boundaries.
  Use fake clocks/adapters, temporary files and controlled concurrency interleavings.
  Keep unit tests independent of live VMs, services and host configuration.
- Trace a failing workflow before deciding whether code or test is wrong. Update
  or remove obsolete tests with a reason. Never weaken tests merely to pass, or
  preserve poor design and special cases solely to satisfy existing tests.
- Respect review-only scope. For implementation changes, run focused checks then
  the local gate described in Development, using its direct fallback if needed.
  Documentation-only work needs reference/whitespace checks, not builds.
- Build Windows changes for the selected target. Native SCM, Win32 sampling, ACLs
  and atomic replacement require Windows execution; cross-target checks and mocks
  do not prove native behaviour. Report what ran, failed and remains unverified.
- Live tests and deployment need explicit authorisation and bounded resources;
  a code or documentation task does not authorise operational effects. Keep open
  work and completion criteria in the roadmap, not in these standards.
