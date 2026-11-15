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

Mesh results per 9300-quad iteration:

| Case/arm | Median us | p95 us | Gross Lua bytes | Measured aborts |
| --- | ---: | ---: | ---: | ---: |
| Normal/growth | 2725.05 | 11307.49 | 2106866 | 9 |
| Normal/early | 2141.23 | 10045.77 | 2106967 | 11-12 |
| Normal/reserved | 1312.13 | 3505.34 | 225795 | 6-7 |
| Corners/growth | 1503.75 | 5400.25 | 2106863 | 9-11 |
| Corners/early | 1364.47 | 4787.10 | 2106967 | 11-15 |
| Corners/reserved | 1073.48 | 2213.94 | 225795 | 7 |

Neither mesh case reproduces `NYI: register coalescing too complex`, in
warmup or measurement. Moving growth earlier does not remove current aborts.
The recurring NYI sites are `UCLO` and `FNEW` in `__rock_nb_alloc`: the return
from its small FFI-array branch and construction of the off-heap finalizer
closure. Some runs also hit blacklisting or recording interruption.

The default 16384-byte off-heap threshold makes the reserved control a
materially different allocation workload: it starts directly with a large
buffer while growth allocates several intermediate small buffers. The large
Lua-allocation reduction is not a reduction in total native bytes of the
same magnitude. Host contention and GC produce wide timing ranges (stored
in the JSON); do not attribute the timing difference to a coalescing fix.

All 144 final processes passed their content checks. #264 remains open:
these bounded public cases do not prove the historical witness impossible.
The next concrete investigation is NativeBuffer allocation/finalizer trace
behavior, with ownership and collection safety preserved; adding a reserve
API or removing capacity guards is not justified by these results alone.
