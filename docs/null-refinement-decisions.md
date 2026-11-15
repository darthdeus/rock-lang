# Null predicate identity

Audited 2026-11-15. Null refinement recognizes the selected stdlib `is_null`
declaration. A user function with that spelling is an ordinary function,
even when annotated `@pure`.

## Evidence and decision

The [stdlib declaration](../stdlib/unified.rock) implements the concrete
predicate `value == nil`. Its `@intrinsic @nowrite` annotations are not a
general user-defined nullable-predicate mechanism. The existing
[narrowing implementation](../crates/bone-inference/src/narrowing.rs) instead
recognized the call spelling twice: as proof that a false result implies a
non-null argument, and as an exemption from condition effects. Neither follows
from a user function's name. An arbitrary same-name function can return false
for null or mutate the guarded path.

History commit `ec40150f` made condition effects consult selected overloads
and walk all expression children, but retained the older spelling exemption.
There was no accompanying declaration that all user `is_null` overloads must
implement the stdlib predicate.

Both exemptions now use the call's selected `(ModuleId, function)` from
`resolved_overload_calls`, the existing `is_stdlib_module_file` ownership
helper, and the selected function's canonical name. No source-file basename
or call-site spelling grants the contract. The ordinary `@pure` exemption
remains independent: a pure user function preserves an existing path fact,
but creates no new predicate fact.

[Call inference](../crates/bone-inference/src/calls.rs) records the selection
in `commit_selected_call`. [Expression inference](../crates/bone-inference/src/expr.rs)
infers the condition before detecting its facts; `&&`/`||` infer the left side
before using its facts on the right. Block guards likewise detect facts after
the statement is inferred. An absent selection conservatively proves no null
predicate and receives no predicate-specific effect exemption. Calls through
function values remain opaque.

This changes three previously accepted invalid shapes: an ordinary user
predicate, a pure user predicate, and an effectful same-name call after a path
check. It does not introduce user-defined predicate contracts or change general
purity rules, call invalidation, path keys, or guard divergence.

## Regression evidence

The standalone [semantic fixture](../crates/bone-semantic/tests/fixtures/nullable_is_null_identity.rock)
first failed with `expected int?, found int` before the implementation changed.
Its five sections distinguish predicate identity, purity, mutation, and local
function-value shadowing. A separate error-runner RED run accepted all three
invalid user-function cases; the local function-value control already rejected.

The committed [rejection cases](../tests/errors/null_predicate_identity.rock.err)
use custom parameter types to avoid affecting unrelated stdlib overload calls
in the legacy shared namespace. Each pins the diagnostic at the intended
nullable operation. The four-case mutation check confirms that both a wrong
diagnostic and an accepted replacement program fail every assertion.

The five [runtime controls](../tests/nullable/is_null_identity.rock) cover
stdlib bare calls, both branches, a returning guard, UFCS, field paths and
short-circuit use with a distinct user overload visible. They also check the
user overload's result and local function-value shadowing. Existing
[effect controls](../tests/nullable/refinement_effects.rock) cover the independent
purity and closure rules.

Validation commands and results:

- `cargo test -p bone-inference`: succeeds; the crate disables its empty harness.
- `cargo test -p bone-semantic`: 109 Rust integration tests and 164 fixture files pass.
- `cargo run --quiet --bin rock -- test tests/nullable`: 415 tests across 25 files pass on LuaJIT.
- `cargo run --quiet --bin rock -- test tests/nullable --backend js`: the same 415 tests pass on Node.
- `cargo run --quiet --bin rock -- test tests/nullable/is_null_identity.rock tests/nullable/refinement_effects.rock`: 13 tests pass; repeating with `--backend js` also passes.
- `python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/null_predicate_identity.rock.err --cases 4`: four original cases pass; all four wrong-diagnostic mutations and all four valid-program mutations fail as required.

## Open questions

None required for this correction. User-defined predicate declarations would
need a separate language proposal; a matching name or `@pure` is insufficient.
