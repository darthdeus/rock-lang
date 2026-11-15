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
  --case mesh --independent-corners --outdir /tmp/stone-runtime/mesh
```

Use `--case segment` for the ribbon writer (the same three capacity arms).

Use `--independent-corners` for distinct computations in all 16 optional
corner lanes. `--corners` reuses existing position/tint values instead; omitting
both options uses scalar defaults. Use the repository's job scheduler where required.
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
uses the production growth guard. `early` calls the public
`reserve_quads(mesh, 1)` before computing quad values. `reserved` starts at capacity 128.
All arms check count/capacity and positions. After timing, counters and trace
hooks stop, frame zero runs again. With independent corners it verifies all
13 fields of every emitted vertex, including copied prefixes and duplicate
triangle vertices. Deliberately corrupting colorize makes this oracle fail.
The early arm still allocates/copies the same buffers; the reserved arm
avoids intermediate buffers entirely, so its difference cannot be attributed
solely to trace aborts.

## Current evidence

Scene measured 2026-11-15 at engine/compiler `8213867e`, Linux x86_64,
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

The allocator keeps finalizer captures outside hot returns, weakly caches
callbacks by byte count, and declares its byte-count local after the size
guard. These resolve the reduced/default-input witnesses; they cannot keep
arbitrary caller float values out of a growth exit.

## Independent corner values and public reservation

The current mesh fixture computes all 16 optional alpha/colorize/U/V fields
with distinct integer remainders and exactly representable binary fractions.
The source calls `sprite_mesh_add_quad` normally. The independent values
reproduce five warmup coalescing aborts in each ordinary-growth trace run.
Explicitly inlining the writer did not remove those aborts in a bounded probe.

`reserve_quads` is available for both SpriteMesh and RetainedMesh. It reserves
**additional** quads without changing the live vertex count. Exact fits are
no-ops; bulk requests choose a geometric capacity and allocate/copy once.
An empty buffer can start at capacity one. Retained GPU contents stay valid
because reservation does not alter vertices; a later append still invalidates
the upload as usual. Call it before constructing float-heavy arguments:

```rock
mesh.reserve_quads(1)
// Compute positions, tint and optional corner values, then append the quad.
```

When the complete append count is known, reserve that count once before the
loop. The normal append guards remain necessary for callers that do not reserve.

Measured source/compiler `d2b2f964` plus the reservation API overlay recorded
in JSON, with the same x86 hardware/runtime as above and a native library
built at `d2b2f964`. Six alternating process rounds per metric, 30 warmup and
120 measured iterations of 9300 quads, 54 processes total:

| Arm | Median us | p95 us | Gross Lua bytes/iteration | Warmup coalescing aborts |
| --- | ---: | ---: | ---: | ---: |
| Ordinary growth | 911.25 | 2688.05 | 2055823 | 5 in each run |
| reserve_quads before arguments | 797.94 | 2535.41 | 2056018 | 0 |
| Initially capacity 128 | 1040.79 | 1323.19 | 205832 | 0 |

Per-process growth medians are 893.83-951.49 us; early reservation medians
are 764.54-821.83 us. This is a JIT-on synthetic fill result, not an
application-frame claim. Early reservation follows the same four doublings
and copy volumes as ordinary growth. Initial capacity 128 skips intermediate
Lua-managed arrays, so its allocation count is a different workload and does
not measure equivalent savings in total native bytes.

All 54 final processes pass the complete independent-corner oracle. Neither
mode records coalescing during measurement; other trace interruptions and
return-to-lower-frame aborts remain. #264 stays open for unreserved callers:
the public API supplies the measured early-growth path without adding new
compiler transformations or weakening capacity/GC-pressure checks.

## Ribbon segment growth

`--case segment` builds adjacent ribbon segments with matching shared
cross-sections and independently varying positions, UVs, colors and depth.
It uses the same 100 meshes, 93 segments, 30 warmup and 120 measured iterations,
three capacity strategies and six alternating rounds as the quad probe.
After measurement, all 13 fields of all 55800 vertices are checked, including
copied prefixes and shared cross-sections. All 54 processes passed; deliberate
colorize corruption failed the validator.

On the same x86 CPU/runtime above, source `13f7320a` with compiler/native
`d2b2f964` reproduced five warmup coalescing aborts in every ordinary-growth
trace run. Early reservation and initial reservation had none. No arm had
measured coalescing; other trace interruptions remained with early reservation.

| Arm | Median us (process range) | p95 us | Gross Lua bytes/iteration |
| --- | ---: | ---: | ---: |
| Ordinary growth | 1901.16 (927.78-2426.33) | 6105.91 | 2055828 |
| reserve_quads before arguments | 977.86 (846.49-1580.42) | 3635.47 | 2055963 |
| Initially capacity 128 | 1081.89 (1069.65-1131.49) | 1648.57 | 205832 |

Host contention and overlapping ranges prevent a timing speedup claim.
The existing reservation API avoids this writer's growth coalescing witness;
no further production change is justified by these results. This is a public
SpriteMesh writer probe, not an end-to-end measurement of Stone's Trail path,
which uses a separate RenderBatch reservation wrapper.
