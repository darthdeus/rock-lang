# Pass construction

CPU-only probe for issue #236. Extract the current Stone `Pass`, `Pipeline`,
`Canvas`, input and supporting data declarations, plus the production
copy-on-change helpers, then compile fresh and retained construction through
the same compiler. Only the `pub` visibility spelling is
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
idiom; retained passes call `with_canvas_input` for every slot every frame.
Unchanged slots return the same record; each changed slot copies the input list
and Pass. Inputs stay fixed, alternate every 60 frames, or alternate every frame.
Initial retained construction is outside the measurement.

Timing includes construction/update, queueing, consumption and normal GC after
12,000 warmup frames, across 120,000 measured frames per fresh process. It is
reported as CPU ns/pass, not just constructor time. Allocation evidence is a
separate 600-frame run with collection stopped after warmup and a full collect:
the metric is uncollected Lua heap growth in bytes/pass, not gross allocator
traffic. Small residual growth can include trace/runtime bookkeeping. Do not
use the stopped-GC run as the runtime performance result.

Uniform packing, native/GPU calls, shader validation, real scene-queue scheduling,
observer retention and hot reload are excluded. The production helpers preserve
previous submissions through copy-on-change; renderer tests separately cover
queued records, inspector retention, resize, output changes and real pixels.
Direct mutation of a shared Pass or its payloads is outside that guarantee.

## Current measurement

Compiler: `82647e11`, release build, with the production API and fixture in this
revision. Intel Core i9-14900K, Linux, LuaJIT 2.1.1785763465, JIT enabled, six
alternating fresh-process pairs per cell (216 processes total). Shared host load
at start was 4.8/14.2/12.1. These are microbenchmark results, not an application
speedup. All sinks matched.

| Inputs | Changes | CPU ns/pass fresh -> retained (median) | Fresh range | Retained range | Uncollected bytes/pass fresh -> retained (median) |
| --- | --- | --- | --- | --- | --- |
| 1 | None | 375.90 -> 20.39 | 367.36-380.37 | 19.42-21.60 | 1544.18 -> 0.19 |
| 7 | None | 828.61 -> 93.51 | 806.31-865.75 | 92.05-97.06 | 3024.18 -> 0.19 |
| 10 | None | 1013.33 -> 119.85 | 999.56-1030.68 | 117.09-122.15 | 3768.17 -> 0.21 |
| 1 | Every 60 frames | 379.52 -> 26.02 | 374.01-385.29 | 25.25-27.57 | 1544.21 -> 22.11 |
| 7 | Every 60 frames | 819.16 -> 154.95 | 784.68-839.45 | 148.75-161.31 | 3024.22 -> 158.89 |
| 10 | Every 60 frames | 1014.61 -> 228.55 | 1008.23-1031.48 | 220.76-243.33 | 3768.24 -> 237.59 |
| 1 | Every frame | 383.96 -> 351.38 | 379.32-524.08 | 344.37-419.39 | 1544.21 -> 1312.23 |
| 7 | Every frame | 816.75 -> 3116.96 | 795.28-851.55 | 3078.44-3267.33 | 3024.22 -> 9520.28 |
| 10 | Every frame | 1014.96 -> 4942.81 | 1000.29-1086.21 | 4884.55-5247.93 | 3768.24 -> 14240.25 |

Use retention for stable or occasionally changing inputs. Updating all 7 or 10
slots every frame copies the Pass/list repeatedly and is slower and more
allocating than constructing once. This API does not replace fresh construction
for high-churn passes. The one-input scene-return blit uses the retained path;
its end-to-end frame-time gain has not been measured.

Canvas fields remain scalar. Fresh allocation includes the Pass table, input
wrapper/backing list, per-input tuple and SampledTexture tables, and the default
empty array payload. Stable retained inputs show only small runtime bookkeeping
growth, not a claim of universally allocation-free rendering.
