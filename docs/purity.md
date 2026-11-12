# Purity: what `@pure` promises and who relies on it

An audit (2026-11-12) of every place the compiler reads `@pure`, what each
one assumes, how the checker backs that assumption, and where it does not.
The conclusion first, then the evidence.

## The contract

`@pure` is a **source-level checker contract**, verified on Rock bodies and
trusted on opaque ones:

- The checker (`bone-inference/src/purity.rs`) rejects, in a `@pure` Rock
  body: reads of mutable globals, assignments, compound assignments and
  increments (any target, so index and field writes included), array and
  heap struct literals, lambdas, nested fns, statics, calls through function
  values, and calls to callees that are neither `@pure` nor `@debug_impure`.
  Interpolation needs no rule: a struct renders through the runtime's
  `inspect`, never a user `to_string`.
- Two things the checker cannot see, by design: an `inline(lua)`,
  `inline(js)`, `operator(lua)` or `extern` body marked `@pure` is trusted as
  written, and a `@debug_impure` callee may do anything. The first is how the
  stdlib's arithmetic and `math.*` wrappers are pure at all; the second is
  the `print` escape hatch, kept for debug printing only. Neither is a proof.

So a `@pure` call is a *contract that the body mutates nothing reachable*,
held by the checker for Rock bodies and by review for the stdlib's opaque
ones. An optimization may rely on it for **type refinements**, where a
violation degrades to a runtime error with a worse message (`attempt to
index a nil value`), because that is the same failure class a hand-written
`?.`-less access gives. An optimization must **not** rely on it where a
violation would silently compute a wrong value or write past a bound: those
need a structural proof (a matched body, an SSA fact), not an annotation.

## The consumers

| Site | Relies on `@pure` for | Sound under the contract | Notes |
| --- | --- | --- | --- |
| Nullability narrowing, call commit (`bone-inference/src/calls.rs`, `commit_selected_call`) | A `@pure` callee keeps `refined_nonnull` (field paths, globals); any other call clears it | Yes, within the contract | Escape hatches: a `@pure` wrapper around a `@debug_impure` writer, or a lying opaque body, keeps a stale narrow. Both degrade to a Lua/JS runtime error, never a silent wrong value. |
| Nullability narrowing, condition effects (`narrowing.rs`, `cond_has_effect`) | A `@pure` call inside the condition after the null check does not drop the narrow; anything else does | Yes, after this audit | Was unsound before: only `Call`, `Not`, `BinOp`, `FieldAccess` and `SafeNav` were inspected, so a call under an index, interpolation, cast, tuple, `match`, `if`, `?.let` or a safe-nav call kept the narrow (fixed; `tests/errors/narrowing_effects.rock.err`). |
| Whole-local narrowing (`infer_branch`, block guards) | None directly; "a callee cannot reassign a local" | Was unsound for closure-assigned locals | A lambda declared earlier that assigns the local runs at any call. Fixed: such narrows are revoked by the same effects that clear paths (`closure_assigned_locals`, `invalidate_mutable_refinements`); a `@pure` call keeps them. |
| Inliner gate (`skull-transform/src/inline.rs`, `annotation_gate`) | Eligibility only: `@inline` or `@pure` bodies may be spliced | Yes, independent of purity | A splice forwards each argument *value* once (`forward_param_reads`), keeps every instruction that can raise, and never reorders across a call, so inlining an impure body would still be correct. `@pure` here is a performance policy, not a soundness input. |
| Bounds-check elision (`skull-transform/src/in_bounds.rs`) | Nothing | Yes | Deliberately does not trust `@pure` (a violation would be a silent out-of-bounds write). Only the stdlib push, matched by body text, is exempt from the call kill. |

Things that are **named** "pure" but do not read the annotation:

- `skull-codegen-lua/src/regalloc.rs` `statement_is_pure`: a MIR allowlist of
  instructions that run no Lua call, used to place trace-exit kills.
- `bone-lints` `is_pure`: a syntactic walk for the unused-result warning.
- `bone-query` `init_is_pure`: the LSP "inline local" refactoring treats a
  binary-operator call with pure operands as duplicable; a user
  `operator +` with a side effect would be duplicated by the refactoring.
  IDE-only, documented, not fixed.
- `bone-lower` `expr_is_pure`: compound assignment `place op= v` evaluates
  the place's effect-free parts twice instead of hoisting them. A binary
  operator call over pure operands counts as effect-free, so
  `xs[a + b] += 1` with a user `operator +` that counts its calls counts
  twice (`p13` probe: `2 1`). Stone and the demos have no such site; the fix
  (treat operator calls as effectful at lowering) changes the emitted shape
  of every `xs[i + 1] += v`, so it is left as a known edge.
- `copy_prop.rs` `keeps_vec_lengths`, `cleanup.rs` `is_dead_value`,
  `sinking.rs` `always_inlinable` / `preserve_effect_order`: all treat every
  call as a barrier. No purity input.

## Probes

Twenty purity-checker probes and twenty-four narrowing probes were compiled
and run (scratch programs, not committed; the surviving holes became tests).
What the checker rejects: named calls to non-pure fns (direct, method, via
macro, in `defer`, via a `@pure` wrapper), index and field writes, lambdas,
statics, coroutine construction, `vec_push`/`set`. What it admits and why:

| Probe | Result | Status |
| --- | --- | --- |
| `@pure` body calls a fn-value parameter | type-checked before | fixed: rejected, error test |
| `@pure` body calls a `@debug_impure` fn that pops a vector / nulls a field | admitted | escape hatch, kept for `print` only |
| `@pure inline(lua)` body that writes `len = 0` or a field to nil | admitted | opaque bodies are trusted |
| `@pure` body interpolates a struct with an impure `to_string` | admitted | sound: runtime `inspect`, user `to_string` never runs |
| whole local nulled by an earlier lambda, then a fn-value or named call, then used | ran, `nil` arithmetic error | fixed: narrow revoked |
| narrow kept across a call hidden under index / interpolation / cast / tuple / unary minus / `match` / `if` / `?.let` / safe-nav call in the condition | ran, `nil` arithmetic error | fixed: generic child walk |
| lambda nulling the local declared *inside* the narrowed block | already rejected | the lambda body's assignment un-narrows at inference time |
| local field nulled by a lambda, then fn-value call | already rejected | fn-value call clears paths |
| `@pure` callee with an impure argument (`id(evil())`) | already rejected | arguments are inferred before the call's effect |
| `while g.f != null && n < 3 { ..; evil() }` | correct | the condition is re-inferred per turn |

## What to rely on going forward

1. **Refinements may trust `@pure`.** The two escape hatches are
   `@debug_impure` (use it only for debug printing) and opaque bodies
   (review them when adding `@pure`; the stdlib's are arithmetic wrappers).
   A violation is a runtime error at the use, not silent corruption.
2. **Value-level optimizations must not rely on `@pure` of a Rock body.**
   They use the inferred effect table instead (`bone_core::effects`), which
   walks Rock bodies and so sees through a `@pure` wrapper to its
   `@debug_impure` callee. For an *opaque* body the table trusts `@pure`,
   `@nowrite` and `@writes_elems` alike: there is nothing to read, and the
   author who wrote the raw target code is the one making the promise. That
   is the one escape hatch, the same one `inline(lua)` is on its own.
3. **Calls are the only effect boundary the narrowing understands.** A
   yield, a fn-value call and a non-pure named call all invalidate; the
   condition walk and the closure-assigned rule close the gaps found. The
   remaining theoretical gap is aliasing of a *local struct's field* through
   a value stored elsewhere and mutated by a `@pure`-wrapped opaque body,
   which is the same escape hatch as above.
4. **Known, accepted edges:** the compound-assignment double evaluation of a
   user operator in a place expression, and the LSP inline-local refactoring
   duplicating a user operator call. Both are documented above and have no
   occurrence in the public corpora.

The regression tests: `tests/errors/pure_verification.rock.err` (checker),
`tests/errors/narrowing_effects.rock.err` (rejected narrows),
`tests/nullable/refinement_effects.rock` (narrows that must still hold), and
`tests/iterators/vec_bounds_elision.rock` (elision ignores `@pure`).
