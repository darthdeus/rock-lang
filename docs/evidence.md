# Evidence-driven cleanup

Make the compiler easier to explain, change and check. Collect facts to choose
useful repairs; do not optimize a quality score or require a complete language
specification before repairing anything.

## Run the baseline

Python 3.11+, Git, and the compiler's normal test dependencies are required.
Run from the repository root:

```sh
python3 perf/inventory.py
python3 perf/check_evidence.py
python3 perf/overview.py
python3 -m unittest discover -s perf -p 'test_*.py'
```

Open `target/inventory/index.html`. The adjacent `inventory.json` contains
every measured file; `evidence/results.json` and its logs record the checks.
Reports stay local. Both record the source revision and working-tree dirtiness;
a dirty report is not a reproducible release baseline. The inventory reads
tracked working-tree files only, excluding symlinks and missing files (listed
separately). Newly added files enter the inventory once tracked by Git.
The source extensions are explicit in `perf/inventory.py`; this is not a count
of every repository file. Known upstream-derived crates and prebuilt Stone
bindings are separated; origin classification is not a generated-code detector.

For breadth, start with `target/inventory/overview.html`: the searchable
[subsystem map](subsystems.md) combines measured size, reviewed responsibility
and test locations, bounded checks, explicit unknowns and a short parallel-work
queue. Its 21 groups cover the tracked source; they are work boundaries rather
than a complete language-feature taxonomy.

## What counts as evidence

[evidence.json](evidence.json) holds a deliberately small set of claims, each
with its kind, source paths and symbol/text anchors, runnable checks, and a
limit on what those checks establish. The checker requires the expected exit
code and output, including nonzero test counts. A test filter matching nothing
must not turn green. Source anchors detect moved/deleted evidence, not whether
the implementation still means the same thing. That needs review.

Keep these distinctions:

- **Intended rule:** a language/design decision, with its rationale and scope.
- **Observed behavior:** what a specific program or implementation does now.
- **Discrepancy:** intended/prose behavior and implementation disagree; do not
  promote the implementation to the specification automatically.
- **Test gap:** a check cannot distinguish the intended behavior from a
  plausible defect. Passing tests alone do not close this gap.

A passing discrepancy probe means the discrepancy still reproduces, not that
the compiler is correct. Resolving one requires changing its record and checks
alongside the decision and implementation/documentation.

## Initial audit, 2026-11-15

Baseline source revision: `fdaab188`. This is a small audit, not a full-suite
certification. The machine-readable ledger is the rerunnable subset.

| Finding | Evidence | Status |
|---|---|---|
| Guide fences match files, runnable examples match expected output, shaders compile | [guide checks](../crates/rock-cli/tests/guide.rs) | Three tests passed; no claim that examples cover every semantic rule |
| CLI MIR verification is opt-in, including `rock test`; its printer scope guard is on by default for tests | [gate and its test](../crates/rock-cli/src/process.rs), [verifier default](../crates/skull-mir/src/verify.rs) | Gate test passed; clarified the architecture guide's ambiguous wording |
| Five tested value shapes share representation decisions across body/call/return/publisher | [plan test](../crates/skull-mir/tests/abi_plan.rs), [implementation](../crates/skull-mir/src/lower/abi.rs) | Focused test passed; not a claim about every type/backend |
| Unqualified shift masking in the reference disagreed with `uint` literal shift rejection | [reference](language-reference.md#operators-and-references), [inference](../crates/bone-inference/src/expr.rs), [reproducer](../perf/evidence/uint_shift.rock) | Initially unresolved; corrected in the numeric operator cleanup below using the original fixture |
| Numeric examples pass without exposing that discrepancy | [numeric edges](../tests/operators/numeric_edges.rock) | 11 passed on LuaJIT and 11 on JS; out-of-range masking examples use `int` |
| 395 of 514 rejection cases lack a nonempty expected diagnostic | [runner](../crates/rock-cli/src/error_test.rs), inventory case list | Review queue: unrelated rejection can satisfy these cases; not 395 proven compiler bugs |
| Plain Cargo commands exclude ten workspace members, not only the GLSL runner | [workspace manifest](../Cargo.toml), inventory workspace table | Corrected stale testing documentation |

The reference corpus also passed 25 LuaJIT cases in
[refs.rock](../tests/values/refs.rock). That is observed execution, not a full
audit of reference lifetime/aliasing rules.

## First completed cleanup: numeric operators

The [operator audit](numeric-operators-decisions.md) resolves the shift
discrepancy from the original `uint` fixture and the later guard-elimination
contract. The reference now distinguishes `int` masking from checked `uint`
counts. Compiler behavior is unchanged.

All 18 unsupported-operator cases now pin their symbol-specific diagnostic:
the unpinned count at that stage was **377 of 514**, down from 395. The 514-case
inventory includes six existing TODO cases; the executed error suite reports
508 passed and six TODOs. The evidence checker then had seven records, including
both backends' dynamic shift checks and deliberate mutations of this error
family. Wrong diagnostics and valid replacement programs each fail all 18 cases.

## Reference suspension and compiler admission

The [reference audit](reference-suspension-decisions.md) adds 12 semantic
fixture sections, six diagnostic-pinned rejection cases and four runtime
controls on LuaJIT and JS. A disabled suspension policy was detected and
restored. Wrong diagnostic and valid-program controls fail all six rejection
assertions. The static rejection inventory is now 520 cases, with 377 unpinned;
the six new cases did not weaken existing assertions.

The same audit reproduced an open construction-time discrepancy: an explicit
ref can survive between coroutine creation and first resume, whereas implicit
identity parameters are rejected at construction. The executable record
`reference-construction-discrepancy` preserves the observation without adopting
it as the intended lifetime rule.

The [admission audit](compiler-admission-decisions.md) adds nine compiler-entry
tests. Bypassing the initial syntax, expanded-syntax or macro gates makes their
targeted checks fail. Valid controls reach real Lua emission. One existing
editor-query test checks partial recovery in-process, not editor transport.
These gates serve different stages; the audit found no justified consolidation.

Those thirteen evidence records are runnable through the shared checker.

## Small production and test-discovery repairs

Removed the write-only `coroutine_rejections` map from
[`instance_graph.rs`](../crates/skull-graph/src/instance_graph.rs). Its only uses
were initialization, insert, remove and clear. `graph_rejections` retains the
published diagnostics and `coroutine_positions` retains incremental removal
positions. The deletion also eliminates two clones during installation. No
performance measurement or speedup is claimed.

Enabled the existing `allow_lines_tests::matches_str_lines_on_allow_lines` test
in the ordinary [`bone-lints`](../crates/bone-lints/Cargo.toml) suite. Before
the fix, an unfiltered crate run executed only 119 integration tests; afterward
it also executes the one unit test. A filtered Cargo invocation ran that unit
test even before the fix, so a filtered pass alone would not verify this repair.

Combined validation passed 120 bone-lints tests, five skull-graph tests, and
1404 skull tests with one existing ignored test. The semantic fixture harness
also permits empty selections, like ordinary filtered Cargo runs; the evidence
checker already requires explicit nonzero counts. No harness policy was changed.

## Failed overlays, graph counts and purity assertions

The [hot-reload audit](hot-reload-rollback-decisions.md) reproduced the direct-call
rollback gap: after a late initializer failed, retained CodeRefs stayed old but
ordinary callers could reach already-replaced functions. A reload-local journal
now restores emitted global bindings and aborts pending slot publication. Two
runtime regressions cover failure, restoration of constants and definitions,
and a successful retry. Initializer side effects remain outside this contract.
The audit also reproduced and repaired static-reset and removal retries.
Cumulative overlays now retain transition requests, and runtime initializer
keys decide whether they have already applied. Two additional regressions check
failed retries, successful repeated loads and preservation of accumulated state.

The [graph cleanup](instance-graph-cleanup-decisions.md) shares the repeated
root-hole calculations while retaining full-refresh and local-refresh scopes.
It also fixes concrete-instance counts for mangled generic functions by retaining
the failed call's declaration identity. Tests compare incremental results to
fresh builds and check bounded work counters. These are work measurements, not
a wall-clock speedup claim. The follow-up span repair indexes authoritative
contribution rows instead of cloning them; four byte/line insertion/deletion
variants now agree with fresh builds without rewalking retained owners.

All 20 [purity rejection cases](purity.md#rejection-assertion-audit-2026-11-15)
now pin intended diagnostics; 15 previously accepted any rejection. The loop
fixture isolates its mutation from unrelated global reads. Wrong diagnostics
and valid replacement programs each fail all 20 assertions; removing `@pure`
admits all 20 original programs. The inventory at that stage was **362 unpinned of 520
rejection cases**, down from 395 of the original 514. These are assertion gaps,
not a count of known compiler defects.

## Selected language contracts, casts and null predicates

The [compact language contract](language-contract.md) indexes selected value,
reference, nullable, generic and coroutine rules with source and test links.
It distinguishes established rules from bounded observations and gives concrete
alternatives for the unresolved reference-at-construction boundary without
silently choosing a language change. Five selected LuaJIT runtime files are
now part of the rerunnable ledger; they do not certify every statement or
backend in the document.

The [checked-cast audit](checked-cast-decisions.md) pins all 12 rejection reasons
and removes downstream calls through two already-invalid lambdas. The 22 nearby
accepted conversion/ascription cases pass on LuaJIT and JS. Wrong-diagnostic
and generic-valid mutations fail all 12 rejection assertions.

The [null-predicate audit](null-refinement-decisions.md) reproduced three invalid
program shapes admitted because `is_null` was recognized by name. Both predicate
recognition and its effect exemption now use the selected stdlib declaration.
Five semantic sections, four pinned rejection cases and five runtime controls
distinguish declaration identity, effects and local function-value shadowing.
The full nullable corpus passes 415 tests on each backend. The rejection
inventory at that stage was **350 unpinned of 524 cases**; the full error scan reports
518 passed and six existing TODOs.

## Embedded fields, map keys and reserved names

| Family | Pinned rejection cases | Accepted controls |
|---|---|---|
| [Embedded projections](embed-projection-rejections.md) | 10 existing cases | 39 Lua cases; 35 JS cases and four Lua-only cases; two new selector-admission controls |
| [Map keys](map-key-rejections.md) | 9 existing cases | 10 newtype-key and 46 scalar/enum cases on each backend |
| [Reserved runtime fields](reserved-field-rejections.md) | 8 existing cases plus the missing enum `__ty` case | One nearby-name test covering structs, value structs and enum records on each backend |

Wrong diagnostic and valid-program mutations fail every rejection assertion in
these families. This strengthens 27 existing cases and adds one pinned case;
the inventory at that stage was **323 unpinned of 525 cases**. The full error scan passes 519
cases with six existing TODOs. No production compiler code changed.

The audit corrected stale map-key and embed-layout comments. It also keeps the
limits explicit: the embed runtime suite contains an inverted reflection
sentinel that passes because a known defect remains, and the Map key checker
has source-scan and newtype-recursion limits. Green corpus counts do not erase
those limitations or establish exhaustive enforcement.

## Control flow, opaque representation and iterator bounds

| Family | Existing cases strengthened | Nearby accepted controls |
|---|---|---|
| [Control flow](control-flow-rejections.md) | 8: tuple elements, final `else`, loop context | Eight new statement/value boundary tests on Lua and JS |
| [Opaque representation](repr-opaque-rejections.md) | 7: annotation placement, body and argument forms | 11 Rust boundary checks and five runtime push controls on each backend |
| [Iterator bounds](iterator-bound-rejections.md) | 7: bound declaration, conformance and use | 58 runtime cases on each backend, including direct multi-value iteration |

All 22 original inputs already rejected for their intended reasons. Each now
asserts that reason, and wrong-diagnostic and generic-valid mutations fail all
22 assertions. The inventory at that stage was **301 unpinned of 525 cases**;
the full error scan remained 519 passed with six existing TODOs. No production
compiler code changed. The separate coroutine/iterator fixture was unaudited
at that stage. Opaque annotations remain author promises, and the control-flow
audit records scalar
`break`/`continue` branch typing as an adjacent behavior needing review.

## Coroutine rejection boundaries

The [coroutine audit](coroutine-rejections.md) pins ten existing rejections:
four result fall-through cases, three lambda yields, resume-argument iteration,
removed delegation syntax and an invalid result annotation. The last case had
claimed to test a residual bounded-call restriction but rejected its invalid
`co fn` signature first. Its corrected iterator chain now has a runtime control.

Wrong-diagnostic and valid-program mutations fail all ten assertions. Six
nearby runtime files pass 28 tests on each backend, with no JS skips. The
inventory at that stage was **291 unpinned of 525 cases**; the full error scan passes 519 with
six existing TODOs. No production compiler code changed.

## Static locals and nested functions

The [hoisted-capture audit](hoisted-capture-rejections.md) checks seven static
and eight nested-function rejections, adding 14 missing diagnostic assertions.
Three fixtures lost unrelated rejection causes: mutation after a pure static,
uses of a rejected coroutine static and `len` on an iterator after an invalid
nested capture. The generated-looking-name fixture now explicitly limits its
claim; it does not establish privacy of the current emitted cell name.

Wrong-diagnostic and valid-program controls fail all 15 rejection assertions.
The runtime corpus, including four new accepted controls, passes 19 tests on
each backend without JS skips. The inventory is **277 unpinned of 525 cases**;
the full error scan passes 519 with six existing TODOs. No production compiler
code changed.

## Useful measurements

| Measure | Use | Interpretation boundary |
|---|---|---|
| Physical/nonblank lines by crate, file role, language and origin | Locate bulk; distinguish test fixtures and upstream-derived code | Comments/blanks are included in physical lines; inline tests remain mixed with source; size is not a defect |
| Largest files and declared local crate dependencies | Pick implementation areas to inspect | Moving code between files does not simplify its dependencies or rules |
| Static test markers and rejection cases by crate/corpus directory | Find where checks live and where diagnostics are weak | Not executed counts or semantic coverage; snippets/macros/custom harnesses distort marker counts |
| Audited claims and open discrepancies | Track how much we understand and what needs a decision | No percentage coverage without an agreed feature inventory; no reward for splitting one claim into ten |
| Existing compile/runtime/output measurements | Check costs before and after an affected cleanup | Use the same corpus and comparable environment; see [performance tooling](../perf/README.md) |

For one cleanup, record the rule, implementation sites, relevant tests, before
and after measurements, and what duplicated decision or machinery disappeared.
Inspect test assertions and use a targeted deliberate break where practical;
do not merely add more tests. Keep a regression even if several tests look alike
until their behavioral purposes are understood.

## Next actions and open questions

Use the ranked queue in [subsystems.json](subsystems.json), rendered in the
overview. It separates the unresolved reference-lifetime decision from a
deferred JS reload audit and records the completed null-refinement correction,
with file ownership and acceptance checks. JS reload parity has not been
audited by this work.

The breadth survey also found a stale LSP fixture warning: the parser already
rejects unknown expectation lines. Its existing malformed-block regression
passes; the documentation now matches that behavior. The ledger contains 29
bounded records, not 29 comprehensively verified subsystems.

Extend the evidence ledger alongside concrete repairs. Its checks establish
bounded claims, not a percentage of language coverage or a launch certificate.
