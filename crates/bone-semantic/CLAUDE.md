# bone -- working agreement

The `bone-*` crates are a from-scratch bidirectional typechecker (clean HIR + arena layout,
generics-aware). This file holds conventions specific to working in it.

## Working in bone

Typechecker source is editable. Until 2026-09-09 Claude wrote only tests here and the
author transcribed every `src` change by hand; that rule is lifted -- the author
sanctions agent edits across the repository and reviews them in the diff.

What the old rule was protecting is still true, so keep these habits:

- **Lead with the fixture.** `crates/bone-semantic/tests/fixtures/*.rock` and
  the structural tests in `crates/bone-semantic/tests/structural.rs` are the
  contract. Write the failing case first, watch it fail, then change `src`.
  The `src/` trees hold no tests.
- **Change whole functions, not fragments.** This was originally a
  transcription rule, but the failures it prevented were real bugs, not typos:
  "drop the unused `let ids =`" deleted an entire `lower_modules` call and left
  a stale module that was hard to find; a lost `!` in `!errs.is_empty()`
  dropped every diagnostic; name-keyed `old_gens` collided overloaded fns. Edit
  in coherent blocks and re-read the whole function afterwards.
- **Run it.** `cargo test -p bone-semantic` after every change, and check failing names
  against `docs/known-failing-tests.md` rather than totals.
- `bone-query/` (IDE/LSP navigation -- `definition_at`, `references`, and the
  `lsp_types` facade `BoneWorkspace`) was always freely editable and remains so.
  `crates/bone-semantic/src/query.rs` is just `type_at`, the position->type primitive that
  bone's `//^` fixtures and LSP hover both read.

## Verify against real rock before deciding bone's behavior

bone is a STRICT port of rock's type system, so "what should bone do here?" is
always answered by what REAL rock does -- not by an existing fixture or a
plausible assumption. Probe it: `cargo run --bin rock test /tmp/x.rock`. This
has caught wrong assumptions that were baked into fixtures/docs (comparison
int/float widening -- the doc said "strict", rock widens; `any` coercion --
feared too-generous, was actually mostly-correct). 30 seconds of probing beats
implementing the wrong spec.

## Testing incremental work: deterministic gates first

When pinning an incremental/perf change (per-body re-inference, narrowing a
pass), lead with a DETERMINISTIC observable -- a real field or a counter -- and
fall back to timing ratios only when there's no deterministic signal. In the
per-body re-inference arc the deterministic gates
(`body_edit_bumps_only_changed_body_generation`, the overload variant, reading
`Body.generation`) caught the name-collision bug instantly; the timing-ratio
gate `body_edit_infer_work_independent_of_module_size` was flaky enough to need
the threshold loosened 4x -> 10x and still failed under parallel load.

The deterministic work-counter now exists: `Workspace.reinferred_body_count`
(`Arc<AtomicUsize>`) -- `check_module_incremental` bumps it on the non-reuse
(real `check_function`) branch, and `Session::update_file` resets it per edit. So
re-inference reuse asserts `== 1` deterministically (`body_edit_reinfers_only_
the_changed_body`, which replaced the flaky timing ratio). An analogous
`relowered` counter for per-body LOWERING (rung 2, `docs/parked/skull.md`) is
still missing -- add it the same way.

## Design docs -- read first

These predate recent work and may lag current state (check against the source
and the parked note before trusting them):

- `docs/bone-backend-plan.md` -- current state, capability, open items (entry point).
- `docs/bone-typechecker-architecture.md` -- architecture + how inference / stdlib loading work.
- `docs/bone-hir-mir-plan.md`
- `docs/parked/bone.md` -- current state + next steps (kept fresh via `/park`).

## Cross-arena copy invariant (learned the hard way)

Any place that does `*gamma.get(x)` to copy a `TypeExpr` node into `gamma.out`
where `x` can be a COMPOUND type (App/Tuple/Function/Nullable) MUST go through
`reify` (deep-copy + remap child indices). A shallow top-node copy leaves the
child `IdxRange` pointing at the source (Static) arena; a later
`with_idx`/`resolve` reads `gamma.out` with a Static index -> out-of-bounds
panic or a cyclic type -> stack overflow. Leaves (`Builtin`/`Generic`/`Infer`)
are safe to shallow-copy. Helpers: `reify`/`reify_range`/`tuple_of`. Audit:
`grep -rn '\*gamma\.get(' crates/bone-inference/src/` (it spans ~10 files there).

**Corollary (cost a detour): `reify` only copies a WELL-FORMED type.** When this
bug crashes, fix every CREATION site (`*gamma.get` of a compound -> alloc), NOT
the read/`resolve` site that panics on the already-broken type -- reify-ing or
guarding the reader can't un-dangle a child index that's already wrong.

## Finding which stdlib fn trips a bone wall/crash (no span)

bone's `Failed to unify ...` / `index out of bounds` errors often carry no span,
and `semantic_pass` is fail-fast, so the failing fn isn't named. Locate it by
stubbing Rock-bodied fns to `inline(lua)` (bone skips inline bodies; signatures
stay so callers still resolve), then probing via `BONE_STDLIB_PROBE`: (1) stub
ALL Rock fns -> if clean, the bug is in one; (2) **stub-all-EXCEPT-each-fn** and
flag the ones that reproduce -- this finds ALL culprits. Do NOT
binary-search-by-stubbing: it assumes a single culprit and silently
mis-identifies when several fns trip the same bug. A throwaway python loop that
brace-matches `fn N(...) { ... }` -> `inline(lua) fn N(...) = "return nil"` is
enough (~0.5s/probe).
