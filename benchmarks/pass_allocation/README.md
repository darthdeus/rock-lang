# Pass construction

CPU-only probe for issue #236. Extract the current Stone `Pass`, `Pipeline`,
`Canvas`, input and supporting data declarations, then compile fresh and retained
construction through the same compiler. Only the `pub` visibility spelling is
removed for the standalone fixture. This avoids loading the graphics runtime;
it is not a frame-time or renderer benchmark.

```sh
python3 benchmarks/pass_allocation/prepare.py /tmp/pass-allocation.rock
target/release/rock compile /tmp/pass-allocation.rock --output /tmp/pass-allocation.lua
luajit -e 'dofile("/tmp/pass-allocation.lua"); main()'
python3 benchmarks/pass_allocation/measure.py /tmp/pass-allocation.lua > /tmp/pass-results.json
```

`--luajit` selects the VM and `--rounds` sets the number of alternating pairs
(default six). Run timing jobs on one x86 host with the JIT enabled. Keep the
compiler revision and source declarations fixed across both modes. JSON retains
every sample, the emitted-file SHA-256 and the VM version.

Each synthetic frame queues twelve passes with 1, 7 or 10 canvas inputs, then
consumes all input handles, filters and output handles and clears the queue.
The consumer checks expected contents every frame. Both modes use the same
queue and consumer. Fresh passes build literal input lists, matching the source
idiom; retained passes reuse twelve distinct records and rebuild tuples/enums
only when the input handle changes. Inputs either stay fixed or alternate every
60 frames. Initial retained construction is outside the measurement.

Timing includes construction/update, queueing, consumption and normal GC after
12,000 warmup frames, across 120,000 measured frames per fresh process. It is
reported as CPU ns/pass, not just constructor time. Allocation evidence is a
separate 600-frame run with collection stopped after warmup and a full collect:
the metric is uncollected Lua heap growth in bytes/pass, not gross allocator
traffic. Small residual growth can include trace/runtime bookkeeping. Do not
use the stopped-GC run as the runtime performance result.

Uniform packing, native/GPU calls, shader validation, real scene-queue scheduling,
observer retention and hot reload are excluded. Reusing a record queued more
than once before consumption can change earlier queued work; this fixture uses
distinct records and finishes consumption before the next update. It does not
establish a safe general mutable-pass API.

## Current measurement

Compiler and declarations: `82647e11`, release build. Intel Core i9-14900K,
Linux, LuaJIT 2.1.1785763465, JIT enabled, six alternating fresh-process pairs
per cell (144 processes total). Shared host load at start was 8.5/7.4/6.2;
these are microbenchmark results, not an application speedup. All sinks matched.

| Inputs | Changes | CPU ns/pass fresh -> retained (median) | Fresh range | Retained range | Uncollected bytes/pass fresh -> retained (median) |
| --- | --- | --- | --- | --- | --- |
| 1 | None | 363.81 -> 13.43 | 345.88-371.49 | 13.05-16.68 | 1544.16 -> 0.20 |
| 7 | None | 797.13 -> 43.69 | 788.95-814.97 | 43.38-45.51 | 3024.18 -> 0.20 |
| 10 | None | 1009.97 -> 64.67 | 980.36-1245.83 | 57.29-102.60 | 3768.18 -> 0.23 |
| 1 | Every 60 frames | 358.49 -> 14.33 | 353.57-368.99 | 14.06-16.77 | 1544.21 -> 4.24 |
| 7 | Every 60 frames | 812.88 -> 55.91 | 790.24-849.87 | 53.91-61.35 | 3024.23 -> 28.19 |
| 10 | Every 60 frames | 1007.18 -> 75.10 | 984.59-1096.84 | 74.38-162.08 | 3768.25 -> 40.25 |

The emitted Lua contains scalar Canvas fields, not Canvas boxes. Remaining
allocation includes the Pass table, input wrapper/backing list, per-input tuple
and SampledTexture tables, and the default empty array payload. Reusing these
objects is worthwhile in this probe; queue ownership and observer lifetimes
still decide which engine API can safely make that reuse convenient.
