# LuaJIT trace pressure

What limits a LuaJIT trace, the emitted shapes that follow from it, the rules
that the compiler and stdlib depend on, and the approaches already rejected.
The target is native LuaJIT, judged on x86 first. Larger mcode arenas or trace
limits move the capacity boundary without settling the trace population, so
the work is reducing distinct hot code and trace breakers in the emitted Lua,
not runtime policy. Tuning knobs (`maxirconst`, `loopunroll`, `hotexit`) and
penalty slots are excluded from further work by standing decision. The
prelude in `crates/skull/src/lib.rs` sets the runtime values and exposes them
as `ROCK_JIT_*` environment overrides; the test
`runtime_prelude_keeps_the_application_trace_working_set` pins the shape.

## What limits a trace

- **Side-trace register budget.** A side trace head needs every distinct
  non-constant, non-sunk value in the parent's exit snapshot live in a
  register, per class: 13 GPR and 16 FPR (12 and 15 when a move cycle must be
  broken), `asm_head_side` in `lj_asm.c`, with no fallback. Measured: ints
  clean to 12 and abort from 14; floats clean to 16 and abort from 17. What
  counts is distinct refs per class, not snapshot entries. The failure is
  `NYI: register coalescing too complex`. Only removing values from the
  parent's exit snapshot reduces these aborts; removing a guard after the
  exit, halving call frequency or eliminating one abort class does not, since
  the exit moves.
- **Doomed exits retry `256 - hotexit` times.** `trace_abort` penalises and
  blacklists root starts, not side exits. A whole-VM flush discards completed
  exits and root penalties.
- **Penalty cache.** `penalty_pc` has 64 round-robin slots; only root starts
  are penalised. A warm-up burst with more than 64 distinct hot starts evicts
  slots before they saturate. No `PENALTY_SLOTS` above 64 assembles on arm64:
  `vm_arm64.dasc` encodes `GG_G2DISP+GG_DISP2HOT` as a 12-bit immediate and
  `penalty[]` sits between `g` and the dispatch table.
- **Constant limit.** `maxirconst` is 500 IR slots. On GC64 each KGC and
  KNUM64 takes 2 slots; each inlined call site adds a frame-link KNUM, each
  inlined callee a KGC, each string key a KGC plus a KSLOT. About 40 inlined
  Rock functions exhaust it. Every "trace too long" abort is this limit, and
  it comes from emission. Raising the limit converts the class into unroll
  aborts at the same starts.
- **Strided-row offsets.** Fold `base + offset` into each store only through
  12 leaves; wider rows advance a running index. A uniform 50-leaf push used
  34 -> 81 constant slots on arm64 and 40 -> 183 on x86 with distinct offsets.
  On x86, narrowing retains both a two-slot KNUM and a one-slot KINT for an
  offset; seeing KINT operands in the final IR does not mean a one-slot cost.
  The constants are interned by value across stores: two consecutive 50-leaf
  pushes used 83/185 slots, not twice 81/183. A benchmark with 118 field keys
  and 30 small callees starts at 453/459 slots on arm64/x86. Compact 24- and
  50-leaf pushes hit the 500-slot limit on x86; 12 leaves stay at 488. This
  cutoff keeps all nine Stone append sites compact while bounding their
  extra constant cost. Isolated numeric-row benchmarks can hide the cost
  because the source's leaf expressions already use the same constants.
- **Native vertex offsets.** A numeric index increment or literal pointer
  increment can be narrowed and reassociated into a distinct byte-offset
  constant at each store. Stone's quad writers reuse the native buffer's
  cached float view and advance a pointer between vertices. The stride uses
  `len / len` after checking a positive destination span: a runtime-derived
  one prevents reassociation, and the division is loop invariant. Each vertex
  reuses the same 13 field offsets. Keep the field order and zero tails checked
  against typed `Vertex` stores. In 15 interleaved fresh-process rounds with
  the shipped LuaJIT, the arbitrary-quad loop used 174 -> 155 constant slots
  on Apple M1 and 192 -> 159 on Intel Core i9-14900K. Median JIT-on process CPU
  time was 11.49 -> 10.84 and 15.03 -> 12.56 ns/quad respectively; the accepted
  writers show no measurable slowdown. The [writer benchmark](../benchmarks/stone_vertex/README.md)
  retains exact emitted bodies and reports every trace and process sample.
- **Tail-call unroll.** Every recorded `CALLT` counts against `loopunroll`.
  The budget is spent by accumulation over many shallow forwarders, not deep
  chains.
- **Rangeless goto loops.** Rock emits non-counted loops as a backward
  `goto`, which LuaJIT turns into `BC_LOOP` without a range, so a root trace
  started there records past the loop exit. "Inner loop in root trace" is a
  root walking into an already-compiled loop. "Leaving loop in root trace" is
  a root started at a `for` header that left on the recorded iteration, or a
  small callee that became the root and returned to a lower frame (a `CALLT`
  into C is not a `RET`).
- **Snapshot pruning.** `snap_usedef` prunes dead slots only in the innermost
  frame and stops at the first jump. Caller-frame locals and function-top
  cells are never pruned; an assigned `nil` is a snapshot constant, not a
  parent SLOAD, and that holds across calls.
- **Integer versus float.** `max`-style branchless rewrites are for floats
  only: on ints LuaJIT narrows subtraction to `SUBOV`, itself a guarded exit.
  For equality chains use a sum of absolute differences, never `max`.
- **Mcode arena.** A small arena flushes the trace cache many times a minute,
  and flush plus recovery dominates over-budget frames. Flushes also erase
  penalties.

## Rules the emitted code depends on

Shrinking the live set at a guard is the fix pattern for doomed exits, per
site. Verify with the parent-exit hook; entry count is not the budget,
distinct refs per class is.

- Replace `&&`/`||` chains with one branchless float `max` compare, keeping
  the strictness of each side, and compute both operands before either test
  so the emitter's nil-kill drops the dead locals.
- Hoist acquisition out of loops so compare exits do not sit between live
  floats and their use. `@inline` splices only when the call-site arity
  matches the declaration.
- Keep saved state in stash cells rather than a `Mat4` local (it scalarises
  to 16 live floats) or a `Mat4` global (a store rebuilds the value).
- Splitting a float-heavy argument off a call needs a real frame: a `let` is
  folded back in, and the helper must stay un-`@pure` and un-`@inline`.
- Pre-size vectors that grow while traces record
  (`vec_new_with_capacity`): the array part growing bakes an array-versus-hash
  guard that fails forever. A state table that started as `{}` does the same
  through its `asize`; the prelude presizes the state table at its rehash
  fixpoint.
- Keep number keys non-negative on a table with a non-empty array part:
  `rec_idx_key` aborts `NYI: mixed sparse/dense table` for a negative
  non-constant key. String-keyed maps are immune.
- Keep closures out of frames that return early on a hot path: an early
  `return` above a closure compiles to UCLO, which is unrecordable.
- `crec_call` records a `bool` C return only with a Lua frame above it in
  the trace. `-> bool` externs declare `uint8_t` and the emitter compares
  `~= 0`.
- A one-line function ending in `return C.f(..)` compiles to `CALLT` into C,
  so `rec_ret` aborts "leaving loop in root trace" every time. The emitter
  binds `__rock_ret` in value constructors and every value-returning
  `extern(ffi)` wrapper (`skull_codegen_lua::wrapper_return`); stdlib
  `inline(lua)` bodies are fixed in source.
- `Entity.get` in `stdlib/arena.rock` is spelled to minimise inlined frames
  and constants. Do not tidy it.
- A one-element fast path removes a leaving-loop site only when the loop's
  population is uniformly trip-one; on a mixed population it removes the
  cheap iterations and the site gets worse. Measure the trip-count
  distribution, not the mean.
- An entry/SELF inner-loop pair cannot be fixed by moving the loop: LuaJIT
  inlines calls, so a callee's loop is still an inner loop of the caller's
  root. Transforms that remove a pair: a straight-line path for a known-small
  trip count, replacing a digit loop with interpolation, removing a
  `continue` (which emits a second back edge), merging repeated passes over
  one collection. Recovering counted loops moves aborts from the inner-loop
  class to the leaving-loop class; it is not a leaving-loop lever.
- Memoized caches that touch hot traces must be invalidated by every edit
  that changes their inputs, and memoize the whole set of per-element walks:
  memoizing one hands the recorder to the next.
- A uniform block must be a `value` struct (fixed layout for byte packing).
  `pack_uniform_block` on a global source reads each leaf right before its
  own store (`boxed_leaf_paths` in `skull-mir`), so the leaves are not live
  in every exit snapshot. A persistent cdata engine block refreshed in place
  was built and reverted: every nested cdata field read allocates a
  reference cdata in the interpreter.

### Emitter mechanisms

- **Nil-kill** (`regalloc::nil_kills` in `skull-codegen-lua`) assigns `nil`
  to a local at its last use so snapshots drop it. It costs a `KPRI` per kill
  in the interpreter (about 7.5% of mean frame with the JIT off) and lowers
  aborts in the coalescing class with the JIT on. Exclusions: pinned cells,
  coroutine `__step` bodies, loop-scoped registers outside their loop block,
  counted-loop header and latch instructions. The kill and the
  local-declaration loop read one predicate for "does this cell render as a
  local"; a kill for a cell without a local would be a global write.
- **Callee binding as immutable upvalues.** Each callee costs 2 constants
  instead of 5 plus a guard. Measured about -10% mean frame and "trace too
  long" down by two thirds. `@nomangle` and extern callees are not bound.
- **State cell.** A Rock state global reads a closed-upvalue cell bound to
  `_G.__rock_globals` instead of walking `_G` (about 14 IR instructions and 7
  guards per read, re-walked because every state store may alias). The cell
  must be a closed upvalue: the prelude opens a `do` block before the cell
  declaration and the full emitter writes the matching `end` as its last
  line. As an open upvalue each read interned a 2-slot i64 constant.
- **FORL recovery** (`skull-transform/src/forl.rs`) rewrites tagged protocol
  loops into counted `for` loops. The detector rejects arena pair iteration,
  hand-written `while` loops with fused increments and counted compares with
  no `+1` store.
- **Tail calls all forward.** Not forwarding them raised aborts and trace
  starts with no frame gain, costs about 7% in the interpreter, and breaks
  unbounded tail recursion.

## Measuring with a trace hook

`rock perf <scenario> --gc-window N --gc-window-mode running --format json`
reports recorder activity for the allocation window in
`memory.gc_window.jit_trace`. `starts` counts trace starts, `compiles` counts
completed traces (LuaJIT `stop` events), `aborts` retains the total abort count,
and `flushes` counts trace-cache flush events. `abort_reasons` contains every
nonzero error-code bucket, ordered by code:

```json
{"code":2,"reason":"trace too long","count":17}
```

The code indexes the running VM's `jit.vmdef.traceerr`; the message is its
template, so NYI bytecode and FastFunc arguments share their error-code bucket.
Code 2 covers the trace/IR constant limit, code 10 the loop unroll limit.
Code -1 means the event supplied no error code. The reason counts sum to
`aborts`; an available window without aborts reports `[]`, and unavailable
counters report `null`. Counting uses preallocated numeric slots; labels and
JSON are built after the window closes.

`run.jit_trace.abort_reasons` shares the same scope and frozen histogram as
`run.jit_trace.abort_sites`: the completed allocation window when present,
otherwise the process up to export. Its existing `starts`, `compiles`,
`aborts` and `flushes` retain their process scope. Use the counters under
`memory.gc_window.jit_trace` to compare window totals with the histogram.

LuaJIT keeps one handler per `jit.attach` event; a hook loaded with
`luajit -e` must wrap `jit.attach` and chain later handlers, since an
application prelude may install its own counter and silently replace an
earlier one. For `TRACEOV` aborts, `jit.util.traceinfo` and `tracek` still
describe the aborted trace inside the callback, including the function
constants it inlined. Under GC64 the `SNAP_FRAME` entry of an inlined frame
in `jit.util.tracesnap` holds the return PC; the frame's function is the
entry one slot below it.

## Rejected approaches

Do not retry these; each was measured.

- Structured loop emission: wrapping single-entry natural loops in
  `while true do .. end` so `BC_LOOP` gets a range made the inner-loop class
  2.8x worse, because `rec_loop_interp` then aborts `LINNER` where the
  rangeless path unrolled, and game loops are dominated by trip counts of one
  or two. Goto is worst at every non-zero trip count, a counted `for` is
  nearly free at trip 1, and `while` is catastrophic at trip 0. The `goto`
  shape ships. Loop-header canonicalization and structured `if` emission are
  in the same class.
- Splicing value constructors: removes the vtable rebox family but a table
  constructor in a register-hungry body costs more; calls stay.
- Inlining the checked-index guard at each Vec read: 5 fixed constant slots
  per trace plus 2 per call site, and only redistributed aborts, twice.
- Raising `maxirconst` or `loopunroll`, `hotloop=16`, `hotexit=400`:
  each removes its class and lowers nothing else, or loses outright.
- Eliding dominated `.unwrap()` nil guards: no effect on totals.
- Dead return-value elimination as a live-slot lever: worth about one slot.
- Parameter count as a lint proxy for live slots: r = -0.233.
- Boxing the engine uniform block, or unmarking `nobox` on the vector and
  rect value types; scalarization must stay.
- `i < 0 or i >= n` as a single product-sign test diverges on fractional `i`.
- Removing a whole abort class or a downstream guard as a fix: the exit
  moves. Measure totals and per-site rows together.

## Field-store nil-write guards (2026-11-13)

A nil store into an absent table key is expensive, so the Lua emitter
guards a field store whose value it cannot prove non-nil
(`if v ~= nil or t.f ~= nil then t.f = v end`).

The mechanism is time, not memory, and measuring only memory makes the
guard look pointless: a nil store never leaves a node behind, so a table
whose hash part has no free node rehashes to make room, drops the nil on
the way, and rehashes again for the next such store — the same work
forever at no net memory growth. The cost is proportional to the table's
live entries. Per store on an M1 with the bundled LuaJIT
(`benchmarks/nil_store_probe.lua`, which reports both columns so this is
re-checkable per architecture):

| live entries in the table | raw nil store | guarded |
| --- | ---: | ---: |
| 3 | 35.8 ns | 1.24 ns |
| 15 | 195.8 ns | 1.28 ns |
| 63 | 716.0 ns | 1.91 ns |
| 127 | 1264.8 ns | 1.79 ns |

A real store into the same absent key costs 1.16 ns and does occupy a
node. Keep the guard: it is worth its branch by two to three orders of
magnitude on the path it protects, and it is free on a warm trace.

The guarded and raw stores agree on every value — the guard only skips a
store that would be a no-op — so
removing a guard whose value cannot be nil is dead-code removal, and a
wrong never-nil answer could only reintroduce the rehash, never change
behavior. Presence tests folded by cleanup are the dangerous consumer, and
they read a separate, deliberately narrow tier; see
[Never-nil analysis](compiler.md#never-nil-analysis) for the rules.

The earlier version omitted `getmetatable` under a plain-record assumption.
The subsequent heap-record callback audit restores that test for source
heap records: a host-supplied metatable can observe even a nil write to an
absent field. It checks the metatable before field presence, preserving both
setter callbacks and the absence of getter callbacks during a store. The
plain-storage variant remains valid for compiler-owned fields and frames.
The original interpreter comparison (33.9 -> 25.8 ns for a present clear,
27.3 -> 16.8 ns for an absent clear) measured removing the metatable test;
it does not justify changing callback semantics, and the warm JIT folded
the metatable load in that experiment.

Lowering marks cells never-nil from concrete binding types, from the leaf
types of splatting value parameters (the dotted-key `embed` stores that
guarded every leaf) and at an `if` expression's join. The emitter drops the
guard when the value's shape or a declared type says it cannot be nil, and
drops the statically false `nil ~= nil` disjunct from literal-nil stores. A
single frame field assigns directly (never-nil) or through a one-line guard
(plain operand) instead of a `do local v0` scope. Stores that can be nil —
nullable leaves, presence tags, flat-enum slot clears — keep their guards,
pinned by test.

On `packages/stone`: 548 -> 437 field-store guards, all 472 frame-store
`do local` scopes gone (373 become one-line guards), 127,097 -> 125,931
lines. Every one of those differences is a guard or scope shape: a
classifier over the full diff of the two emitted programs leaves nothing
unaccounted for. Cold compile is unchanged (interleaved stone compiles, min/median
129.9/142.4 -> 130.3/143.9 ms on Apple M1): the never-nil marking is two
map probes per binding cell.

What this buys, measured (`benchmarks/field_store.rock`, 15 interleaved
fresh-process rounds, bundled LuaJIT 2.1.1794164915, Apple M1, JIT on):
**warm trace times are flat**. `leaf_capture` (7 guarded leaf stores per
call) 16.51 -> 16.43 ns/elem median, `row_reset` 2.59 -> 2.68,
`coro_points` 3.65 -> 3.66, `nullable_passthrough` control 1.64 -> 1.69 —
all within round-to-round noise, consistent with the rejected
unwrap-guard elision: a compiled trace folds `getmetatable` of a known
table shape and hoists the presence read, so the guard is nearly free
once hot. The extracted Stone vertex writers were likewise flat (±0.6%).
The interpreter, which pays every guard, improved where guards vanished:
`leaf_capture` 68.5 -> 47.6 ns/elem (-30%), `row_reset` 79.5 -> 71.3
(-10%); interpreter numbers are diagnostic, not the target. The
change is justified by smaller emitted code and cheaper
recording/off-trace execution at zero compile-time and runtime cost, not
by a warm-trace speedup claim.

### Coroutine yield slots are seeded in the constructor (2026-11-14)

A coroutine's frame IS the handle, and its constructor builds it from a
table literal holding what it knows at creation. A flat-enum channel's
yield slots (`__y*`) were not in it, so the first yield inserted them,
growing the frame, moving its hash part under any trace that had read it,
and keeping the frame's shape trace-hostile: LuaJIT cannot sink a table
allocation whose keys appear after construction.

The constructor now seeds every yield slot in the literal with `false` and
clears it to nil on the next line. The template is still built once and
copied with one TDUP; the nil-valued nodes make every later slot write a
store to a PRESENT key, so yield writes do not grow the frame, and a frame that stays
inside one trace is sunk outright. The clears keep the seeds invisible —
a nil-valued node does not appear to `pairs` or snapshots, and no slot is
readable before the first yield writes it — so nothing observable changes.
Measured with `benchmarks/coroutine_lifecycle.rock` (15 interleaved
fresh-process rounds, bundled LuaJIT, M1, JIT on), per coroutine:

| workload | before | after |
| --- | ---: | ---: |
| short-lived, handle stays in the trace | 196.1 ns | 2.1 ns |
| short-lived, handle escapes to a pool | 221.6 ns | 107.7 ns |
| long-lived, all-quiet yields (ns/resume) | 1.25 | 1.20 |
| long-lived, mixed yields (ns/resume) | 3.25 | 3.24 |

The interpreter gains about 10% on short-lived creation and is unchanged
long-lived.

Seeding moves a frame's growth to its birth, so the one case that pays is
a coroutine created and never resumed, and only when the seeds push the
literal past a power-of-two hash size. Measured per frame (20,000 live
instances, bundled LuaJIT, M1):

| frame | before | after |
| --- | ---: | ---: |
| 3 slots, never resumed | 269 B | 269 B |
| 3 slots, resumed | 269 B | 269 B |
| 7 slots, never resumed | 269 B | 461 B |
| 7 slots, resumed once | 461 B | 461 B |
| 7 slots, driven to completion | 462 B | 462 B |

A narrow frame is free (its seeds fit the size the literal already had) and
any resumed frame is identical, because the unseeded table grows to the
same size on its first yield. A wide frame that is created and never
resumed costs 192 B more. Seeding only as many slots as fit under the
current power-of-two boundary would avoid that, but partial seeding
reinstates growth for the rest and loses the sinking property, so it is
not obviously better; it is unmeasured and open.

`tests/coroutines/frame_slot_seeding.rock` pins the observable half: a
never-resumed coroutine's status, kill and completion; values across 3-,
7- and record-variant frames; that `pairs` sees no yield slot on a fresh
frame and only the live variant tag after a cleared yield; and that a
frame rebuilt by a host `pairs` copy resumes where it left off. That last
case checks the representation a host can rebuild by walking `pairs`; it
does not exercise a snapshot implementation or a cross-version reload.
Compatibility with older frames is inferred from the layout stamp: it is
computed from frame cells and the channel shape, not the constructor's
literal, so seeding alone does not change it. The step's nil-write
guards STAY: removing them instead was measured 80% slower on short-lived
creation (the absent-key nil stores hit the repeated-rehash path above),
and a frame rebuilt by a host copy (snapshot restore walks `pairs`, which
skips nil-valued nodes) has no seeded nodes to rely on.

Rejected on the way: building the whole frame with `table.new(0, n)` plus
field assignments. A constant-key literal is not a sequence of stores —
LuaJIT builds a template table at load time and each evaluation is one
TDUP copy (67.6 ns against 125.0 for `table.new` plus five stores) — so
that variant made per-frame creation 65% slower and was reverted. Seeding
through the literal keeps the TDUP and gets the stable shape anyway.

## Map loops (2026-11-11)

`Map` walks lower to `MapNext` over a bare table or a column map's spine.
Emit a recognized non-suspending walk as `for k, v in next, table do`: LuaJIT's
native table-iteration bytecode is much cheaper than calling `next` at a
backward goto. Keep the anonymous cursor separate from user bindings, including
bindings captured or written by closures. A coroutine keeps its framed cursor.
Removing the current or a pending entry and replacing an existing entry retain
the live walk's behavior; inserting new keys during a walk has no new guarantee.

Unused map bindings need no value snapshot. In particular, don't generate a
column bounds check for an unused value: cleanup correctly preserves checks
that were emitted, even after removing all their dead leaf loads. This also
helps `is_empty`, `clear`, and the key walk in `keys_into`. `values` and
`to_array` still return sorted snapshots and read their values in key order.

`benchmarks/map_loop.rock` measures scalar, row-struct, and flattened column
values, small/large lengths, direct key/value walks, sorted snapshots, native
Lua controls, and a register/remove workload with 200 retained entries. Compile
it, then run the emitted Lua with `dofile("map.lua"); main()`. Each case flushes
and warms the JIT, records traces and aborts, then times the warm workload.
Interpreter times are diagnostic only. Length timings are per call; direct
walk timings below are per entry over 1,000 entries. Results are min/median ns
from 15 interleaved baseline/candidate rounds, reversing order each round.
The baseline is `bd14ff9d`; both machines used bundled LuaJIT 2.1.1794164915,
with the JIT enabled. These are isolated workloads, not an engine frame claim.

| Workload | Apple M4 Max baseline | Candidate | Intel i9-14900K baseline | Candidate |
| --- | ---: | ---: | ---: | ---: |
| Scalar len, 8 entries | 45.35 / 51.03 | 17.94 / 18.44 | 64.40 / 66.81 | 15.33 / 16.17 |
| Scalar len, 1,000 entries | 4322.9 / 4688.2 | 1163.0 / 1194.3 | 6250.4 / 6708.3 | 1439.5 / 1450.6 |
| Row len, 1,000 entries | 4108.7 / 4574.5 | 1191.3 / 1215.1 | 6322.7 / 6765.9 | 1415.1 / 1423.4 |
| Scalar keys | 4.343 / 5.079 | 1.192 / 1.208 | 6.263 / 6.622 | 1.430 / 1.442 |
| Row keys | 4.235 / 4.681 | 1.197 / 1.215 | 6.316 / 6.600 | 1.414 / 1.422 |
| Column keys | 4.055 / 4.915 | 1.167 / 1.214 | 6.207 / 6.627 | 1.426 / 1.448 |
| Scalar values | 3.937 / 4.559 | 1.159 / 1.216 | 6.297 / 6.674 | 1.407 / 1.416 |
| Row values | 5.219 / 6.095 | 1.503 / 1.652 | 6.599 / 7.067 | 1.639 / 1.741 |
| Column values | 4.547 / 5.022 | 1.411 / 1.481 | 6.268 / 6.739 | 1.667 / 1.686 |
| Row registration, 200 entries | 900.82 / 1177.40 | 303.18 / 310.58 | 1258.38 / 1314.42 | 352.76 / 360.82 |

Direct walks generally recorded two completed traces and two warmup aborts
on both versions; across all rounds some cases recorded up to four aborts.
The improvement comes from cheaper iteration, without a claim of fewer trace
aborts. The native Lua controls were effectively unchanged (about 1.2 ns/entry
on arm64 and 1.4 ns/entry on x86), and candidate key walks reached that range.

Column-map `len` was already an `occupied_count` read and showed no gain:
about 0.4 ns/call on warm, invariant maps, where the JIT can hoist the read.
The mutating column registration case also showed no consistent gain (arm64
median 5.84 -> 6.74 ns; x86 4.62 -> 4.50 ns). Sorted `keys`, `values`, and
`to_array` snapshots improved only 2-6% at the median; allocation and sorting
still dominate. No count is added to bare maps and no insertion cost is added.

For `rock compile packages/stone` on the M4 Max, 15 interleaved fresh-process
runs at one thread, caches disabled, writing to internal storage: retired
instructions min/median were 3.39530/3.40610 billion before and
3.39583/3.40643 billion after (median +0.010%, no compile-speed gain).
Wall min/median was 0.28/0.29 s before and 0.27/0.29 s after. Emitted Lua shrank
from 4,663,279 to 4,659,316 bytes and from 128,653 to 128,284 lines. Its map
loops change shape, so output identity is intentionally different.

A separate 15-round ablation kept native generic-for emission and restored the
old unused-binding lowering. Removing the unused column read then changed the
column key-walk min/median from 1.405/1.447 to 1.206/1.225 ns/entry on arm64
(-15.3% median). On x86 it showed no gain: 1.421/1.436 -> 1.435/1.453 ns/entry.
Other direct walks were effectively unchanged in this ablation. Keep the
simpler key-only lowering for its measured arm64 gain and smaller emitted work.

### Follow-up opportunity investigation (2026-11-14)

At `62355a8e`, ordinary maps still count entries and column maps read
`occupied_count`. Constant-time ordinary length is a representation and
interop problem, not just an extra increment: `map_new` returns a bare Lua
table, `table_set` and opaque Lua can write it, and callers can supply tables
created outside `map_new` (for example `perf_phase_index_new` in Stone).
A count stored in the map would pollute iteration and collide with keys;
a side table would avoid those problems but still miss external mutations.
`__newindex` cannot observe overwrites or deletion of existing entries.
Retaining bare-table interop therefore needs a proven closed set of mutations
for a counted fast path, with ordinary counting for exposed maps. Measure
insert, overwrite, present/absent remove, clear, and mutation-only workloads
before accepting the extra bookkeeping. No counted representation was added.

`benchmarks/map_count_probe.lua` measures a counter prototype with all writes
under its control. It checks overwrite, false values, nil-as-delete, absent
remove, clear, and explicitly demonstrates a stale count after a raw external
write. `crates/skull/tests/map_len_interop.rs` pins ordinary Map length across
Lua mutations, aliases, and externally created tables. The prototype therefore
does not justify replacing ordinary Map with a counted wrapper.

On Intel i9-14900K, system LuaJIT 2.1.1785763465, JIT enabled, nine alternating
raw/counted pairs in one process, each flushed and warmed before timing:

| Controlled mutation prototype | Raw median ns/iteration | Counted median ns/iteration |
| --- | ---: | ---: |
| Insert/remove, length read between them, 200 retained entries | 311.610 | 2.110 |
| Toggle presence across 64 keys, no length reads | 1.867 | 1.992 |
| Toggle presence across 64 keys, length read every 16 writes | 31.557 | 2.564 |
| Clear/refill eight entries | 36.080 | 42.210 |

The mutation-only cases expose a cost even when length is unused. Fixed-key
and invariant-length cases in the probe can be hoisted or sunk by the JIT;
they are not general insertion or length costs. A production fast path still
needs proof of a closed mutation boundary, including host callbacks and
retained aliases. Adding that analysis solely for local maps is not yet
justified by a representative Rock workload.

Snapshot sorting has a smaller candidate: `__sort_map_keys` always calls
`table.sort(keys._data, __rock_inspect_key_lt)`, whose comparator applies
`tostring` to both keys. For statically string keys, default `table.sort`
preserves that comparison without the callback. This must not be applied to
numeric keys: their existing order is string order, not numeric order.

Measured with the release compiler at that revision, Apple M4 Max (arm64),
system LuaJIT 2.1.1774896198, JIT enabled. Compile
`benchmarks/map_loop.rock` with `compile-single --output map.lua`, copy its
output, and in the copy replace only
`table.sort(keys._data, __rock_inspect_key_lt)` with
`table.sort(keys._data)`. Every map key in this benchmark is a string.
Run each file with `luajit -e 'dofile("map.lua"); main()'` in seven pairs
of fresh processes, reversing baseline/candidate order each pair. The existing
driver flushes and warms each case before timing. This was an emitted-Lua
experiment before compiler implementation or x86 measurement.

| Snapshot, 1,000 entries | Baseline min/median ns/entry | String sort min/median ns/entry |
| --- | ---: | ---: |
| `keys` | 186.176 / 199.180 | 127.057 / 144.481 |
| `values` | 189.987 / 199.144 | 133.332 / 148.009 |
| `to_array` | 215.452 / 226.132 | 149.897 / 164.695 |

Median reductions were 26-27%; allocation behavior was not changed or measured.
One hundred randomized string-array comparisons, including zero and high
bytes, produced identical sorted arrays with both comparators. Existing
snapshot regressions are in
`crates/skull/tests/map_empty_snapshots.rs` and `tests/nobox/map_value.rock`.
The same baseline runs measured scalar length at 19.030 ns for 8 entries and
1,302.300 ns for 1,000; column length at 0.436 ns for 1,000 (an invariant read
the JIT can hoist); and row register/remove with 200 retained entries at
324.840 ns. These confirm the length cost, not the payoff of a counted-map
implementation.

The implemented path folds the string-key decision in each concrete Lua
`Map.keys_into` body. String keys and transparent string newtypes use default
`table.sort`; other keys retain the `tostring` comparator. `values` and
`to_array` inherit the sort through `keys`. JS retains its existing comparator.
Regressions cover numeric and mixed-key ordering, string newtypes, empty maps,
column maps, and independent snapshots.

For the implementation measurement, compile `benchmarks/map_loop.rock` with
`rock compile-single benchmarks/map_loop.rock --output map.lua` using release
compilers before and after the change (baseline `bd40143a`). Run each emitted
file with `luajit -e 'dofile("map.lua"); main()'` in 15 interleaved pairs of
fresh processes, reversing order each pair. The benchmark flushes and warms
each case before timing; the JIT is on for the reported times. The arm64 run
used an Apple M4 Max and system LuaJIT 2.1.1774896198; the x86 run used an
Intel i9-14900K and system LuaJIT 2.1.1785763465. The arm64 job began at
9.5 load on 14 cores while unrelated work was present. Times are ns per entry
for 1,000-entry snapshots; each cell gives min / median.

| Snapshot | M4 Max baseline | M4 Max string sort | i9-14900K baseline | i9-14900K string sort |
| --- | ---: | ---: | ---: | ---: |
| `keys` | 188.904 / 199.423 | 131.203 / 146.525 | 264.619 / 273.183 | 145.991 / 150.947 |
| `values` | 191.606 / 202.695 | 132.178 / 147.366 | 267.948 / 275.038 | 150.462 / 155.095 |
| `to_array` | 217.672 / 226.383 | 149.737 / 166.023 | 292.017 / 298.834 | 171.278 / 176.616 |

The sort change does not alter snapshot allocation. The empty snapshot
allocation and freshness regressions still pass.

Returning a shared cached snapshot would break independence. `keys_into`
already provides explicit caller-owned reuse, used by Stone's draw list and
JSON streaming. Further reuse should have explicit scratch ownership and
tests for nesting, aliases, stale tails, and retained references. Caching
sorted keys across calls has the same unobserved-mutation problem as length.

An emitted-Lua prototype let `values()` overwrite its private sorted key
buffer with the corresponding values and return that buffer. On an Intel
i9-14900K with system LuaJIT 2.1.1785763465 and the JIT on, nine interleaved
pairs for 1,000 entries changed median runtime from 155.684 to 153.020
ns/entry. A GC-stopped, JIT-on allocation probe measured 16,874.32 versus
8,446.24 bytes per snapshot. The result would remain fresh
and independent, but the generic `values()` body also instantiates for column
and strided outputs. A helper returning `[V]`, or a cast from `any`, crosses
the compiler's opaque-body representation boundary for column results even
when the guard would fold false. This prototype was not landed; a later change
needs a representation-aware return path and direct regression coverage.

The other candidates need more targeted evidence:

- Coroutine enum yields already have an unboxed frame-slot channel. Inspection
  of emitted `benchmarks/coroutine_lifecycle.rock` found all six resume sites
  using `__advance`, including the pooled-handle case. A follow-up emission
  check of Stone's generic `task_poll_step<R>` also found `__advance` and
  `__y*` reads at its `resume(pc.coroutine)` call, with no `.__resume` call.
  `crates/skull/tests/instance_graph.rs` now pins the same property for a
  generic resume helper, generic task wrapper, iterator `next`, and `await`
  delegation of a mixed enum. All four consumers emit `.__advance`; none emits
  `.__resume`. The generated `__resume` functions are still needed for Lua
  interop and for yield types without a flat-enum channel. For such types,
  the wrapper returns the yielded scalar or cdata directly. The mixed-enum
  wrapper would build a table if called, but this survey found no reachable
  boxed mixed-enum consumer in the typed Rock paths it checked.

  A JIT-on allocation check in `packages/stone-core/tests/poller_alloc_rules.rock`
  drives 64 long-lived `Wait::Frame` coroutines through the actual poller for
  200 warmup updates and 2,000 measured updates, with GC stopped after a full
  collection. Five fresh runs measured 0.0104-0.0237 bytes per coroutine per
  update for `run` and 0.0035 for `run_into` (Apple M4 Max, LuaJIT 2.1, JIT on).
  The residual is fixed measurement or JIT activity, far below one result
  table per resume; the test allows 0.25 bytes per coroutine per update. These
  steady-state figures do not measure creation or terminal transitions. No
  compiler change to the boxed wrapper is warranted by these workloads.
- `Map.remove` once had only an element-write effect, so
  `after_direct_call` forgot every `Key::Elem` but kept direct length facts.
  A typed Map can arrive from Lua with an `__index` metamethod. A missing-key
  `remove` invokes it, and it can rewrite a vector element or shrink a vector.
  Keeping either fact miscompiles the guarded access. `Map.get` therefore has
  no `@nowrite` promise, and its transitive effect makes `Map.remove` unknown.
  Narrowing this needs proof that the receiver has no such metamethod and
  cannot acquire one through an escaped alias. Distinct locals and types do
  not establish that proof.
- Wide live floats were subsequently measured on x86. The retained arithmetic
  deferral below reduces the measured membership and grid exits. It leaves the
  scoring caller's wide value in place, and cached still has over-budget exits.

String-key snapshot sorting and wide arithmetic deferral are implemented.
Constant-time ordinary length has clear potential but a larger interop and
representation burden; neither buffer caching nor metadata alone resolves it.

### Reproducing live-float exits

Run `rock run benchmarks/live_floats.rock` with the JIT on, on x86-64. The
benchmark covers record membership with early return, a counted grid walk,
geometric vec2/vec3 scoring, and cache acquisition with a prepared 24-leaf
value. The scoring caller retains another 20 scalar leaves. It warms and
times the same loop, using 300,000 and 1,000,000 iterations respectively;
`ns_per_iter` measures process CPU time on the warmed cache. Trace counts
cover the warmup phase.

For every coalescing failure it prints the parent/exit, count, distinct
non-constant parent snapshot double refs, inherited double SLOADs, and parent
PHIs. These loop-root examples have two PHIs and 25-30 snapshot doubles on
x86. The inherited caller values dominate the side-trace head. Arm64 has a
larger floating register bank; a clean arm64 recording does not disprove the
x86 failure.

`blacklist_encounters` counts recordings encountering interpreter-only code;
`blacklisted_entries` counts observed starts ending with interpreter-only
entry bytecodes. `exhausted_exits` separately counts coalescing sites reaching
156 failures, the retry cap at the shipped hotexit. Those side exits need not
produce a blacklist event. Parent refs and inherited SLOAD IR instructions
also do not directly report the assembler's physical live-register set.

Guard arithmetic materialization and exclusive-arm float-expression deferral
were prototyped against this benchmark. Materialization lets nil-kills precede
a comparison; deferral can postpone wide arithmetic until after a helper
call. Both need care: assigning the boolean introduces the guard before the
kills, moving one division can replace one result with two live inputs, and
an aggregate saving across arms can hide an increase on one exit. Mutable
reads, throwing operations and coroutine frame reads must retain their order.
Constant-only arithmetic does not consume a floating register in the snapshot.

Those prototypes were not retained. Reducing an individual snapshot or fixing
one repeatedly failing exit did not establish a consistent pooled runtime
improvement. In the scoring reproduction, even removing three doubles leaves
more than the x86 side-head budget because the caller retains a wide value.

### Deferring wide float arithmetic past calls (2026-11-14)

The Lua emitter now delays a wide group of non-throwing `FloatAdd`, `FloatSub`
and `FloatMul` MIR instructions until just after a scalar call in the same
block. It does not move field, index, global or cell reads, so a value read
before a mutating call remains the original snapshot. Existing effect-order
checks keep a throwing read before the call, and the call is pinned at its MIR
site so expression folding cannot move it back after the arithmetic. The pass
applies to ordinary Lua functions, not coroutine step bodies or emission with
source line directives. It requires at least eight externally used float
results and a conservative reduction of at least eight live values after
counting distinct nonconstant inputs that must stay live across the call.
This avoids trading one result for two inputs. Numeric MIR arithmetic follows
the compiler's typed-number contract; a host value masquerading as a float
is outside that contract.

Measured `benchmarks/live_floats.rock` on an Intel i9-14900K with LuaJIT
2.1.1785763465, JIT on, in nine interleaved fresh-process baseline/candidate
pairs. Each process warmed 300,000 iterations and timed 1,000,000 iterations
per workload. Times are ns per iteration of warmed process CPU time:

| workload | baseline min / median | deferred min / median |
| --- | ---: | ---: |
| membership | 27.450 / 79.556 | 12.465 / 26.064 |
| grid | 33.364 / 34.720 | 9.036 / 9.247 |
| score (unchanged control) | 138.077 / 150.169 | 129.727 / 148.199 |
| cached | 79.892 / 90.708 | 23.063 / 65.702 |

All four output sinks matched. The parent snapshot's maximum double refs fell
from 26 to 5 for membership and grid. Their counted coalescing failures fell
from 0/312/468 (membership, varying between processes) and 156 (grid) to zero.
Cached still reached 156-312 failures (468 before) and a 25-26-double maximum;
score retained its 25-30-double maximum and 312-780 counted failures, with no
consistent runtime change. The minima and medians show substantial run-to-run
variation in membership and cached, so neither one number alone describes
their speed. Tests pin the emitted order and outcomes when the call mutates an
input field, a global or a captured local, when the input comes from another
call, and when an earlier nullable read or producer call throws.

Cold compilation of `packages/stone` was checked against a compiler with the
same other changes but without this pass: 15 interleaved fresh-process pairs,
one thread, no on-disk caches, Apple M4 Max. Median retired instructions rose
from 4,101,709,453 to 4,107,690,955 (+0.146%); minima were 4,098,525,024
and 4,105,053,395. Median CPU time was 0.36 s for both, and median wall time
was 0.33 s for both. Each executable received one untimed invocation before
the paired rounds to exclude one-time startup. The measured compile cost is
small relative to the x86 runtime gains in the affected loops.

## Large native buffers

This section is about collector cost, not traces.

`native_buffer_alloc<T>`, the sized constructors (`native_buffer_u8` and the
rest) and `FlatVec` storage all allocate through `__rock_nb_alloc(ctype, n)`
(`NB_ALLOC_PRELUDE` in `crates/skull-codegen-lua/src/preludes.rs`). Storage
below 16 KB is an `ffi.new("T[?]", n)` array on the Lua heap. Storage of 16 KB
or more comes from `calloc`; `ptr` is then a `T*` with an `ffi.gc` finalizer
that frees it.

### Why

LuaJIT counts every allocated byte as collector debt, including a cdata array
that holds no references. `lj_gc_step` repays that debt at the next allocation
check, so a large buffer allocated while a cycle is in its sweep phase makes
that one step sweep in proportion to the buffer's size. Measured with a
collector-timing probe in a game with about 300 MB live: 8 to 17 microseconds
of sweep per requested KB, so a 2 MB allocation in a sweep is a 15 to 35 ms
frame. About 180 MB of that heap was buffers of 4 KB or more, nearly all
retained for the session, which the collector still accounted for on every
cycle.

### Measured effect

Same game, the same build with and without the mechanism
(`ROCK_NB_OFFHEAP_MIN=1e18` for the control), 1800-frame captures:

- Lua heap 365 to 400 MB down to 230 to 260 MB in the walking scenarios; about
  110 MB in about 580 buffers is off-heap.
- The largest single-frame heap growth at an area arrival fell from 2.0 to
  3.0 MB to 0.7 to 0.9 MB.
- Mean and median frame time did not change beyond run-to-run variation in
  five scenarios (four interleaved pairs in one, two in the others).
- The forced-collection guard described below did not fire in any scenario.

It does not remove collector hitches. What remains is ordinary Lua-heap
allocation (tables, strings, small cdata) in bursts of one to two megabytes;
those are fixed where they are allocated.

### The threshold

The threshold was 64 KB at first. After the megabyte buffers left the heap,
the frames that still grew it by several hundred KB were mostly retained
buffers of 16 to 64 KB allocated a few at a time: fitted mesh vertex storage,
instance buffers, the first steps of a growing `FlatVec`. Same game, the same
build at 64 KB and at 16 KB (`ROCK_NB_OFFHEAP_MIN`), interleaved pairs:

- Frames growing the Lua heap by more than 300 KB in the main walking
  scenario: 32 of 1800 down to 5 or 6. In a riding scenario 13 to 19 down to 5.
  No change in a scenario that loads nothing new.
- Lua heap after a collection about 175 MB down to about 163 MB; off-heap
  buffers about 580 up to about 1400, about 115 MB up to about 140 MB. The
  median heap over a capture does not fall, because less debt per frame also
  means the next cycle starts later.
- Mean frame time unchanged beyond run-to-run variation in four scenarios; the
  forced-collection guard did not fire.

4 KB was also tried and gave no further reduction in those frames. Each
off-heap buffer costs a `calloc`, a finalizer registration and a `free` at
collection, so the threshold stays as high as the measurements allow.

### What differs for off-heap storage

- `ptr` is a pointer, so nothing about it gives its length: `ffi.sizeof(ptr)`
  is the pointer size and `tostring(ffi.typeof(ptr))` is not the array type.
  The descriptor's `len` is the length. Generic code that needs to recognise
  storage or know its byte size calls `__rock_nb_storage_of(x)`, which returns
  the compiler's storage description and the byte length for both forms.
- `ffi.istype("T", ptr)` is true for a `T*`. Code that classifies an unknown
  cdata must ask `__rock_nb_storage_of` before `__rock_c_struct_of`, or a
  struct buffer is treated as one struct. `deepcopy` and the engine's data
  image do; `tests/native_buffers/large.rock` and
  `packages/stone/tests/data_image_storage_rules.rock` fail if the order is
  reversed.
- Only the pointer object the allocation returned is known. A cast of it is a
  foreign pointer: it does not keep the storage alive and is not recognised as
  storage. This was already true of casts over an `ffi.new` array.
- A site that allocates both small and large buffers sees two cdata types
  (`T[?]` and `T*`) for `ptr`. Indexing either is compiled; a loop that
  alternates between a small and a large buffer of the same element type takes
  a side trace for the second type.
- The collector no longer sees these bytes, so `collectgarbage("count")` and
  heap figures derived from it under-report. `native_buffer_offheap_bytes()`
  returns the live off-heap total, and `__ROCK_NB_OFFHEAP_STATS` has
  `live_bytes`, `live_count`, `since_cycle`, `forced_steps` and
  `forced_collections`.
- A dropped off-heap buffer is freed when the collector finalizes its pointer,
  and its size no longer brings that cycle forward. The guard does, in two
  stages. Once 256 MB has been allocated off-heap within one collector cycle
  (`__ROCK_NB_OFFHEAP_CYCLE_MAX`), a large allocation advances the collector
  for at most 0.5 ms, and no more often than every 4 ms, until the cycle ends
  (`forced_steps` counts these). Allocated bytes are not dead bytes: a program
  that keeps 300 MB of buffers reaches this stage without having anything to
  free, so the stage has to be cheap. The first version ran a full collection
  here, and a profiler mode that allocates about 285 MB of history buffers
  took one frame of 110 to 125 ms for it. Past 1 GB in one cycle
  (`__ROCK_NB_OFFHEAP_CYCLE_HARD`) the steps are not keeping up with the
  allocation, and the next large allocation runs a full
  `collectgarbage("collect")` (`forced_collections`). That is a long pause;
  a program that reaches it should stop churning large buffers.

### If this is suspected of causing a problem

1. Run with `ROCK_NB_OFFHEAP_MIN=1e18`. Every buffer is then an `ffi.new`
   array as before the change. If the problem remains, the cause is elsewhere.
   Other values move the threshold (bytes).
2. Resident memory grows, or there are long pauses: read
   `__ROCK_NB_OFFHEAP_STATS`. A rising `forced_collections` means some code
   allocates and drops large buffers continuously; find it and reuse the
   buffer. A rising `forced_steps` alone costs at most an eighth of the
   program's time while a cycle is brought forward; if it rises every cycle
   the same advice applies. A `live_bytes` that keeps rising means buffers are retained, which
   is the program's own retention and was the same on the Lua heap.
3. Wrong values or a crash after a copy, snapshot or hot reload: look for code
   that inspects `ptr` with `ffi.sizeof`, `ffi.typeof` or `ffi.istype` and
   move it to `__rock_nb_storage_of`.
4. A crash in native code that holds a buffer address past the call that
   received it: the address is valid only while the Rock-side buffer is
   reachable. This was already true, but `free` returns the memory to the
   allocator immediately, where a collected Lua-heap array was often still
   readable.

### What to try next

- Threshold. 64 KB was chosen from one program's size distribution (128 MB in
  682 buffers of 64 KB or more). Moving every buffer off-heap was measured
  slower (median frame time about 7.0 ms against about 6.4 ms), because each
  off-heap buffer costs a finalizer and a registry entry and small buffers are
  short-lived.
- If the implicit switch turns out to be the wrong contract, replace it with
  an explicit constructor for long-lived large storage and return
  `native_buffer_alloc` to `ffi.new` only. The allocator and the storage
  lookup stay as they are; only the threshold test moves to the caller.
- For a buffer that only native code reads, keep it on the native side and
  pass a handle, so no Lua-side storage exists at all.

## Vector bounds checks (2026-11-11)

`xs[i]` on a `[T]` reads `xs._data[i + 1] or __vec_checked_get(xs, i)` and
`xs[i] = v` is a `__vec_checked_set(xs, i, v)` call. The elision pass
(`skull-transform/src/in_bounds.rs`) marks an access `VecInBounds` when a
forward dataflow proves `0 <= i < xs.len` at that point; it then prints as the
bare `xs._data[i + 1]`, read or written. Results and errors never change,
because an access is marked only where the check could not have raised.

What proves an index, in the shapes Stone and the demos write:

- `for i in 0..xs.len()`: the bound cell holds a value at most the length,
  the counter starts nonnegative and only increments.
- `for x in xs` after FORL recovery: the hoisted length read is the bound.
- `while i < xs.len()`: the header re-reads the live length every turn.
- `if i < 0 { return }` then `if i >= xs.len() { return }` before `xs[i] = v`.
- `xs[i]` read, then `xs[i] = ..` on the same slot: the read checked it.
- `rows[r][c] = ..` after `c < rows[r].len()`: `rows[r]` is one element place
  as long as nothing stores into any container between the two reads.

What keeps the check: any call in between (it may pop, clear or reassign
through any field or global it can reach; `@pure` is not trusted, because the
checker admits `@debug_impure` callees and never verifies an `inline(lua)` or
`extern` body, and a call through a function value or an overloaded operator
is a call like any other), a `len` store,
a store to the vector's cell or through its field, a yield, a bound the
vector does not cover (`for i in 0..n`), and any second vector indexed by the
same counter (`ys[i]` next to `xs[i]`, however equal their lengths). The
stdlib push is not a kill: it only grows the vector and only writes a slot
that was outside it. A `break` or `return` inside the loop needs no care; it
only removes later accesses. Mutation inside the loop body is handled by the
same kills, so a body that pops still raises on the slot that is gone, with
the same error as before.

Facts live on SSA values and on places (cells, globals, field and element
chains). A value is tied to the place it was read from, and the tie breaks
when the place is written, so a `len` read into a cell before a pop does not
borrow a fact from the `len` read after it. The pass runs after closure
conversion, where every captured mutable variable is already a box reached
through a field.

`ROCK_SKULL_BOUNDS_OFF=1` keeps every check for one compile. On
`packages/stone` the elision removes 8 of 65 `__vec_checked_set` sites and
159 of 820 `__vec_checked_get` sites; with the switch set the output is byte
for byte the baseline.
The remaining Stone stores are slot writes guarded on one vector and applied
to a dozen parallel vectors, `if i == xs.len() { push } else { xs[i] = v }`
free-list writes, and guards with a call between them and the store.

`benchmarks/vec_store.rock` times the shapes above, 1,000 elements per pass,
with the JIT on and then off; `_kept` workloads are controls the pass must
leave alone. Compile it with and without the switch and run each emitted file
with `luajit -e 'dofile("vec_store.lua"); main()'`. Apple M4 Max, bundled
LuaJIT 2.1, 9 interleaved rounds, min / median ns per element with the JIT on,
then min ns per element with the JIT off, checked -> elided:

| Workload | JIT checked | JIT elided | Interpreter checked -> elided | mcode bytes |
| --- | ---: | ---: | ---: | ---: |
| fill `xs[i] = i % 13` | 1.625 / 1.646 | 1.547 / 1.552 | 27.6 -> 9.3 | 1576 -> 1536 |
| increment `xs[i] = xs[i] + 1` | 0.820 / 0.900 | 0.615 / 0.686 | 39.1 -> 13.3 | 1404 -> 1140 |
| scale_field `s.items[i] = ..` | 1.945 / 1.948 | 1.864 / 1.867 | 51.3 -> 25.1 | 1584 -> 1384 |
| set_slots (guarded setter per slot) | 0.658 / 0.660 | 0.656 / 0.659 | 60.0 -> 40.6 | 1352 -> 1112 |
| bump_entries (read, mutate, store back) | 2.896 / 3.028 | 2.931 / 3.061 | 48.7 -> 23.2 | 1652 -> 1532 |
| clear_pair (`while`, second vector kept) | 0.995 / 0.997 | 0.991 / 0.997 | 62.8 -> 41.9 | 1936 -> 1940 |
| fill_pure_call (`@pure` callee, inlined away) | 2.046 / 2.087 | 1.972 / 1.998 | 29.5 -> 12.1 | 1376 -> 1152 |
| fill_and_collect (push in the body) | 5.718 / 5.798 | 5.325 / 5.343 | 45.1 -> 23.0 | 2736 -> 2428 |
| fill_rows (`row[c]` of `rows[r]`) | 1.016 / 1.019 | 0.764 / 0.767 | 28.4 -> 10.3 | 2136 -> 1948 |
| copy_into (`dst[i] = src[i]`, read only) | 0.741 / 0.744 | 0.662 / 0.673 | 38.8 -> 29.2 | 1456 -> 1456 |
| fill_call_kept (control) | 1.651 / 1.660 | 1.654 / 1.658 | 41.4 -> 41.6 | 1428 -> 1532 |
| fill_n_kept (control) | 1.639 / 1.644 | 1.641 / 1.646 | 27.7 -> 27.6 | 1316 -> 1204 |

Trace and abort counts were the same or within one trace on every workload.
With the JIT on the helper was already inlined into the trace, so the gain
is the dropped length load and compare, the frame-less snapshot and the
smaller trace: 4-11% on plain fills and reads, 25% where the slot is read and
written in one turn or the vector is itself an element. A bound the loop
proves but a setter re-checks (`set_slots`) gains nothing on trace, because
LuaJIT folds the redundant compare itself; the interpreter gains everywhere a
call disappears. These are isolated workloads, not an engine frame claim.

What the pass assumes, and what pins each assumption: a Vec's slots outside
`[0, len)` are nil or false and `_data` is never replaced (the stdlib's
representation; a direct `xs.len = n` write keeps the helper's behavior,
because the raw form and the helper write the same slot once `i < len`
holds); every cell is a plain local after closure conversion (a captured
mutable variable is a box reached through a field, and a body still holding
a lambda is skipped); a call may reach anything through memory, so it drops
every fact through a global, field or element, and only the stdlib push
(matched by its body text, not its name) is exempt; a yield drops everything;
a re-executed instruction drops the facts about its previous value. The
adversarial cases in `tests/iterators/vec_bounds_elision.rock` cover a
function value, a `@pure` wrapper around a `@debug_impure` pop, a lying
`@pure inline(lua)` body, an overloaded operator, an alias of the vector, a
captured closure, a row replaced inside the inner loop, an index below zero,
a pop across a yield and a global shrunk by a call: each still raises.

Other runtime checks Stone emits, counted in the same compile, that the same
facts could prove: `__nb_checked_get` / `__nb_checked_set` /
`__nb_checked_index` on NativeBuffers (62 / 31 / 54 sites; `{ptr, len}` has
the same `len` shape, and the raw form is `b.ptr[i]`), `__vec_checked_index`
on column-stored vectors (129 sites; done below, with a backend-aware slot
instruction), and the 484 nil-key store guards
(`if v ~= nil or getmetatable(b) ~= nil or b.f ~= nil then b.f = v end`),
which a field-presence fact would remove where the field was read or written
earlier on every path. `__map_col_slot_checked` (6) tests key presence, not
an index, and `__rock_layout_check` (92) runs once at load; neither is a
candidate.

## Column element checks (2026-11-12)

A column-stored `[E]` (an all-scalar `value` element) checks the element
index once per access, `__vec_checked_index(vec, i)`, and the leaf reads
then index their columns raw. The elision pass now proves that check with
the same facts as the flat case and replaces it with the bare slot
(`Inst::VecIndexSlot`: `i + 1` on Lua, `i` on JS), so every consumer of the
checked form is unchanged. Two shapes Stone indexes with were added to the
facts: an integer literal keys by its value, so `0 < xs.len` proves `xs[0]`;
and `k % xs.len` lands in `[0, len)` once `0 < len` holds (floor
modulo on both backends; without the guard an empty vector makes it NaN,
which the helper passes through, an existing edge of the checker itself).
An offset such as `xs.len - 1` retains its check unless independently proven
below the length: infinity and large floating-point integers can remain
unchanged by subtraction. The measurements below predate this correction.

`packages/stone`, same compile, checked -> elided:

| Check | Before this pass | After | If every call left vectors alone |
| --- | ---: | ---: | ---: |
| `__vec_checked_index` | 129 | 103 | 78 |
| `__vec_checked_get` | 661 | 633 | 327 |
| `__vec_checked_set` | 57 | 57 | 52 |

The last column is a one-off measurement with the call kill disabled, not a
shipped mode: it is the ceiling a "this call does not shrink or rebind any
vector" fact would reach with the present intraprocedural pass. The 103
column checks that remain, by the shape of their index:

- 32 inside a counted loop whose body calls a function (arena lookups,
  batch pushes, removals) after the access. The call may pop the vector the
  loop walks, so the bound read in the preheader is stale around the back
  edge and the proof must drop. Only an effect fact about the callee opens
  these.
- 25 whose index is a parameter with no guard in the body: small accessors
  (`section_time_metric(i)`, `drop_text_batch_for(i)`) whose callers hold
  the proof. Splicing them into their callers (an `@inline` on the accessor)
  moves the access into the caller's loop and the existing pass proves it;
  so would a call-site specialization that carries the fact across.
- 26 whose index is a computed register: the slot a column map hands back
  (`__map_col_slot(m, key)` then `m.data[slot]`), the result of
  `random_int(0, len)`, or `cur_count - 1`. These are data-structure
  invariants (a map's slot is below its data length; the random range is
  the length) that no local fact states.
- 20 others: `len - 1` and literal indexes without a non-empty guard, two
  modulo forms without one, and `i + 1` / `i - 1` of a proven `i`.

`benchmarks/vec_store.rock` gained the column shapes. Apple M4 Max, bundled
LuaJIT 2.1, 9 interleaved rounds, min / median ns per element with the JIT
on, then min with the JIT off, checked -> elided:

| Workload | JIT checked | JIT elided | Interpreter | mcode bytes |
| --- | ---: | ---: | ---: | ---: |
| column_sum (counted loop, two leaves) | 2.220 / 2.236 | 1.703 / 1.724 | 55.7 -> 21.9 | 1696 -> 1740 |
| column_each (`for s in ss`) | 2.461 / 2.474 | 2.086 / 2.118 | 50.2 -> 26.2 | 1660 -> 1892 |
| column_field (vector in a struct field) | 0.835 / 0.846 | 0.595 / 0.608 | 36.5 -> 13.6 | 1196 -> 1120 |
| column_global (global vector) | 0.840 / 0.848 | 0.593 / 0.613 | 36.0 -> 12.6 | 1080 -> 1004 |
| column_store (whole-element stores) | 4.630 / 4.686 | 4.270 / 4.373 | 60.4 -> 37.6 | 1728 -> 1644 |
| column_ring (ends + modulo under a guard) | 2.173 / 2.182 | 1.896 / 1.963 | 30.2 -> 13.8 | 1640 -> 1236 |
| column_call_kept (control) | 0.998 / 1.007 | 1.016 / 1.027 | 41.8 -> 42.1 | 1240 -> 1240 |

The flat workloads of the earlier section repeated within noise. With the
JIT on the column walks gain 15-29%: the check was already inlined, but its
two compares and the length load sat in the trace per element and the `or`
join kept a snapshot. The control is unchanged.

### Call effects (2026-11-12)

The first cause above is now addressed. `bone_core::effects` infers, once
per workspace, what every function may write: its own field, element and
global assignments joined with the effects of everything it calls, closed
over the call graph (a cycle of bodies that write nothing writes nothing).
A body the compiler cannot read -- `inline(lua)`, `extern(lua)`, a call
through a function value, a yield, a call the inference left unresolved --
writes everything, with these exceptions: a C function reached through
`extern(ffi, ..)` may write native elements (and may mutate any state when
its library exposes a callback or `any` parameter), an opaque body marked `@pure`, `@nowrite` or
`@writes_elems` does what it says, and a type constructor or the
`enumerate` template runs no user code. The initial push exemption was
removed in the callback audit below: appending can invoke `__newindex`.
The elision pass then drops only the facts a call could have touched: a callee that
writes field `n` drops facts through `n` and keeps the loop's length facts.

Annotating a body is a promise about raw target code that only review can
check, exactly like writing the body; `@nowrite` says the body stores to no
table field, element or global the program can reach, including NativeBuffer
storage (IO, allocation and raising are fine) and `@writes_elems` that it stores only
into elements of containers it is handed. NativeBuffer reads, string
methods, transmute and allocation can carry these promises; a Rock body
ignores them and is walked instead. Raw table access, generic string
conversion and printing cannot promise read-only effects when they can
invoke a Lua callback.

`packages/stone`, the same compile, before -> after the effect table:

| Check | Before | After |
| --- | ---: | ---: |
| `__vec_checked_get` | 634 | 522 |
| `__vec_checked_index` | 105 | 98 |
| `__vec_checked_set` | 57 | 54 |

Cold compile of Stone, 25 interleaved runs of each binary on the same
sources (Apple M4 Max): min 140 -> 144 ms, median 161 -> 165 ms, within the
run-to-run spread; the compile budget attributes the table to the codegen
fan-out, 69.6 -> 71.2 ms. The table is a linear walk of every body plus a
fixpoint over the call graph, and is dropped whenever a module's inference
is replaced, so an incremental rebuild recomputes it.

What still blocks a loop with a call: the logging functions (their sink is
a function value), `Arena.get` and the arena intrinsics, Stone's own
`stone::data` and `append_vertex` bodies (they write fields the loop's
vector is read through, or call unresolved bounded `next`s), and
`Map.remove` (a foreign Map can run a metamethod). The latter needs a
receiver provenance proof before its effect can be narrowed.

### NativeBuffers and declared result facts (2026-11-12)

A `NativeBuffer` carries the same `len` and takes the same proof: a proven
element read or write becomes `NativeBufferInBounds` (the C array `.ptr[i]`
raw, the typed array on JS) and a proven `__nb_checked_index` call, the
element check a value-struct buffer's leaf reads share, becomes
`Inst::NbIndexSlot`, the index itself. Nothing new was needed in the
analysis; the kind and the call joined the accesses it already proves.

Two annotations let an opaque stdlib helper state what its result is, since
nothing can read its body: `@returns_index_below("<param>[.field]*")` says
the result is below the live length of the vector that place holds when
the call returns, and `@returns_nonneg` that it is at least zero. The
column-map slot helpers carry them (`__map_col_slot` and
`__map_col_slot_checked` both, `__map_col_slot_of` only the first: it
returns -1 on a miss, which the caller's `>= 0` guard supplies). A slot is
below `m.data.len` because `__slot_alloc` takes the next index of `data`
and `data.len` never shrinks (`__slot_release` only pushes to the free
list). The pass seeds `lt(result, m.data)` at the call, and the column
reads that follow a lookup drop their shared check.

`packages/stone`, the same compile, before -> after:

| Check | Before | After |
| --- | ---: | ---: |
| `__vec_checked_index` | 98 | 86 |
| `__nb_checked_index` | 54 | 42 |
| `__nb_checked_get` / `set` | 62 / 31 | 62 / 31 |

The NativeBuffer element checks that remain index with stride arithmetic
(`(row * width + col) * 4 + lane`) or a literal into a buffer whose length
is a parameter; neither is covered by a local fact. The 12 column checks
the slot helpers removed are the registry lookups; the four that remain
follow a slot through a field the pass cannot name.

`benchmarks/vec_store.rock` gained four workloads. Apple M4 Max, bundled
LuaJIT 2.1, 9 interleaved rounds, min / median ns per element with the JIT
on, then min with the JIT off, checked -> elided:

| Workload | JIT checked | JIT elided | Interpreter | mcode bytes |
| --- | ---: | ---: | ---: | ---: |
| nb_fill `b[i] = v` | 1.667 / 1.754 | 1.544 / 2.038 | 54.3 -> 26.8 | 1436 -> 1432 |
| nb_scale `b[i] = b[i] * k` | 0.540 / 0.567 | 0.395 / 0.485 | 101.9 -> 48.3 | 1692 -> 1432 |
| nb_sum `t += b[i]` | 0.961 / 1.006 | 0.915 / 1.075 | 55.4 -> 26.9 | 1316 -> 1184 |
| map_lookups (column map `get` in a loop) | 1.267 / 1.332 | 1.146 / 1.342 | 86.6 -> 62.7 | 1320 -> 1432 |

With the JIT on the NativeBuffer read-modify-write gains 27% at the minimum
and the others 5-10%; medians in this run were noisier than the earlier
sections (a background load during the run), so the minimums are the
figures to read. The interpreter halves its time per element on every
NativeBuffer loop. The earlier flat and column workloads repeated within
their previous spread.

### Trust and known gaps (review of 2026-11-12)

What the elided checks rest on, from a final read of `in_bounds.rs` and
`effects.rs`, and the gaps found but left as they are:

After the native-write, transitive-edit and length-offset corrections, a
Stone compile with the pass disabled versus enabled contains 807 -> 518
checked vector reads, 155 -> 101 column checks, 65 -> 53 vector stores and
53 -> 41 NativeBuffer index checks (helper definitions excluded). On an
Apple M4 Max, development build (`opt-level = 1`), eight interleaved fresh
processes per mode after warm-up took median 93.1 -> 105.8 ms, minimum
91.5 -> 103.3 ms. This measures the entire pass and effect analysis against
`ROCK_SKULL_BOUNDS_OFF=1`, not just the effect-table addition or a release
build. The earlier release measurements above cover different comparisons.

- **Dataflow invariants.** Intersection at joins,
  SSA values tied to places and untied on every write, every call killing
  what its effect says it may write, yields and lambda-holding bodies
  giving up. Floor modulo (`%` on Lua, `a - floor(a / b) * b` on JS) lands
  in `[0, b)` for `b > 0`; length offsets retain their checks; constants key by
  value. The adversarial runtime cases pin each.
- **Trusted by review, like the code itself.** An opaque body's `@pure`,
  `@nowrite`, `@writes_elems`, `@returns_index_below` and `@returns_nonneg`;
  the map slot helpers' `data.len` invariant. A lie in any of these is a
  silent out-of-bounds access.
- **FFI.** Calls invalidate element-value facts because C can write memory
  shared with a NativeBuffer. A library exposing a callback or `any`
  parameter invalidates all memory facts on every call, including a later
  call with no callback argument. Callbacks hidden behind raw Lua or pointer
  casts still require review of that unsafe binding.
- **Log callbacks.** `_log_once_impl` and `__rock_log_init` can invoke a
  host-installed sink or level source. Their former `@nowrite` promises
  were removed; both now invalidate caller facts.
- **Foreign table metamethods.** A Lua-supplied Map can invoke `__index`
  during `Map.get` or the read in `Map.remove`; the bounds pass treats those
  calls as unknown. The subsequent callback audit extends this to raw
  table intrinsics, tuple access, direct map indexing and nullable boxed
  vector slots. Helper summaries must propagate those effects too.
- **Incremental emission.** A chunk depends on the effects of its callees,
  including effects inherited transitively. Body edits must re-emit a
  caller when a callee's effect summary changes, even when the edited
  source belongs to a deeper callee. Literal-only changes that preserve
  effects can still reuse residual callers.
- **`ref` parameters.** Assignment to a local is treated as writing
  nothing another body can see. A `ref T` parameter rebound as a whole
  would be a write the caller sees; the language reference says field
  writes go through and the probe could not form such an assignment, so
  this is assumed, not shown.
- **Interpolation.** `Inst::Format` and `ToString` still kill as unknown
  calls in the pass. Specializing their effects would require matching the
  actual rendering route and its input representation.
- **Unused proven reads** are dropped only when the operation cannot invoke
  a callback. A proven bound alone is insufficient for a nullable boxed
  vector: its missing slot can invoke `__index`.

### What would eliminate more

The remaining checks, flat and column alike, fall to the same three causes,
in order of weight:

1. **Calls inside loops**: done above (the inferred effect table). The
   remaining gap to the 327 ceiling is the callees listed there, each
   wanting a narrower effect: per-field writes already exist, a proof that a
   Map receiver has no metamethod would free `Map.remove`, and a sink that is
   a function value is unknowable without a declared effect on the sink.
2. **Accessors called with a proven index.** `@inline` on the small
   accessors moves the access to the caller, where the loop fact holds. A
   cheaper variant that keeps the call: a per-call-site specialization that
   passes an "index proven for parameter k" bit and emits the raw form in
   that specialization. The inliner's plan machinery already keys bodies per
   site, so the bit would ride on it.
3. **Data-structure invariants.** Done for the map slot helpers
   (`@returns_index_below`, above). `random_int(lo, hi)` returns `lo` when
   `hi <= lo`, so its result is below `hi` only under a `lo < hi` guard; a
   conditional form of the annotation would cover its one Stone site. An
   arena's `cur_count - 1` is below its storage only by the arena's own
   invariant, which the same annotation on the arena accessors could state.

Guards (`len - 1` / `xs[0]` without `len > 0`) are source shapes: the index
is unproven because the program does not check it, and adding the guard in
Stone is the fix there.

The 484 nil-key store guards are the remaining runtime check of note; they
need a field-presence fact instead of a bounds fact.


### Callback effect audit (2026-11-14)

The bounds optimizer must account for callbacks entered by an apparently
read-only operation. The audit through `c2333989` removes false effect
promises on raw table intrinsics, tuple and vector backing access, generic
string conversion, type-tag lookup and logging callbacks. It also corrects
the native checked setter's element-write summary. Indexed-container and
generic equality effects propagate through Rock helpers. Unused indexed
reads remain observable when a Lua metamethod or JavaScript getter can run.

Lua vectors of concrete non-null primitive scalars have a separate MIR
kind: their live slots exist, so an in-bounds access cannot enter
`__index` or `__newindex`. Nullable and unknown elements remain conservative.
This distinction does not apply to JavaScript, where a Proxy can intercept
a present slot. Push is conservative because it writes an absent slot;
recognizing a constructor alone does not prove that its result never escaped.

Compiling the unchanged `packages/stone` source with the release compiler
and each revision's stdlib gives these textual checked-helper occurrences.
They describe emitted code, not executed checks per frame.

| Helper | Before audit (`7a0bdc27`) | After audit (`c2333989`) |
| --- | ---: | ---: |
| `__vec_checked_get` | 556 | 731 |
| `__vec_checked_index` | 106 | 110 |
| `__vec_checked_set` | 55 | 62 |
| NativeBuffer get / index / set | 62 / 42 / 31 | 62 / 42 / 31 |

The earlier broad prototype treated every boxed vector slot as callback
capable and emitted 777 / 110 / 63 vector helpers. The primitive-element
proof recovers part of that cost without assuming away nullable slots.
The subsequent [heap-record audit](purity.md#heap-record-callbacks-2026-11-14)
closes the source heap-field callback boundary. Native/value representations
and container headers still rely on their ABI invariants.

#### Final runtime measurements

The final comparison is `7a0bdc27` -> `c2333989`; the subsequent commits
through `0b45e543` add tests and documentation only. Both arms use their own
stdlib. The saved baseline `vec_store` Lua was verified byte-for-byte against
a fresh emission by the `7a0bdc27` release compiler.

Apple M4 Max, system LuaJIT 2.1.1774896198, JIT enabled, four alternating
baseline/final pairs in fresh processes. Median ns per element:

| Workload | Before audit | After audit |
| --- | ---: | ---: |
| fill | 0.9663 | 0.9741 |
| increment | 0.3579 | 0.3557 |
| scale_field | 1.1302 | 1.1409 |
| set_slots | 0.3629 | 0.3683 |
| bump_entries | 2.0673 | 1.9915 |
| fill_and_collect | 2.6205 | 3.6183 |
| nb_fill | 1.0143 | 1.0149 |
| nb_scale | 0.2857 | 0.2800 |

`fill_and_collect` is 38% slower. All four baseline samples (2.6028-2.6699)
are below all four final samples (3.6069-3.6757). It stores into one vector
and pushes into another; a push can invoke a foreign backing-table callback,
so bounds facts cannot survive it without a provenance proof. The other
listed median changes are within 4%; this small run does not establish
improvements at that scale. The job began after the preceding timing job
finished, with one-minute system load 5.3 on 14 cores.

End-to-end validation on Intel Core i9-14900K, Linux, bundled LuaJIT 2.1
with the JIT enabled found no reliable runtime improvement or regression.
Repeated samples overlapped on a shared machine; native-library sources
were unchanged but each arm used a separate build. These measurements do
not establish equivalence or rule out costs in other workloads. Allocation
requests showed multiple modes on both compiler revisions, so their mixed
medians do not establish an allocation reduction either. Correctness checks
and a deterministic screenshot comparison passed.

To reproduce the public vector comparison, compile
`benchmarks/vec_store.rock` with separate release compiler builds and stdlibs
at `7a0bdc27` and `c2333989`. Alternate fresh
`luajit -e 'dofile("vec.lua"); main()'` processes four times per arm on one
host, retaining all `ns_per_elem` samples and contemporaneous system load.


### Heap-record callback follow-up (2026-11-14)

The foreign-record fix retains field callbacks, including nil setters, while
keeping local constructor scalarization and fresh single-use projections.
The final nil-store guard first checks `getmetatable(base) ~= nil`; a foreign
record always receives its write and the guard never invokes its getter.
Native fields and compiler-owned frames retain their plain-storage rules.

An intermediate raw-store version exposed the reason the guard matters:
on Apple M4 Max, LuaJIT 2.1.1774896198 with the JIT enabled, six alternating
pairs of `benchmarks/field_store.rock` measured `nullable_passthrough` at
1.0395 -> 1.8674 ns/element (+80%). All baseline samples (0.9498-1.0553)
were below all raw-store samples (1.7767-1.9724). That version was not kept.

The final callback-safe version was compared with the pre-record-audit
compiler (`153d07ec`, compiler-equivalent to `c2333989`) on Intel i9-14900K,
Linux, system LuaJIT 2.1.1785763465, JIT enabled. Both arms used the same
benchmark sources and six interleaved fresh-process pairs, reversing order
on alternating rounds; all result sinks matched. One-minute load at start
was 4.3 on 30 logical CPUs. Median ns/element:

| Workload | Before heap-record fix | Callback-safe fix |
| --- | ---: | ---: |
| nullable_passthrough | 1.0544 | 1.0575 |
| leaf_capture | 3.1854 | 3.1730 |
| row_reset | 2.3995 | 2.2558 |
| coro_points | 4.5427 | 4.5520 |
| coro_create | 63.8040 | 62.6713 |
| fill_and_collect | 4.6678 | 4.5446 |
| scale_field | 0.7683 | 0.7625 |
| nb_fill | 1.0591 | 1.0595 |
| nb_scale | 0.3775 | 0.3775 |

The nullable-store sample spans overlap (1.0542-1.0767 before,
1.0538-1.0786 after); there is no comparable JIT regression in this run.
No broad speedup is claimed: noisier vector cases included `fill` at
1.1139 -> 1.2906 with overlapping spans (1.0576-1.1968 and 1.0695-1.6447).
The earlier collection-loop cost from callback-capable push remains; this
fix does not prove a vector's backing table private.

On Apple M4 Max with LuaJIT 2.1.1774896198 and the JIT enabled, six paired
fresh-process runs compared the pre-record-audit and callback-safe versions.
Median ns/element and full sample spans for selected field workloads:

| Workload | Before fix | Callback-safe fix |
| --- | ---: | ---: |
| nullable_passthrough | 1.0431 (0.9845-1.0473) | 1.0293 (0.9982-1.0805) |
| leaf_capture | 2.7908 (2.6355-2.8441) | 2.7881 (2.6429-2.8527) |
| row_reset | 1.6110 (1.5620-1.6456) | 1.6204 (1.5397-1.6634) |
| coro_points | 2.2240 (2.0735-2.3715) | 2.2660 (2.1425-2.3180) |

All displayed spans overlap. In particular, the final guarded nullable store
does not reproduce the intermediate raw-store regression; this small run
does not establish a broader speedup.

The follow-up [guard elimination audit](guard-elimination.md) records proof
requirements across guard families and paired JIT ablations. Bounded affine
NativeBuffer indexing is the strongest new measured candidate; removing more
checks did not consistently improve the other probes.
