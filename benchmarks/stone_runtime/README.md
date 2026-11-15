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

Use `--case segment` for the ribbon writer (the same three capacity arms). Use `--case bookkeeping`, `--case shader`, or
`--case parametric` with `--arm retained` for the renderer bookkeeping probes.
These use the same separate timing, allocation, and trace processes and can be
compared across revisions with `--compare`.


Use `--independent-corners` for distinct computations in all 16 optional
corner lanes. `--corners` reuses existing position/tint values instead; omitting
both options uses scalar defaults. Use the repository's job scheduler where required.
The GL workload needs a working desktop/headless graphics provider;
`STONE_HEADLESS=1` hides the owned window, it does not emulate a GPU.

`prepare.py` archives tracked packages and stdlib from HEAD (or from the
commit supplied with `--source-revision`), copies this benchmark, then compiles
two isolated arms with the specified compiler. The fresh arm changes only
`scene_return_pass` back to constructing a Pass. `build.json` records the
resolved source commit, hashes of the copied benchmark inputs, compiler and
outputs. Uncommitted engine edits are excluded. The compiler hash does not
establish its source provenance; record that separately.

To compare committed engine candidates in alternating processes, prepare each
revision with the same benchmark inputs, then select one workload arm:

```sh
python3 benchmarks/stone_runtime/prepare.py /tmp/stone-baseline --source-revision BASELINE --compiler /path/to/rock
python3 benchmarks/stone_runtime/prepare.py /tmp/stone-candidate --source-revision CANDIDATE --compiler /path/to/rock
python3 benchmarks/stone_runtime/measure.py /tmp/stone-baseline \
  --compare /tmp/stone-candidate --arm growth --case mesh --independent-corners \
  --runtime /path/to/gamelibs --stone-lib /path/to/libstone_rs.so --outdir /tmp/stone-paired
```

The results include both build records and label each sample baseline or
candidate. Check that benchmark input hashes match and record compiler and
native-library provenance. Fewer aborts alone does not prove more work ran
in compiled code: inspect LuaJIT VM-state sampling in separate profiling
runs as well as abort sites, allocation counts, timings, and the vertex oracle.
Profiling must not be enabled during timing runs. Add
`--require-no-coalescing` to fail when a trace process reports a warmup or
measurement coalescing abort. In paired runs the gate applies to the candidate;
in single-build runs it applies to all selected arms.

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

Bookkeeping: 120 iterations each of world and UI batch resolution with two
textures, four layers, repeated plain/shared state and twelve nonshared
occurrences per pool. Each synthetic frame clears the pools and advances the
render frame; a sink checks the resolved layers. Shader: 120 alternating
lookups of two live registered shader interfaces followed by per-draw sampler
marks and audits, checking the declared slot counts and missing-slot result.
Parametric: 320 calls to `UiRect.parametric_sprite` with one state key, including
the first 256-quad batch and a 64-quad overflow batch. The oracle checks batch and vertex counts plus untimed parameter values. These cases measure bookkeeping and
queue construction, not GPU submission or complete application frames.

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

## Automatic mesh growth preparation

Ordinary SpriteMesh and RetainedMesh append calls now prepare capacity before
eligible argument construction automatically. The compiler snapshots primitive
inputs, checks the argument expressions and called helpers, runs preparation
once, then calls the writer tail. No caller reservation is needed for the
independent-corner and ribbon workloads. Unsafe or unsupported calls, including
omitted defaults and already-built value arguments, keep ordinary evaluation.
See `docs/guard-elimination.md` for the trusted prepare/tail contract and proof
boundaries. Capacity, span and GC-pressure checks remain.

`reserve_quads` remains useful when a complete batch size is known: it reserves
additional quads with one allocation/copy. The `early` and `reserved` benchmark
controls remain available. They are not required by the automatic `growth` arm.

Final x86 comparison uses source `98e9602b` versus `dba982c3`, both compiled with
the release compiler from `dba982c3`, and the native library from `d2b2f964`.
Hardware/runtime are the same as above. Six alternating baseline/candidate pairs
per metric, separate timing/allocation/trace processes, 30 warmup and 120
measured iterations of 9300 primitives per process:

| Workload / version | Median us (process range) | p95 us | Gross Lua bytes/iteration | Warmup coalescing |
| --- | ---: | ---: | ---: | ---: |
| Independent quad / baseline | 871.06 (853.00-877.67) | 2573.34 | 2055856 | 5 in every run |
| Independent quad / automatic | 748.82 (735.24-911.61) | 2561.64 | 2056032 | 0 |
| Ribbon / baseline | 847.29 (821.02-2598.02) | 2634.74 | 2055827 | 5 in every run |
| Ribbon / automatic | 765.66 (733.23-794.44) | 2464.53 | 2055927 | 0 |

Neither version records measured coalescing; other trace interruptions remain.
Host contention produced outliers, visible in the process ranges. These are
synthetic fill results, not complete application-frame speedups.
Allocation volumes are essentially unchanged. All 72 comparison processes pass
the untimed full vertex oracle, including copied prefixes. A separate x86 run
also passes the existing ordinary quad trace constant budget without increasing
its limit.

Separate VM-state sampling uses four alternating pairs per workload, 2000
measured iterations and a 1 ms interval. Candidate compiled-code samples are
82.1% for quads and 82.0% for ribbons; interpreter samples are 0.17% and 0.16%
(baseline 1.42% and 1.20%). This rules out an interpreter fallback as the source
of fewer aborts. Sampling can alter recording; these are coverage diagnostics,
not timing measurements or estimates of GC savings. All 16 sampling processes
also pass the full vertex oracle.

```sh
python3 benchmarks/stone_runtime/profile_vm.py /tmp/runtime-baseline /tmp/runtime-candidate \
  --runtime "$(python3 scripts/gamelibs.py path --plat linux-x86_64)" \
  --stone-lib target/debug/libstone_rs.so --case mesh \
  --outdir /tmp/runtime-comparison/mesh-profile
```

Use `--case segment` for the second sampling workload. `results.json` retains
per-process timing, allocation, trace and sample counts plus build hashes.

## Renderer bookkeeping (#232)

The ordinary world/UI batch paths already use texture-handle candidate caches.
The remaining parametric UI path now builds its cursor key only when the base
batch fills. Shader admission combines private artifact generation/fingerprint
metadata into one lookup and checks program membership without reconstructing
its value. Public semantic mutations still invalidate admission; compatible
reloads restore the new registry without recompiling the program. Texture-slot
marking retains its existing representation and lifetime rules.

Source `7e14babe` versus `612b9853`, both compiled with the release compiler at
`dba982c3`, native library at `d2b2f964` (dev opt-level 1). Same x86 hardware and
LuaJIT as above, JIT and normal GC enabled. Six alternating pairs per metric,
600 warmup iterations, 2400 measured shader iterations and 1200 for each other
case. Timing, allocation and trace capture run separately.

| Workload | Baseline median us (range) | Candidate median us (range) |
| --- | ---: | ---: |
| 320 parametric UI quads, including overflow | 187.33 (180.92-199.51) | 116.76 (112.01-129.81) |
| 120 shader selections and texture-slot audits | 3.291 (3.166-3.395) | 3.105 (3.072-3.248) |
| 504 mixed world/UI batch lookups | 72.97 (72.41-76.82) | 74.11 (71.83-77.86) |
| Rendered scene-return control | 57.09 (56.36-57.41) | 56.75 (56.25-57.50) |

Parametric fill is about 38% faster in this workload; the small shader probe
improves about 6%. The controls overlap; no application-frame speedup is
established. Gross Lua allocation is essentially unchanged: parametric
18.31 -> 20.99 B/iteration, shader 0.838 -> 0.838, mixed 117.23 -> 118.13,
scene 1016.75 -> 1016.88. Trace interruptions remain. All 144 processes pass
their batch/parameter/slot/pixel checks. The regression suite additionally
checks all parameter lanes and vertex ordering across 520 quads and frame reset.

Eight separate parametric VM-profile processes (6000 iterations, 1 ms sampling)
also pass. Compiled/interpreter/C sample shares are 49.0/38.3/12.5% before and
44.7/35.5/19.7% after. Both versions retain substantial interpreter activity;
this change removes key work rather than making the entire path traceable.
Sampling changes recording and is not timing evidence.

`bookkeeping-results.json` holds per-process measurements and source/binary
hashes. Reproduce with `measure.py --case parametric --arm retained --compare`
using the two prepared revisions; `shader`, `bookkeeping`, and `scene` select
the controls. Use `profile_vm.py --case parametric --frames 6000 --warmup 600`
for separate coverage sampling. A redundant texture-mark write-skipping
candidate was not retained: its comparison encountered contention and did not
establish a gain.
