# Compiler performance tracking

Scripts that measure the compiler on the public corpora and chart the results
over commits. Three questions: how long is a cold build, how long is an
incremental rebuild after an edit, and how fast does the language server
answer.

## Measurements

`perf/run.py` builds `target/release/rock` (or takes `--bin`) and, for each
corpus (`packages/stone` and every project under `demos/` by default),
records:

| Metric | Source |
|---|---|
| cold `rock compile`, cold `rock check` | hyperfine wall time, several runs |
| time budget of one cold compile | `ROCK_COMPILE_STATS` record, fastest of three |
| parse, bone semantic, skull emit and setup phases | `rock backend-stats --format json` |
| body edit to overlay, signature edit to full rebuild | `rock incremental-stats --format json` |
| LSP update, hover, completion per edit kind | `rock lsp-survey --format json` |

Each invocation appends one JSON row to the history file
(`$ROCK_PERF_HISTORY`, default `~/.local/share/rock-lang/perf/history.jsonl`).
The row carries the commit, whether the tree was dirty, the machine
architecture and core count, and the load average, never a host name or a
user path.

```sh
perf/run.py                       # build, measure HEAD, append, print deltas
perf/run.py --no-build --runs 5   # quicker, reuse the existing release binary
perf/run.py --quick --no-build    # about 10 s: iterate on a change, see deltas
perf/report.py                    # write perf/report.html from the history
perf/backfill.py --from 7d1204f0 --step 10   # measure past commits
```

`--quick` runs 3 hyperfine runs and 3 iterations on `packages/stone` only,
skips the LSP survey, and writes to `quick.jsonl` beside the main history so
iteration rows never enter the tracked series. `--only cold,incremental`
narrows any run to the metric groups named (`cold`, `phases`,
`incremental`, `lsp`). Each run prints its headline numbers with the
percentage change from the previous row in the same file, so the loop while
working on a change is: edit, `cargo build --release -p rock-cli --bin rock`,
`perf/run.py --quick --no-build`, read the deltas.

## The time budget

`ROCK_COMPILE_STATS=<file>` makes `rock compile` append one JSON record with a
`budget` tree: process, then discovery, parse+stdlib, semantic, prepare,
codegen, write; codegen splits into the serial setup phases, the body
fan-out (wall window, summed per-pass unit time and the idle time that leaves
on the worker pool) and the serial assembly steps. Every node's `wall_ms` is
the sum of its children, with an `other` child for the remainder, so a phase
nobody timed shows up as growth in `other` rather than vanishing.

```sh
perf/budget.py packages/stone            # compile three times, print the fastest tree
perf/budget.py --record stats.jsonl      # print a record written elsewhere
```

`perf/run.py` records the same tree per corpus (`--only budget`) and the
report charts its main nodes over commits. Compare the fan-out's summed unit
times with each other, never with the wall time.

`perf/report.html` is generated and not committed. Open it in a browser; the
corpus selector switches every chart and the table.

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
- Measurements are noisy on a loaded machine. The row records the load
  average before and after; treat a point taken under load with suspicion
  and rerun it (`perf/run.py` appends; `report.py` charts the latest row per
  commit).
