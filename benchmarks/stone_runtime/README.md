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

The current allocator extracts finalizer creation into a helper. Its
per-allocation closure still captures the same stats table and byte count,
but small returns no longer close upvalues (unrecordable `UCLO`). A direct
small-allocation regression records successfully: 11 NYI aborts before,
zero after, on both x86 and arm64. Large allocations still encounter `FNEW`.

Mesh comparison: identical emitted fixture with only `NB_ALLOC_PRELUDE`
replaced, baseline `cd61ec3f` versus the extracted finalizer factory. Six
alternating baseline/candidate pairs per metric, 108 processes, same native
library/runtime as above. Each iteration writes 9300 quads. Hashes and
per-process samples are in `results.json`.

| Arm | Median us before/after | p95 us before/after | Gross Lua bytes before/after | Measured aborts before/after |
| --- | ---: | ---: | ---: | ---: |
| Growth | 1091.93 / 940.65 | 3390.29 / 3066.46 | 2106967 / 2097473 | 9-11 / 2-5 |
| Early | 1103.83 / 877.09 | 3286.28 / 2783.44 | 2106916 / 2097386 | 10-15 / 4-7 |
| Reserved | 1087.02 / 1112.19 | 1598.70 / 1531.78 | 225795 / 225795 | 7 / 9 |

All 108 processes passed position/count checks. Host contention produces
wide timing ranges, so these medians are observations, not a reliable
application-speedup claim. In particular, reserved-buffer trace aborts
increase; the change does not make large allocation traceable.

Removing the earlier UCLO obstruction exposes a coalescing failure during
growth warmup. Twelve separate site-instrumented processes each record one
`NYI: register coalescing too complex` at the first off-heap instruction
following the size-threshold guard in `__rock_nb_alloc` (parent exit 1).
None records it during the measured interval. This is a public witness for
#264's failure class, not the old repeated-abort frequency. Trace sites now
include phase and reason so they can be attributed independently.

The reserved control starts directly with an off-heap buffer at the default
16384-byte threshold; growth creates intermediate Lua-managed arrays. Its
lower Lua allocation count is not an equivalent reduction in total native
bytes. Capacity and GC-pressure guards remain necessary. The remaining
investigation is the off-heap transition and finalizer `FNEW`, preserving
ownership and collection safety.
