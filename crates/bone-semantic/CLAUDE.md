# bone -- working agreement

The `bone-*` crates are a from-scratch bidirectional typechecker (clean HIR + arena layout,
generics-aware). This file holds conventions specific to working in it.

## Working in bone

Typechecker source is editable; the author reviews edits in the diff. It is
easy to break, so keep these habits:

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
- **Run it.** `cargo test -p bone-semantic` after every change, and inspect failing
  test names rather than totals.
- `bone-query/` (IDE/LSP navigation -- `definition_at`, `references`, and the
  `lsp_types` facade `BoneWorkspace`) was always freely editable and remains so.
  `crates/bone-semantic/src/query.rs` is just `type_at`, the position->type primitive that
  bone's `//^` fixtures and LSP hover both read.

## Verify expected Rock behavior before changing bone

Bone is Rock's only typechecker; Skull consumes Bone's typed program. A
`cargo run --bin rock test /tmp/x.rock` probe reproduces current behavior but
is not an independent typechecker oracle. Check the language contract and
focused fixtures before changing semantics; use a probe to test an assumption
against the current compiler, then make the expected behavior explicit in a
RED fixture. If the contract and behavior disagree, diagnose the discrepancy.

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
the_changed_body`, which replaced the flaky timing ratio).
`Workspace.relowered_body_count` is the lowering twin, reset per edit and
bumped by full, partial, and body-fast-path lowering. Use it to assert lowering
work directly; `UpdateResult.relowered` has different semantics when lowering
fails.

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
