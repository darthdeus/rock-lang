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
the current unpinned count is **377 of 514**, down from 395. The 514-case
inventory includes six existing TODO cases; the executed error suite reports
508 passed and six TODOs. The evidence checker now has seven records, including
both backends' dynamic shift checks and deliberate mutations of this error
family. Wrong diagnostics and valid replacement programs each fail all 18 cases.

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

1. Audit one reference/aliasing concern through its documented rule, admission
   checks and runtime behavior; choose a specific boundary before editing.
2. Strengthen that concern's rejection tests with diagnostic assertions and
   valid-program controls, using the operator family as the worked example.
3. Simplify implementation only where that audit identifies duplicated rules
   or unnecessary machinery; retain the original regression evidence.

Open: which concern should be the first substantial cleanup; which documented
behaviors are deliberate language commitments versus implementation accidents;
which feature boundaries make cross-subsystem test mapping useful. Expand the
ledger as these are answered, not by mechanically summarizing every function.
