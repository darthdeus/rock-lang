# Compiler performance measurement

`python3 perf/inventory.py` writes a source-size and test-location inventory to
`target/inventory/index.html`. Track cleanup work in GitHub issues.

Scripts that measure the compiler on the public corpora: how long a cold
build takes and where its time goes, what the emitted Lua consists of, and
which source definitions cost the most to compile. The `rock backend-stats`,
`rock incremental-stats` and `rock lsp-survey` subcommands measure the
in-process phases, incremental rebuilds after an edit and the language
server's answers.

## Comparing two compilers

Compile speed is guarded by measurement, not by counting internal work in
tests. Before landing a change that could slow compiles, build the base and
the candidate and run `perf/compare.py base/rock cand/rock`. It times cold
compiles of `packages/stone`, `demos/match3` and `demos/testbed` and a
`rock test tests/` run, interleaving the two binaries, and prints each
workload's median wall time, peak RSS and the median cand/base ratio of
paired runs with a distribution-free 95% confidence interval (`noise` when
it includes 1.00). On a loaded machine every interval is wide; measure on a
quiet one. `--runs 20` narrows the range, `--only stone,tests` picks workloads,
`--phases` adds each compile's budget top level. With one binary it prints
the timings alone; run it with the same binary twice to see the machine's
noise floor.

```sh
perf/compare.py base/rock target/release/rock --only stone --phases
```

## The time budget

`ROCK_COMPILE_STATS=<file>` makes `rock compile` append one JSON record with a
`budget` tree: compile, then process start (first Rust instant to the
compile's own start), build vars (the `[build.vars]` commands), discovery,
parse+stdlib, semantic, prepare, codegen, global accessors (the program's
accessor directory), preamble (the runtime preamble the output opens with),
write; codegen splits into the serial setup phases, the body fan-out window
and the serial assembly steps, and carries the per-pass unit
time summed over every lowered body (the fan-out and the bodies assembly
lowers serially). Every node's `wall_ms` is
the sum of its children, with an `other` child for the remainder, so a phase
nobody timed shows up as growth in `other` rather than vanishing. The tree
ends when the record is written, so teardown after it (heap release and the
kernel unmapping the address space) is the part of the process wall it does
not contain.

```sh
perf/budget.py packages/stone            # compile three times, print the fastest tree
perf/budget.py --record stats.jsonl      # print a record written elsewhere
```

Compare the summed unit times with each other and across commits, never
with a wall window.

## Reading the numbers

- Cold builds are process wall time and include startup and output writing.
  The in-process `backend-stats` numbers exclude both and split the work into
  phases; emit phase times are per-unit elapsed sums that overlap under
  parallel emit, so compare them with each other, not with the wall time.
- `incremental-stats` drives the same `IncrementalProject` as `rock stone`:
  a body edit publishes `incremental.lua`, a signature edit (a trailing
  defaulted parameter) publishes a full `main.lua`.
- `lsp-survey` times the language server's in-memory update; its signature
  edit inserts a required parameter, which the server tolerates with
  diagnostics, so it exercises the error path that the hot reload rejects.
- Measurements are noisy on a loaded machine; `perf/compare.py` prints the
  load average before it starts. Treat a number taken under load with
  suspicion and rerun it.

## The output census

`perf/output_census.py` reads the Lua one compile wrote and reports what the
bytes consist of: top-level definitions by class (declared functions, generic
specializations, hoisted lambdas, global initializers, runtime support) and
the glue around them (runtime prelude and type tables, callee binding lines,
registration tables, global initialization); how many bodies repeat another
body byte for byte or after erasing the type suffixes that tell the
specializations of one generic apart, and which generics specialize most;
the structural boilerplate inside bodies (indentation, labels, gotos, control
lines such as `if`/`else`/`end`, register kills, declarations) and the share
of bytes spent on mangled symbols; and the largest bodies.

```sh
rock compile packages/stone --output /tmp/stone.lua
perf/output_census.py /tmp/stone.lua
perf/output_census.py game.lua --engine /tmp/stone.lua --mask   # private symbols print as hashes
ROCK_SKULL_INLINE_OFF=1 rock compile packages/stone --output /tmp/stone-noinline.lua
ROCK_SKULL_BOUNDS_OFF=1 rock compile packages/stone --output /tmp/stone-checked.lua
```

`ROCK_SKULL_INLINE_OFF=1` turns the function inliner off for one compile, so
the output and the time budget with and without splicing can be compared.
`ROCK_SKULL_BOUNDS_OFF=1` keeps every `[T]` bounds check, so the sites the
elision removes (`__vec_checked_get` / `__vec_checked_set` calls) can be
counted and timed against the checked output. Both are measurement switches
only; the emitted program still runs.


## Source costs

`ROCK_BODY_COSTS=<file>` makes a project `rock compile` write one JSON line
per emitted body, named by the source definition it came from (function,
generic specialization, coroutine, global initializer or decorator, with file,
package and line): the worker's stage times (`lower_ns` ... `regalloc_ns`,
elapsed on the worker; together they are the budget's `codegen.unit_cpu_ms`),
the worker's thread CPU for the body (`thread_cpu_ns`, for bodies the parallel
fan-out built), MIR size after lowering, after inlining and after cleanup, the
body's own Lua bytes and lines, the callees spliced into it, and, for a body
whose signature flattens a value, its wire (`wire`: per parameter and then the
result, the Lua values it occupies and the value type it flattens). It also writes
one line per module for the front-end passes that run module by module
(`lower functions`, `infer functions`). Without the variable nothing is
collected and the output is byte-identical either way.

`perf/body_costs.py` turns that file, the compile stats and the Lua into one
self-contained HTML report (offline, light and dark) and a Markdown summary:
ranked source changes with estimated savings, concentration (share of CPU in
the top 1/5/20% of definitions), sortable tables per definition, generic,
file and inlined callee, and output patterns traced to source lines (wide
signatures and calls, unread parameters, many upvalues, labels, table
constructors in loops, failure blocks), plus the value types whose flattened
leaves widen residual calls, ranked by the argument, parameter and result text
they add (positions of `--wide-lanes`, default 8, or more). A type is marked
worth changing, with advice, when a signature with two or more residual calls
carries it in more than 16 lanes (24 for `nobox`). Every CPU number is summed over
worker threads; wall numbers are labelled as wall.

```sh
ROCK_BODY_COSTS=/tmp/stone-costs.jsonl ROCK_COMPILE_STATS=/tmp/stone-stats.jsonl \
  rock compile packages/stone --output /tmp/stone.lua
perf/body_costs.py /tmp/stone-costs.jsonl --stats /tmp/stone-stats.jsonl --lua /tmp/stone.lua \
  --html /tmp/stone-report.html --md /tmp/stone-summary.md
perf/body_costs.py game-costs.jsonl --lua game.lua --root ../game --mask ...   # private names as hashes
```
