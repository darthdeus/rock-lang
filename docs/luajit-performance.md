# LuaJIT performance

Rock targets LuaJIT with the JIT enabled. The shape of emitted Lua affects
trace formation, allocation and frame time even when two forms have identical
semantics. Measure changes on the intended workload, with the same LuaJIT and
stdlib, and include frame-time behavior when evaluating an application. An
interpreter-only result does not establish a JIT improvement.

## Trace pressure

LuaJIT traces have limits on constants, live registers, code size and inlining.
A hot side exit needs the values in its parent snapshot, so a large live set at
a guard can prevent compilation even when the code following the guard is
small. Repeatedly failing exits and trace-cache flushes can also produce uneven
frame times. Reduce unnecessary work and the live state at hot exits before
assuming a larger runtime limit will fix a workload.

The recorder can follow calls across several helpers in one trace. When a
per-row loop hits the constant limit, try separate passes for independent
work so each pass has its own loop back edge. Preserve ordering wherever
one step can observe another. Run `python3 benchmarks/long_trace.py` with
Rock and LuaJIT on the path to compare the two loop shapes on fixed work.

Every call a trace records spends a budget. A tail call counts against
`loopunroll` for the rest of the trace, and any other call keeps the caller's
frame under the 250-slot trace limit while the callee runs. The compiler
splices small loop-free callees at their call sites, as it does `@inline`
ones, so short forwarding helpers spend neither.

Values flattened into scalar lanes avoid allocation, but wide values increase
argument, local and snapshot pressure. Boxing trades that pressure for
allocation and indirection; neither choice is universally faster. The shared
ABI plan and frame checks are described in [compiler structure](compiler.md).

## Emitted code

The compiler emits numeric `for` loops where the iterator protocol permits it,
allowing LuaJIT to recognize a counted loop. Inlining and scalarization can
remove temporary iterator and value allocations; runtime dispatch or an opaque
call can prevent those transformations. Bounds checks are eliminated only when
proven redundant, accounting for mutations and callbacks.

Lua tables may have metatables, including tables received from target code.
An indexed read or write can invoke a callback even when it looks like a simple
access. The optimizer must preserve those effects. Compiler-owned plain
storage and unescaped allocations can support stronger proofs. Nil-store guards
avoid unnecessary writes without suppressing foreign setters.

LuaJIT treats a tail call into C differently from a normal call. Generated FFI
wrappers retain a Lua return frame where needed for traceability. Native buffer
views and shader layouts have separate representation contracts; see
[compiler structure](compiler.md).

The emitter can clear dead locals so they do not remain in exit snapshots, and
binds eligible callees as upvalues to reduce repeated global lookups. Those
choices trade additional emitted instructions or upvalues against trace
pressure. Changes to them need runtime measurements, not just smaller Lua.

## Measuring

The compiler's benchmarks in `benchmarks/` exercise individual emitted shapes;
`perf/` contains compile measurements. Compare builds against the same
source inputs and LuaJIT executable, alternate runs, and inspect the output as
well as timing. A microbenchmark identifies the cost of a particular shape; it
does not by itself establish a whole-application improvement.

`rock perf <scenario> --gc-window N --gc-window-mode running --format json`
reports trace starts, compilations, aborts and flushes for an allocation window.
When adding a LuaJIT trace hook, chain existing `jit.attach` handlers: LuaJIT
keeps one handler per event and an application can install its own.

## Nullable tuple returns

A `-> (T, U)?` returns a boxed tuple or nil. If the tuple contains a `nobox`
value, its box holds that value's scalar leaves. LuaJIT can sink a fresh box
when construction and destructuring happen in one trace. A stored tuple can
pass through as its existing box when copied from one container to another.
Measure escaping values and calls across trace boundaries to see when a box
survives.
