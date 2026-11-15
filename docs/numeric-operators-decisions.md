# Numeric operators: checked rules and implementation

## Decision

Preserve the established distinction: `int` shifts mask the count to five bits;
`uint` shifts reject counts outside `0..=31`. Correct the reference's previously
unqualified masking statement. No compiler semantics change is needed.

This is an existing contract, not a rule inferred from one successful probe.
Commit `94900075` introduced `uint` with both the range check and a rejection
fixture for `x << 32`. Commit `b4594b0a` removed guards only for valid literals;
the [guard-elimination contract](guard-elimination.md#literal-uint-shifts)
explicitly retains checks for dynamic counts. Commit `b7b1bf54` established the
LuaJIT-compatible remainder and division-by-zero behavior on JS.

## Rules and evidence

| Rule | Executable evidence |
|---|---|
| `/` is float division, `./` floors, and remainder follows the floor formula; zero divisors retain infinities/NaN, including `uint` | [numeric edges](../tests/operators/numeric_edges.rock), run on LuaJIT and JS |
| `int` arithmetic is not restricted to signed 32-bit results; its bitwise operations are, and shift counts mask to five bits | Same corpus: large floor division results and counts -1, 32, 33 |
| `uint` add/subtract/multiply and bitwise results wrap unsigned; shift counts must be 0 through 31 | [Lua execution tests](../crates/skull-codegen-lua/tests/emit.rs), [JS execution tests](../crates/skull-codegen-js/tests/uint.rs), [literal rejection probe](../perf/evidence/uint_shift.rock) |
| Only the six listed binary operator families are user-overloadable; unsupported declarations must identify the forbidden symbol | [operator table tests](../crates/bone-semantic/tests/operator_table.rs), [18 rejected declarations](../tests/errors/non_overloadable_operator_def.rock.err), [accepted runtime overloads](../tests/operators/overloadable.rock) |

These are selected rules, not a complete numeric specification. Runtime checks
and source references are recorded in [evidence.json](evidence.json); run
`python3 perf/check_evidence.py` to repeat them.

## Where the decisions live

| Responsibility | Implementation |
|---|---|
| Operator names and overload eligibility | `OPERATOR_FNS` in [bone-core HIR](../crates/bone-core/src/hir.rs) |
| Reject unsupported operator declarations | `pass_lower_functions` in [semantic passes](../crates/bone-semantic/src/semantic_passes.rs), consuming that table |
| Operand typing and literal `uint` shift admission | [expression inference](../crates/bone-inference/src/expr.rs) |
| Typed operations passed to emitters | `UintShl`, `UintShr` and arithmetic instructions in [MIR](../crates/skull-mir/src/mir.rs) |
| JS dynamic count check, shared by left and right shifts | `checked_uint_shift` in [effects](../crates/skull-codegen-js/src/emit/effects.rs), called by [instruction emission](../crates/skull-codegen-js/src/emit/inst.rs) |
| Lua dynamic check and valid-literal fast path | `write_u32_shift_count` in [value emission](../crates/skull-codegen-lua/src/value.rs), shared helper in [preludes](../crates/skull-codegen-lua/src/preludes.rs) |

The operator eligibility table and each backend's shift guard are already
shared. The compile-time rejection and dynamic checks serve different input
stages. This audit found no justified production-code deletion or new shared
abstraction; it changes tests and documentation, not generated programs.

## Before and after

- All 18 unsupported-operator cases now assert the intended diagnostic,
  including its symbol. The unpinned rejection inventory falls from 395 to 377
  out of the same 514 cases. That total includes six existing TODO cases; it
  is a static inventory, not an executed-test count.
- The JS dynamic-shift test previously checked only `1 << 31`. It now checks
  both directions at 0 and 31, and rejection at 32, 33 and the maximum `uint`,
  with zero and nonzero values. Node must actually execute.
- Temporarily removing the JS count guard made the strengthened test fail at
  `root(0, 32)`. The original implementation was restored and the suite passed.
- `python3 perf/evidence/check_operator_rejections.py` verifies the 18 original
  cases pass, then verifies all 18 fail with wrong diagnostic expectations and
  all 18 fail when their programs are replaced with valid code. It operates on
  temporary copies; source fixtures are unchanged by the check.

## Open questions

The type `uint` can carry non-finite division results despite its usual range.
That behavior is explicitly tested today; this audit does not redesign it.
Other operator combinations and optimizations have not received a complete
semantic audit. Runtime and compile-time performance were not remeasured:
production compiler code is unchanged, and no performance claim is made.
