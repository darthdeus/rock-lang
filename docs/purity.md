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
`?.`-less access gives. Value-level optimizations use inferred effects for Rock bodies instead of
trusting their `@pure` annotation. Opaque bodies remain a reviewed trust
boundary: their annotations supply the effect summary because the compiler
cannot inspect their target-language behavior.

## The consumers

| Site | Relies on `@pure` for | Sound under the contract | Notes |
| --- | --- | --- | --- |
| Nullability narrowing, call commit (`bone-inference/src/calls.rs`, `commit_selected_call`) | A `@pure` callee keeps `refined_nonnull` (field paths, globals); any other call clears it | Yes, within the contract | Escape hatches: a `@pure` wrapper around a `@debug_impure` writer, or a lying opaque body, keeps a stale narrow. Both degrade to a Lua/JS runtime error, never a silent wrong value. |
| Nullability narrowing, condition effects (`narrowing.rs`, `cond_has_effect`) | A `@pure` call inside the condition after the null check does not drop the narrow; anything else does | Yes, after this audit | Was unsound before: only `Call`, `Not`, `BinOp`, `FieldAccess` and `SafeNav` were inspected, so a call under an index, interpolation, cast, tuple, `match`, `if`, `?.let` or a safe-nav call kept the narrow (fixed; `tests/errors/narrowing_effects.rock.err`). |
| Whole-local narrowing (`infer_branch`, block guards) | None directly; "a callee cannot reassign a local" | Was unsound for closure-assigned locals | A lambda declared earlier that assigns the local runs at any call. Fixed: such narrows are revoked by the same effects that clear paths (`closure_assigned_locals`, `invalidate_mutable_refinements`); a `@pure` call keeps them. |
| Inliner gate (`skull-transform/src/inline.rs`, `annotation_gate`) | Eligibility only: `@inline` or `@pure` bodies may be spliced | Yes, independent of purity | A splice forwards each argument *value* once (`forward_param_reads`), keeps every instruction that can raise, and never reorders across a call, so inlining an impure body would still be correct. `@pure` here is a performance policy, not a soundness input. |
| Bounds-check elision (`skull-transform/src/in_bounds.rs`) | Inferred memory effects; opaque annotations are trusted | Subject to reviewed opaque contracts | Rock bodies are walked transitively, including callbacks reached through indexing, heap fields and equality. Opaque `@pure`, `@nowrite` and `@writes_elems` bodies supply trusted summaries. Push can invoke a foreign backing table's `__newindex`. Only a local Lua allocation/escape proof exempts an individual append; the callee summary remains conservative. |

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
  `sinking.rs` `always_inlinable` / `preserve_effect_order`: use MIR effects
  and operation-specific guarantees. A proven bound alone does not make a
  table access removable: a missing nullable slot can invoke `__index`.

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

## Opaque callback audit (2026-11-14)

A read-only Lua expression can still write through a metamethod. Generic
raw table reads, writes and tuple access therefore cannot promise
`@nowrite` or element-only writes. The same applies to `tostring`, `print`
and runtime type-tag lookup. Host logging level and sink callbacks also
invalidate caller facts. `__nb_checked_set` writes native elements and is
annotated accordingly. JavaScript type-tag getters, `ffi_string` conversion
and `join` on a proxy-backed vector have the same callback risk. Their
shared effect promises must cover the backend that can run a callback.

The regression suites exercise callbacks that shrink a guarded vector,
then require the subsequent access to raise its bounds error. They also
cover native element mutation invalidating a previously checked index.
The direct operation and helper summaries must agree: preserving a check
inside a helper does not protect its caller from stale memory facts.

### Heap-record callbacks (2026-11-14)

A heap-record type alone does not prove that its runtime table or object
has ordinary fields. Host-supplied Lua `__index`/`__newindex`, JavaScript
getters/setters and proxies can run arbitrary code during field access.
Reads and writes now invalidate memory facts both directly and through
inferred helper summaries. Unused reads execute, and repeated reads remain
separate operations.

Lowering identifies source heap-record receivers while their concrete types
are available. Their physical field accesses, including embedded leaves,
use the callback-capable `Record` index kind. This preserves effects through
inlining, cleanup and emission without assigning those effects to native
value fields, compiler-owned coroutine frames or shader structs. A generic
instantiation uses its concrete type, so a native value does not become a
heap record merely because its source parameter was generic.

Nil writes to a potentially foreign record remain observable. Lua skips a
nil write only after proving at runtime that the table has no metatable and
the key is absent. The metatable test precedes the field read, so the guard
cannot introduce an extra getter. The already-absent embedded-field shortcut
is restricted to compiler-owned storage. Local records can still be
scalarized under the existing constructor-family and nonescape proof;
a fresh constructor used only by one field read can still disappear.

Lua and JavaScript regressions cover direct and helper-mediated callbacks,
unused reads, repeated getters and guarded vector accesses. Lua additionally
covers safe navigation, embedded leaves, absent-field clearing and repeated
nullable payload clearing. Existing value-copy, coroutine and local-record
scalarization tests protect their distinct semantics.

This is not a promise about arbitrary host replacements for native/value
representations or container headers. Those representations retain their
ABI invariants; their internal fields are not general heap-record fields.
