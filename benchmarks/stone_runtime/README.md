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

The byte-count local is declared after the storage-size guard. The guard
checks the same product, but that temporary no longer occupies an exit
snapshot slot. Actual mesh trace dumps contain 17 distinct numeric values
in the baseline threshold snapshot and 16 afterward. LuaJIT's x86 side-trace
entry must shuffle inherited values through 16 XMM registers; removing one
value resolves this witness without changing the allocation threshold.

Direct regressions cover small/large allocation tracing, buffer lifetime,
callback collection, and a storage transition with 15 live float parameters.
The latter fails with five coalescing aborts before the change and zero
afterward on bundled x86 LuaJIT, while checking all stored float values.
Higher-pressure synthetic cases with 16 or more live float parameters can
still fail: this is a reduced lifetime, not a general solution to LuaJIT register pressure.

Mesh comparison: identical emitted fixture with only `NB_ALLOC_PRELUDE`
replaced, baseline `0e9a94b8` versus the delayed byte-count local. Six
alternating baseline/candidate pairs per metric, 108 processes, same native
library and runtime as above. Each iteration writes 9300 quads. Hashes and
per-process samples are in `results.json`.

| Arm | Median us before/after | p95 us before/after | Gross Lua bytes before/after | Measured aborts before/after |
| --- | ---: | ---: | ---: | ---: |
| Growth | 685.94 / 686.19 | 2385.69 / 2332.76 | 2055921 / 2055985 | 0-2 / 0-3 |
| Early | 680.78 / 682.63 | 2385.44 / 2283.92 | 2055982 / 2056043 | 0-2 / 0-4 |
| Reserved | 988.62 / 997.88 | 1350.31 / 1345.65 | 205462 / 205872 | 0 / 0-1 |

Steady-state timing and allocation are effectively unchanged; no speedup is
claimed. The improvement is warmup tracing: baseline growth records five
coalescing aborts in two of six processes, versus zero in all six candidate
processes. The optional-corner comparison adds 108 processes: baseline growth
records five coalescing aborts in three of six trace runs, versus zero in all
candidate runs. Early/reserved controls have none. Neither arm records
coalescing during measurement. All 216 processes pass content checks.
Recording interruptions, short traces and return-to-lower-frame aborts remain.

The reserved control starts directly with an off-heap buffer at the default
16384-byte threshold; growth creates intermediate Lua-managed arrays. Its
lower Lua allocation count is not an equivalent reduction in total native
bytes. Capacity and GC-pressure guards remain necessary. #264's current
public normal/corner witnesses pass, while higher-pressure caller shapes
still require investigation; arbitrary growth callers are not proven safe.
