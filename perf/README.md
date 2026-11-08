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
perf/report.py                    # write perf/report.html from the history
perf/backfill.py --since 2026-11-01 --step 5   # measure past commits
```

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
