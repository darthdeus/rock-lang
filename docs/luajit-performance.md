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

The prototypes were not retained. Reducing an individual snapshot or fixing
one repeatedly failing exit did not establish a consistent pooled runtime
improvement. In the scoring reproduction, even removing three doubles leaves
more than the x86 side-head budget because the caller retains a wide value.
The compiler still needs a way to avoid constructing or loading those leaves
before an independent guarded call, with evaluation order preserved.
