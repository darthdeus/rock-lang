# Value return boundary

This CPU-only probe isolates a conditional value return with a local binding
(issue #43). The selected `Receipt` contains an optional wide `Landing`; the
inputs differ and the flag alternates. The direct-return form is a control.
Results escape into a 256-entry ring. Before and after the loop, the driver
checks every field, both selections and a Rock value update that must preserve
previous results and inputs. It does not model an application workload.

```sh
python3 benchmarks/call_boundaries/measure.py BASELINE_ROCK CANDIDATE_ROCK > results.json
```

The compilers must be executable files. The script compiles the same source
with each, records SHA-256 hashes for source, driver, compilers and outputs,
and records the LuaJIT version. Use `--luajit PATH` to select LuaJIT and
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
