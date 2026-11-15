# Stone runtime allocation and trace probes

Public, source-compiled workloads for #236 and #264. JIT stays enabled,
normal GC stays enabled, and timing, gross Lua allocation and trace capture
run in separate processes. No production code changes are required.

## Reproduce on Linux x86_64

Build the compiler and native library from the revision being measured:

```sh
cargo build --release -p rock-cli
cargo build -p stone-rs
python3 scripts/gamelibs.py sync --plat linux-x86_64
python3 benchmarks/stone_runtime/prepare.py /tmp/stone-runtime
python3 benchmarks/stone_runtime/measure.py /tmp/stone-runtime \
  --runtime "$(python3 scripts/gamelibs.py path --plat linux-x86_64)" \
  --stone-lib target/debug/libstone_rs.so \
  --case scene --outdir /tmp/stone-runtime/scene
python3 benchmarks/stone_runtime/measure.py /tmp/stone-runtime \
  --runtime "$(python3 scripts/gamelibs.py path --plat linux-x86_64)" \
  --stone-lib target/debug/libstone_rs.so \
  --case mesh --outdir /tmp/stone-runtime/mesh
```

Repeat mesh with `--corners` and a separate output directory for optional
corner-lane pressure. Use the repository's job scheduler where required.
The GL workload needs a working desktop/headless graphics provider;
`STONE_HEADLESS=1` hides the owned window, it does not emulate a GPU.

`prepare.py` archives tracked packages and stdlib from HEAD, copies this
benchmark, then compiles two isolated arms with the specified compiler.
The fresh arm changes only `scene_return_pass` back to constructing a Pass.
Uncommitted engine/compiler edits are excluded. Build the compiler at HEAD;
its SHA is recorded but the script cannot establish its source provenance.

`measure.py` alternates process order over six rounds and records compiler,
Lua output and native-library hashes, LuaJIT identity, per-process medians
and p95s, allocator counts and trace abort locations. Raw logs stay in the
requested output directory. Allocation uses Stone's native Lua allocator
counter, not stopped-GC heap growth; it excludes off-heap buffer payloads
and GPU allocations. Trace hooks and allocation counters are absent from
timing runs. Results summarize process medians, not pooled samples.

## What the workloads establish

Scene: 600 warmup and 2400 measured iterations. A stable scene scope renders
one rectangle, applies a color-halving fullscreen pass to the partner canvas,
and returns it to the screen canvas through the actual scene-return path.
The center pixel must contain the halved color, so omitting the return blit
fails the check. Each timed iteration includes batch submission, both passes
and `stone_gl_finish()`. It excludes game update, events and present/swap;
this is a synthetic renderer iteration, not a complete application frame.

Mesh: 30 warmup and 120 measured iterations, each creating 100 meshes and
writing 93 quads per mesh (9300 quads). `growth` starts at capacity 8 and
uses the production growth guard. `early` calls the same production growth
helper before computing quad values. `reserved` starts at capacity 128.
All arms check final count/capacity and position content. Optional corner
lanes are a separate stress case, not the original reported witness.
The early arm still allocates/copies the same buffers; the reserved arm
avoids intermediate buffers entirely, so its difference cannot be attributed
solely to trace aborts.

## Current evidence

Measured 2026-11-15 at engine/compiler `8213867e`, Linux x86_64,
Intel Core i9-14900K, NVIDIA RTX 4090, bundled gamelibs v0.6.18 and
LuaJIT 2.1.1794164915. The native library was built from the same revision with the normal dev
profile (opt-level 1); the compiler used the release profile.
The host had concurrent unrelated work; wide timing ranges prevent a
speedup claim. See `results.json` for per-process summaries and hashes.

Scene results (median of six process medians/p95s; parentheses are the
minimum and maximum across processes):

| Arm | Median us | p95 us | Gross Lua bytes/iteration | Measured aborts |
| --- | ---: | ---: | ---: | ---: |
| Fresh | 96.50 (86.93-150.13) | 288.11 (185.94-437.04) | 2553.68 (2552.30-2556.68) | 23-32 |
| Retained | 97.43 (93.23-144.25) | 286.68 (248.70-483.11) | 1016.44 (1016.35-1018.75) | 20-25 |

The retained scene-return command removes about 1537 bytes per iteration
(60% of this workload's gross Lua requests). Timing distributions overlap;
this does not establish a frame-time speedup. Both arms still encounter
loop-shape/trace-length aborts; the cache does not make the renderer abort-free.

The allocator creates finalizer closures outside its hot return path and
reuses them by payload byte count through a weak-value cache. The closure
captures only the stable stats table and byte count, never a buffer pointer.
LuaJIT's internal FFI finalizer table keeps the callback alive until its
buffers finalize; unused cache values can then be collected. Ordinary weak
buffer metadata cannot replace these captures: LuaJIT clears those keys
before running cdata finalizers.

Direct JIT regressions pass on x86 and arm64: the small-allocation loop has
zero bytecode-NYI aborts, and repeated large allocations go from 13 to zero.
Tests also verify retained buffers, partial frees of shared-size allocations
(including different element types), zero live bytes after finalization, and
collection of unused callbacks. A cold cache miss still creates a closure.

Mesh comparison: identical emitted fixture with only `NB_ALLOC_PRELUDE`
replaced, baseline `acd0f843` versus the weak-value cache. Six alternating
baseline/candidate pairs per metric, 108 processes, same native library and
runtime as above. Each iteration writes 9300 quads. Hashes and per-process
samples are in `results.json`.

| Arm | Median us before/after | p95 us before/after | Gross Lua bytes before/after | Measured aborts before/after |
| --- | ---: | ---: | ---: | ---: |
| Growth | 948.98 / 805.04 | 2953.59 / 2650.63 | 2097363 / 2056026 | 2-4 / 0-2 |
| Early | 917.06 / 691.14 | 3161.13 / 2416.64 | 2097383 / 2055945 | 2-6 / 0-4 |
| Reserved | 1110.37 / 987.65 | 1538.05 / 1263.97 | 225796 / 205423 | 9-10 / 0-1 |

All 108 processes passed position/count checks. Lower process medians appear
in five of six paired growth/reserved runs and all six early-growth runs.
These are JIT-on synthetic mesh-fill measurements, not application frame
speedups. Per-process ranges include scheduling/GC variation.

The candidate records no `FNEW` aborts in these captures. Growth still sees
coalescing failures during warmup: three of six processes record five each,
at the off-heap size-guard exit and its continuation into the caller loop.
None records coalescing during measurement. Early growth and reserved
controls record none in either phase. Recording interruptions, short traces
and return-to-lower-frame aborts remain; see the per-arm reasons in JSON.

The reserved control starts directly with an off-heap buffer at the default
16384-byte threshold; growth creates intermediate Lua-managed arrays. Its
lower Lua allocation count is not an equivalent reduction in total native
bytes. Capacity and GC-pressure guards remain necessary. #264 now concerns
the off-heap transition's warmup side trace; the finalizer cache does not
establish that arbitrary growth callers are free of coalescing failures.
