# Value return boundary

This CPU-only probe isolates a conditional value return with a local binding
(issue #43). The selected `Receipt` contains an optional wide `Landing`; the
inputs differ and the flag alternates. Both `if` and `match` forms are measured;
the direct-return `if` is a control.
Results escape into a 256-entry ring. Before and after the loop, the driver
checks every field, both selections and a Rock value update that must preserve
previous results and inputs. It does not model an application workload.

```sh
python3 benchmarks/call_boundaries/measure.py BASELINE_ROCK CANDIDATE_ROCK > results.json
```

To compile on one machine and measure on another, prepare a new directory,
transfer it and this runner to the measurement host, then run:

```sh
python3 benchmarks/call_boundaries/measure.py --prepare-only artifacts BASELINE_ROCK CANDIDATE_ROCK
# Transfer artifacts/ and measure.py to the measurement host.
python3 measure.py --artifacts artifacts > results.json
```

The bundle includes the fixture, driver, emitted Lua and build.json; the runner
verifies their recorded hashes before measuring. The compilers must be executable
files. The script records SHA-256 hashes for source, driver, compilers and outputs
(using compiler file names, not paths), and records the LuaJIT version.
Use `--luajit PATH` to select LuaJIT and
`--pairs N` to override the default six alternating pairs. Run on the same
otherwise quiet machine; retain the JSON and machine details when reporting.

Each sample is a fresh LuaJIT process with JIT enabled. After 50,000 warmup
calls, timing runs 500,000 calls with normal GC and reports CPU ns/call.
Separate 5,000-call runs stop GC after warmup and collection: heap bytes/call
is uncollected Lua heap growth, a proxy, not gross allocator traffic. Separate
fresh processes attach the LuaJIT trace callback and report starts, stops and
aborts across warmup and measured work; their timing is not the timing result.
The JSON retains every sample and timing/heap medians. The runner asserts that
sinks match across all processes. The direct control can also be run alone:

```sh
luajit benchmarks/call_boundaries/driver.lua EMITTED.lua direct time
```

## Current x86 evidence

Measured 2026-11-15 on an Intel Core i9-14900K, Linux x86_64, bundled gamelibs
v0.6.18 / LuaJIT 2.1.1794164915. Baseline release compiler built at `dba982c3`
(production compiler sources unchanged through `cd34f5f8`), candidate at
`38463a0b`; both use the same fixture and standard library. JIT is enabled and
timing uses normal GC. Six alternating pairs per timing/heap case and one
separate trace pair per case: all 78 processes pass the full-field checks.

| Return shape | Baseline CPU ns/call (range) | Candidate CPU ns/call (range) | Heap proxy B/call before / after |
| --- | ---: | ---: | ---: |
| Local `if` | 364.09 (255.86-419.41) | 3.04 (2.09-3.10) | 1760 / 0 |
| Local `match` | 404.74 (251.82-421.01) | 3.26 (2.25-3.73) | 1760 / 0 |
| Direct return control | 3.63 (3.51-5.35) | 3.39 (2.94-4.42) | 0 / 0 |

Concurrent host work widened timings, so no precise speedup factor is claimed.
The selection cases now run in the allocation-free control's range. This is a
small escaping-value microbenchmark, not an application-frame measurement.
The separate compiler regressions count two table constructions per selected
local call before the fix and zero afterward, preserving the selected child
identity. Trace probes report four aborts in both versions; removing allocations
does not make the whole driver abort-free. `results.json` retains every sample,
source/output/compiler hashes and measurement conditions.
