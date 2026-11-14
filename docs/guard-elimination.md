# Guard elimination audit

Audit of compiler revision `1340cf0a`, 2026-11-14. This follows the heap-record
callback fix and the earlier bounds/effect audit in
[LuaJIT performance](luajit-performance.md). It examines bounds, field-presence,
numeric conversion, defaults, enum/type, coroutine and reload checks. It is a
source/probe audit, not an exhaustive dynamic census of every application.

The useful question is whether a guard's predicate and lack of observable
side effects can both be proved. A bound does not prove a table read is inert:
an absent nullable slot can invoke `__index`, and a heap-record write can invoke
`__newindex`. The new `Record` MIR operations deliberately preserve that boundary.

## Measured priorities

`benchmarks/guard_proofs.rock` contains small source examples. The Lua runner
uses valid, fixed inputs and the Python driver compares the emitted program
with exact, asserted textual removals of selected guards. These removals are
experiments, **not sound compiler transformations**. In particular, the accessor
accepts arbitrary indices outside this runner, and the stride function's
negative rejection tests are not sufficient range proofs for arbitrary Lua
numbers.

Run these commands through the normal job scheduler, reserving an exclusive
timing slot for the measurement:

```sh
rock compile benchmarks/guard_proofs.rock --output /tmp/guard-proofs.lua
# After compilation completes:
python3 benchmarks/guard_proofs.py /tmp/guard-proofs.lua
```

The driver requires `luajit` on PATH, emits raw JSON, checks equal result sinks,
and reports medians, ranges and trace-abort counts. Exact replacement assertions
must be reviewed when code generation changes; do not silently weaken them.

Intel Core i9-14900K, Linux, LuaJIT 2.1.1785763465, JIT enabled, six independent
process pairs with alternating order. Each workload flushes traces, warms for
200 invocations, then measures. Start load was 1.1 on 30 available logical CPUs.
The allocation workloads include allocation and GC. Units are ns/element,
except fixed-buffer which is ns/invocation. These are microbenchmarks, not
end-to-end application results.

| Probe | Checked median (range) | Unchecked median (range) | Interpretation |
| --- | --- | --- | --- |
| Accessor inside counted loop | 0.3537 (0.3535-0.3601) | 0.3537 (0.3535-0.3560) | No measured benefit from call-site specialization here. |
| Fresh vectors, push to separate output | 3.0258 (3.0022-6.1931) | 5.4465 (5.3400-6.2174) | Trace-sensitive; removal is not automatically faster. |
| NativeBuffer, four lanes per row | 1.6759 (1.6679-1.6911) | 1.2361 (0.3534-1.2388) | Best candidate, about 26% lower median, but trace-sensitive. |
| Allocate four-element NativeBuffer, initialize two slots | 155.3454 (154.2807-162.0380) | 175.9320 (174.3970-182.0363) | Allocation dominates; removal worsened this probe. |

Accessor and fresh-vector runs had no trace aborts. Fixed-buffer runs had 47
in both versions. Stride checked runs had 1-4 and unchecked runs 0-3. The outlier
and differing trace paths require trace inspection and a second architecture
before attributing all of the stride difference to per-element comparisons.

A second six-pair run using the checked-in driver reproduced stride medians
1.6762 -> 1.2372 and accessor medians 0.3536 -> 0.3537. Fixed-buffer was
155.3864 -> 177.8551. Fresh-vector medians reversed direction, 6.2031 ->
4.4380, with overlapping ranges 3.0547-6.3582 and 2.7320-6.4085. That reversal
is why no fresh-vector speed claim is supported by these probes.

## Proof opportunities

### 1. Bounded affine NativeBuffer indices

The current pass (`crates/skull-transform/src/in_bounds.rs`) tracks nonnegative,
less-than-length and at-most-length relations, but not general affine intervals.
`buf[row * 4 + lane]` stays checked under a buffer-capacity guard and counted
row/lane loops. The probe confirms the emitted `__nb_checked_get` remains.

A small exact-integer interval domain could establish:

- `0 <= rows <= 100000`, `buf.len >= rows * 4`;
- `0 <= row < rows`, `0 <= lane < 4`;
- therefore `0 <= row * 4 + lane < buf.len` without rounding/overflow.

Require ordered comparisons that exclude NaN and a finite exact-integer domain.
The probe deliberately uses negative rejection conditions: `not (rows < 0)`
is not equivalent to `rows >= 0` for NaN. NativeBuffer header/representation
invariants remain the existing ABI contract. Unknown callbacks must still
invalidate live header facts. A guard in a different function does not prove
this access unless its relation is carried by a sound summary.

Start with constant stride and bounded integer induction, not arbitrary symbolic
algebra. Required regressions: zero/negative rows, insufficient capacity, last
lane, NaN/infinity/large rounded numbers, mutated length, callback invalidation,
and both Lua and JS behavior. Measure cold compile cost as well as JIT runtime.

### 2. Fresh private backing-table provenance

`ArrayPush` currently clears facts as an unknown call. A push into a genuinely
fresh, private plain backing table cannot invoke a foreign metamethod. This
could preserve bounds of a distinct input vector in collection loops and
recover some of the previously measured callback-audit regression.

Freshness of the wrapper alone is insufficient: prove the backing allocation,
all aliases, no escape to a host call/global/container/captured closure, no
metatable installation, and no callback during element materialization. Growing
one vector can invalidate its own length relation; it must not accidentally
preserve facts about an alias of that vector. Prefer a narrowly scoped local
allocation proof before adding global points-to analysis.

The earlier public `fill_and_collect` measurement motivates this work, but the
new small probe did not improve on x86. Evaluate the actual regression workload
on both architectures before landing machinery solely for performance.

### 3. Bounded subtraction and constant allocation lengths

`lt_of` intentionally does not infer `n - 1 < n`: infinity and sufficiently
large floating-point integers violate that implication. A finite exact-integer
interval can make `count - 1` safe under `count > 0 && count <= xs.len()`.
The probe's negative rejection form needs additional NaN handling.

Fresh `native_buffer_int(4)` emits checks on its first stores to indices zero
and one. Subsequent reads become raw after the checked stores establish facts.
Seeding constructor length facts could eliminate the first checks. It needs
allocation identity and invalidation on header writes/escape. The allocation
probe got slower when checks were removed, so this is a simplicity/code-size
opportunity pending a better workload, not a demonstrated runtime win.

### 4. Conditional result bounds

`random_int(lo, hi)` returns `lo` when `hi <= lo`; an unconditional
`returns_index_below(hi)` promise is false. The nonempty sample path is a
candidate for a conditional result relation, with finite integer inputs and
arithmetic limits accounted for. Existing `@returns_index_below` annotations
on map slot helpers are trusted contracts; any new conditional contract needs
direct positive and negative tests, including invalidation of the referenced
length. Do not add an annotation merely to silence an emitted check.

### 5. Field-presence and metatable checks

A known nonnil value needs no absent-nil-store guard. For a maybe-nil value,
a compiler-owned plain table and a stable known-present field could allow a raw
store. A known-absent nil store can disappear only when it cannot invoke a
setter. These facts must survive aliases, field mutation and callbacks.

The callback-safe guard is ordered to test metatable presence before reading
the field. It handles a protected metatable whose public value is `false`.
Removing it broadly is wrong; using raw stores broadly also caused the measured
nullable-store rehash regression. A LuaJIT guard in generated source is not by
itself evidence of runtime cost.

## Other guard families reviewed

| Family | Existing behavior / remaining proof boundary |
| --- | --- |
| Default arguments | `lower/mod.rs::fill_param_defaults` and direct-call folding already handle supported constants. Keep omission checks for function values, host calls, explicit nullable defaults and lazy effectful defaults. |
| Enum/tag dispatch | Constant cases and local scalar replacement already remove many tags. Dynamic `any`, reflected values, foreign records and escaping payloads still need discrimination. |
| Checked uint conversion | `__rock_checked_u32` validates `[0, 4294967295]` then floors. A proven finite range can remove comparisons; fractional values still require flooring. |
| Float-to-int conversion | The Lua `float_to_int` function value in `stdlib/unified.rock:1770` checks the host value type, then floors. Direct MIR `FloatToInt` emits `math.floor` without that host type guard; fractional values still require flooring. |
| Shift count | A constant count alone does not currently remove `__rock_u32_shift_count`; a whole constant shift expression may fold. A uint type alone does not prove `<= 31`; a range fact could remove the runtime check. No measured workload yet. |
| Coroutine status/hooks | `next` distinguishes suspended from terminal after resume, including a real nullable yield. Hooks and terminal/error checks encode behavior. Source absence of hook registration alone does not prove a mutable frame has no hook. |
| Coroutine `pcall` | Converts step errors to the errored state and performs cleanup. Removal requires a complete no-raise proof, including nested callbacks and defers. |
| Native layout / reload compatibility | Checks validate foreign data or version changes. Compile-time knowledge of the current body does not establish compatibility with an old live value. |
| Vector false/nil fallback | `data[i] or checked_get` takes the slow path for false, but a nil-only test is not a general replacement for bounds checking. Preserve nullable-slot callbacks and out-of-range behavior. |

Relevant implementation entry points are `skull-transform/src/in_bounds.rs`,
`copy_prop.rs`, `sroa.rs`, `skull-mir/src/nilness.rs`,
`skull-codegen-lua/src/preludes.rs`, `value.rs`, `term.rs`, and
`skull/src/coroutine_emit.rs` (all under `crates/`), plus `stdlib/unified.rock`.
Existing regression suites include `opaque_effects`, `default_guard_fold`,
`nullable_embed_clear`, `coroutine_frame_writes`, and scalarization suites.

## Next implementation order

1. The bounded affine follow-up is implemented below. Broaden it only against
   a real NativeBuffer consumer; retain the trace-sensitivity caveat.
2. Recover private-vector push effects against the existing collection workload;
   retain the callback regression matrix as the safety boundary.
3. Reuse any resulting small interval domain for subtraction and numeric guards.
4. Defer call-site specialization, constructor facts and coroutine specialization
   until a measured workload warrants their additional machinery.

The initial audit removed no compiler guards. Its manual benchmark ablation
must never be used as production output. The implemented follow-up below is
a separate compiler proof, with narrower applicability than that ablation.

## Implemented follow-up: bounded constant stride

The bounds pass now recognizes `row * stride + lane` with a positive constant
stride. It proves whole-number operands, nonnegative row/lane, ordered
`row < rows`, `lane < stride`, and `rows * stride <= buffer.len`, with both
products and the sum within the exact binary64 integer range. The whole-number
certificate is separate from finiteness: flooring may produce NaN or infinity,
which the ordered finite bounds must exclude. A public `int` annotation alone
is not a certificate.

`guard_stride_proven` in the benchmark explicitly converts a float count with
`as int` and tests positive comparisons before entering the loops. Its Lua
inner read is now raw. The original `guard_stride`, with an unvalidated count
and negative rejection tests, stays checked. No runtime validation or changed
input behavior is injected into user programs.

The extra analysis activates only in functions with the target access shape.
It uses finite sets of facts, intersects them at joins, and bounds each upper
bound search by depth and work. It does not grow intervals one integer per
solver iteration. Arithmetic snapshot keys invalidate expansion when a MIR
operand producer executes again. Unknown callbacks and length writes invalidate
the proof. This implementation intentionally does not add general symbolic
algebra, interprocedural specialization, or integer assumptions at host entry.

Regression coverage includes the initial failing MIR case; fractional operands;
NaN-admitting false edges for count and length guards; excessive arithmetic
ranges; an extra lane; callbacks; length writes; a stale capacity after count
reassignment; and a lane changed across loop iterations. Lua and JS runtime
fixtures cover zero, negative, fractional, infinite and NaN counts and short
buffers. The normal JS fixture retains its helper and checks boundary parity;
the measured elimination is in the Lua pipeline.

For the actual compiler comparison, emit the same benchmark source with the
pre-change compiler and the new release compiler, then run each artifact as:

```sh
luajit benchmarks/guard_proofs.lua emitted.lua proven_stride
```

The checked-in harness warms for 200 calls, then runs 100000 calls over 4096
integer elements with LuaJIT enabled. Eight alternating fresh-process pairs on
Intel Core i9-14900K / Linux / LuaJIT 2.1.1785763465 measured 1.6850 -> 1.2449
ns/element (about 26% lower median). Sample ranges were 1.6775-1.7217 and
0.3542-1.2549; sinks matched. Both versions had 0-3 trace aborts across samples.
The fast outlier remains evidence of trace sensitivity, not a general
application speedup. This confirms the earlier ablation with actual generated
output and an explicit sound input domain.

The Stone source survey found no existing loop with this complete local proof.
The nearest cases are alpha-mask row writes in `packages/stone/engine/ui/mouse.rock`,
glyph-cache lanes in `packages/stone/engine/rendering/text.rock`, and record
serialization in `packages/stone/engine/debug/perf_records.rock`. Their capacity
relations come from allocations, equality/reuse paths or producer invariants;
the required whole/count bounds are not locally established. Those checks stay.
No extra guards were added to application code merely to exercise this proof.
Allocation provenance is the next useful bridge to those consumers.

The same eight-pair run on Apple M4 Max / LuaJIT 2.1.1774896198 measured
1.1930 -> 0.8062 ns/element (about 32% lower median), with non-overlapping
ranges 1.1815-1.2249 and 0.7940-0.8180. Sinks matched; both versions had 0-2
trace aborts per sample. The final compiler's emitted probe was byte-identical
to the artifact used for both architecture measurements.

Fresh-process compilation on the M4 used ten alternating pairs per standalone
fixture, the same source/stdlib, separate before/after release compilers and no
on-disk compiler cache. OS file caches were not flushed. Wall-clock milliseconds
include process startup and writing emitted output:

| Fixture | Before median (range) | After median (range) |
| --- | --- | --- |
| guard_proofs | 15.361 (14.846-81.480) | 15.355 (14.904-84.541) |
| field_store | 15.511 (15.348-16.849) | 15.742 (15.401-16.415) |
| vec_loop | 17.888 (17.669-18.505) | 18.024 (17.450-18.704) |

The sample spans overlap and median differences are at most 1.5%. This small
standalone set does not establish compile-cost neutrality for large programs.
The Stone module was excluded because it is not a standalone CLI input; no
successful whole-Stone compile measurement is claimed here.

## Implemented follow-up: private Lua vector pushes

The bounds pass now tracks fresh boxed `MakeVec` allocations and their aliases
through unborrowed local stores/loads. Each allocation creates both a plain
header and a plain backing table. Unsupported uses revoke the entire alias
family: opaque calls receiving it, backing-field exposure, globals, fields,
container elements, closure environments and borrowed cells. Joins intersect
facts. Reexecuting an allocation site creates a new value fact without restoring
an old escaped cell alias. Container contents are not recursively tracked.

A private `ArrayPush` or recognized stdlib `Vec.push`/`vec_push` can only grow
the numeric length and store an element. It invalidates current length-place
and element-place facts, but keeps captured upper bounds. The stdlib recognition
checks the module, receiver type, parameter binding and reviewed inline body;
a changed body falls back to conservative effects. Publishing a private vector
as an appended item revokes its provenance, including self-appends. The append
itself remains callback-free if its receiver was private before that append.
Returning the finished vector does not invalidate earlier proofs.

This guarantee is **Lua-specific**. A fresh JavaScript array can inherit an
indexed setter from `Array.prototype`. The pass chain now supplies its emission
target explicitly; the generic API and JS path keep the conservative behavior.
A JS inherited-setter fixture and a target-specific MIR test pin that boundary.
The global memory-effect table is unchanged: opaque calls remain opaque, and
private provenance is not inferred from a parameter's `[T]` type or annotation.

The original `fill_and_collect(xs, out)` receives its output as a parameter.
It still retains its bounds check; recovering its earlier regression would
require a caller/escape proof across the call boundary. The new companion
`fill_and_collect_private(xs)` allocates its output locally and returns its
length. Before/after Lua from `benchmarks/vec_store.rock` differs at exactly one
line: the companion's checked input store becomes a raw store. The parameterized
control and every other emitted benchmark function are unchanged.

Two independent batches of six alternating fresh-process pairs used identical
benchmark sources and LuaJIT enabled. All result sinks matched. Units below are
ns/input element, including output allocation and GC. M4 is Apple M4 Max with
LuaJIT 2.1.1774896198; x86 is Intel Core i9-14900K / Linux with LuaJIT
2.1.1785763465.

| Machine / batch | Private before median (range) | Private after median (range) | Parameterized control before -> after median |
| --- | --- | --- | --- |
| M4 / first | 4.1577 (2.1345-4.2658) | 3.1629 (3.1308-3.4280) | 3.5693 -> 3.5475 |
| M4 / repeat | 4.4662 (2.1943-4.5130) | 3.3445 (3.3246-3.4091) | 3.7726 -> 3.7635 |
| x86 / first | 5.4484 (2.1521-5.6926) | 4.6523 (4.3745-4.7840) | 4.8388 -> 4.8910 |
| x86 / repeat | 5.2100 (2.2035-5.4510) | 4.4473 (4.3279-4.6455) | 4.5929 -> 4.7178 |

Private-case medians improved about 24-25% on M4 and 15% on x86, but the baseline
has a faster trace variant around 2.1-2.2 ns/element. The result is trace-sensitive,
not a guarantee that every run improves. In the first batch, the warmup's M4
machine-code sizes were 2148-2392 bytes before and 2048-2052 after; x86 was
2430-2644 before and 2347-2428 after. Warmup recorded no aborts for either case.
These counters are captured before the timed section, not throughout it.

To reproduce, compile `benchmarks/vec_store.rock` with before/after release
compilers, then alternate fresh processes running each emitted file with
`luajit -e 'dofile("vec.lua"); main()'`. Compare the two collection workload
lines, retaining all samples, controls and trace counters. The new companion
uses the existing benchmark's warmup, timing and sink checks.

Regression coverage includes local aliases; both push spellings; literal/spread
append machinery; escapes through calls, globals, records, elements, nested
constructors and closures; branches/reassignment to foreign vectors; self
publication; borrowed cells; and escaped old instances across allocation-site
reexecution. An opaque call without a reference to the allocation does not
publish it, although that call still invalidates ordinary memory bounds facts.

Private tracking is enabled only for Lua functions containing a fresh vector
and an eligible append. The final compiler emitted byte-identical Lua to the
runtime-measured artifact. Fresh-process compilation on M4 used ten alternating
pairs per fixture, the same source/stdlib and separate release compilers, with
no on-disk compiler cache. OS file caches were not flushed. Wall-clock
milliseconds include startup and writing output:

| Fixture | Before median (range) | After median (range) |
| --- | --- | --- |
| field_store | 16.158 (15.294-18.663) | 16.253 (15.560-18.127) |
| vec_loop | 18.472 (17.632-21.185) | 18.552 (18.084-20.159) |
| vec_store | 17.045 (16.263-90.453) | 17.527 (16.611-398.568) |

Median overhead is below 0.6% for the first two fixtures and 2.8% for
`vec_store`. Ranges overlap and include large outliers; these small standalone
samples do not establish compile-cost neutrality or whole-program cost.
