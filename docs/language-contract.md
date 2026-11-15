# Language contract: selected boundaries

This is a compact index, not a complete specification. **Established** means a
rule documented or deliberately pinned by tests; **observed** means a bounded
execution result, not proof for all programs or backends. **Unresolved** marks
a choice that current behavior does not settle. See the [guide](guide.md),
[language reference](language-reference.md), [coroutine reference](coroutines.md)
and [evidence ledger](evidence.json) for wider coverage.

## Values and references

- **Established:** `value` structs copy on assignment and calls; field writes
  to a value local or parameter update that binding's copy. Reference-bearing
  leaves can still refer to shared objects. Identity structs instead refer to
  one place; storage restrictions and package exemptions are described in the
  [guide](guide.md#value-types). The MIR lowering's `is_mutable_place` and
  `boxed_value_copy_repr` distinguish reachable storage from value reads:
  [places](../crates/skull-mir/src/lower/structs.rs),
  [copying](../crates/skull-mir/src/lower/call.rs).
- **Established:** `ref T` borrows a place and writes through it. Returning a
  reference derived from a valid input is allowed; returning one to a local,
  retaining one in persistent storage, or retaining a relocatable collection
  element is rejected. See [reference rules](language-reference.md#operators-and-references),
  [reference examples](../tests/values/refs.rock), and
  [structural admission tests](../crates/bone-semantic/tests/structural.rs).
- **Observed:** reassigned captured value locals are captured by cell, and a
  reference leaf of a value passed to the tested inline Lua boundary remains
  shared. These are different from a deep copy of the whole object graph:
  [decision-point examples](../tests/values/semantics_decision_points.rock).

## Nullability

- **Established:** `T?` admits null. Null tests can refine a local or field path
  within the appropriate control-flow branch; mutation and potentially
  effectful calls can invalidate refinements. `?.` and `??` offer explicit
  nullable access and fallback. The `detect_null_narrow` and
  `stmt_guard_narrows` logic lives in [narrowing](../crates/bone-inference/src/narrowing.rs);
  [soundness cases](../tests/nullable/path_refinement_soundness.rock) and
  [effect cases](../tests/nullable/refinement_effects.rock) exercise boundaries.
  `is_null` supplies predicate facts only when the selected declaration is the
  stdlib predicate; a same-name user function does not gain that contract.
  [Identity audit and controls](null-refinement-decisions.md).
- **Observed:** selected path refinements are invalidated after assignment and
  remain scoped to the relevant branch. These tests do not establish soundness
  for every expression or effect.

## Generics

- **Established:** structs, enums and newtypes allow trailing default type
  parameters; functions do not. Defaults have restricted forms, and a bare
  type name is allowed when all parameters default. See
  [default parameter rules](language-reference.md#default-type-parameters),
  `validate_generic_defaults` in [lowering](../crates/bone-lower/src/lib.rs),
  [runtime examples](../tests/types/default_type_params.rock), and
  [semantic tests](../crates/bone-semantic/tests/default_type_params.rs).
  Inference creates fresh substitutions per call in `instantiate_generics`
  ([source](../crates/bone-inference/src/lib.rs)); the
  [instance graph](../crates/skull-graph/src/instance_graph.rs) tracks concrete
  function instances. These are implementation pointers, not an ABI promise.
- **Documented restriction, not audited here:** `I: Iterator<T>` supplies an
  iterator bound, but calls with that bound in coroutine bodies are documented
  as rejected. See [iterator rules](language-reference.md#iterators) and the
  [rejection fixture](../tests/errors/iterators/bounded_call_in_nested_body.rock.err).
  That fixture does not yet pin the rejection diagnostic.

## Coroutines and suspension

- **Established:** calling `co fn` constructs a handle; execution begins on
  resume. `yield` suspends, `resume` returns `Y?`, and `status()` distinguishes
  a yielded null from termination. `await` delegates yields and returns the
  inner result. See [coroutine reference](coroutines.md) and
  [nullable generic examples](../tests/coroutines/nullable_generics.rock).
  [Coroutine transformation](../crates/skull-transform/src/coroutine.rs)
  turns yields into states and retains frame cells needed at resumption.
- **Established body rule for explicit refs:** a frame reference used after a
  `yield` needs fresh acquisition; a last use before suspension passes that
  body check. The construction-time boundary below remains unresolved. A copied scalar or
  handle may survive and a handle can resolve a fresh borrow. The shared
  [suspension traversal](../crates/bone-core/src/suspension.rs),
  `RefSuspensionPolicy` in [inference](../crates/bone-inference/src/lib.rs),
  and `IdentityBorrowPolicy` in [identity storage](../crates/bone-core/src/identity_storage/sites.rs)
  implement related checks. The [decision record](reference-suspension-decisions.md)
  details their limits and [fixture](../crates/bone-semantic/tests/fixtures/reference_suspension.rock)
  pins selected control-flow cases.

## Unresolved: explicit refs at construction

An identity-struct parameter is rejected for a coroutine even if its last use
is before the first yield, because the constructor retains it until first
resume. In contrast, an explicitly typed `ref Cell` local can be passed to a
coroutine and retained until first resume. The
[probe](../perf/evidence/reference_construction.rock) mutates the cell after
construction and observes the new value on LuaJIT. Passing the cell directly
was rejected during overload selection. This is an observation, not a lifetime
safety certification or a decision about intended behavior.

Concrete alternatives to decide, each requiring a clear diagnostic and tests:

1. **Reject explicit ref parameters at construction**, as for identity
   parameters. The probe becomes a compile error; callers retain an `Entity<T>`
   or copy a value and acquire a fresh reference inside the resumed body.
2. **Permit retention under a defined lifetime rule.** The probe remains valid,
   but the language must specify and enforce when the referenced place may be
   mutated, moved, destroyed or escape before first resume, including aliases.
3. **Change construction semantics or parameter representation.** For example,
   copy an eligible value into the frame or run a defined initial segment
   eagerly. Either changes observable behavior (the probe could read `1`
   instead of `19`) and needs explicit rules for writes, side effects and
   identity values.

No alternative is selected here; nested coroutines, closure-held borrows and
other suspension forms also need separate auditing.

## Bounded checks

Executed on 2026-11-15 from the repository root; `rock test` commands below ran
on LuaJIT. Counts are actual tests, not successful zero-test filters:

| Command | Result |
|---|---|
| `cargo run --quiet --locked -p rock-cli --bin rock -- test tests/values/refs.rock` | 25/25 passed |
| `cargo run --quiet --locked -p rock-cli --bin rock -- test tests/values/semantics_decision_points.rock` | 8/8 passed |
| `cargo run --quiet --locked -p rock-cli --bin rock -- test tests/nullable/path_refinement_soundness.rock` | 41/41 passed |
| `cargo run --quiet --locked -p rock-cli --bin rock -- test tests/types/default_type_params.rock` | 8/8 passed |
| `cargo run --quiet --locked -p rock-cli --bin rock -- test tests/coroutines/nullable_generics.rock` | 13/13 passed |
| `cargo test --locked -p bone-semantic --test fixtures check_rock::reference_suspension.rock -- --exact` | 1 fixture-file test passed (12 sections) |
| `cargo run --quiet --locked -p rock-cli --bin rock -- test perf/evidence/reference_construction.rock` | 1/1 passed; reproduces the unresolved behavior |

These are selected examples, not a complete language conformance suite or a
cross-backend proof. See the [evidence ledger](evidence.json) and the
[reference decision record](reference-suspension-decisions.md#checks-and-results)
for additional checks and explicit limits.
