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
