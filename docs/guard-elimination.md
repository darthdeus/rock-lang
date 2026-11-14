# Guard elimination

This document records the bounds and shift checks the compiler currently
proves redundant, their soundness boundaries, and the measurements that support
them. The bounds analysis is in `crates/skull-transform/src/in_bounds.rs`;
constant shift emission is in `crates/skull-codegen-lua/src/lib.rs`.

## Implemented proofs

### Bounded constant-stride indices

For an access shaped like `row * stride + lane`, the bounds pass can prove the
access when it has whole-number certificates, nonnegative `row` and `lane`,
`row < rows`, `lane < stride`, and `rows * stride <= buffer.len`. The constant
stride must be positive, and both products and their sum must stay within the
exact integer range of binary64 (`2^53`). Ordered comparisons on their true
edges exclude NaN; false edges do not establish these facts. Arithmetic
snapshots stop a repeated MIR producer from lending an old result new bounds.

Calls and length writes invalidate affected facts. An `int` annotation,
a negative-value rejection branch, or a bound in another function is not enough.

### Private Lua vector pushes

For a fresh boxed vector that remains local and private, the pass tracks its
allocation and aliases through local loads and stores. Appending to that vector
cannot invoke a foreign table setter. A private push invalidates facts tied to
current length and element places, while captured upper bounds can survive
because length only grows. Unsupported escapes revoke privacy.

This exception is Lua-specific: JavaScript arrays can inherit indexed setters.
The JS path remains conservative. Opaque calls still invalidate ordinary
memory-bound facts, even when they do not receive the private vector. Vector
parameters, container contents, and caller-to-callee privacy are not inferred.

### Descriptor capacities

A `MakeStruct` constructor establishes its `len` initializer as a bound on that
new descriptor. Local assignments copy the relation; reassignment, length
writes, opaque calls and repeated producers invalidate stale facts. This proves
fills of fresh NativeBuffers when allocation and loop share the same count or
repeat `width * height` over unchanged local operands. Product keys preserve
operand order and are not recursively expanded or reassociated. Only valid
unborrowed local-cell ties stand for repeated reads; other operands retain their
value snapshots. Writes and reexecution invalidate dependent products.
The sized primitive constructor wrappers are inline so the pass sees their
headers. No pointer-ownership or general finite-integer assumption is added.
True equality (or false inequality) transfers existing live capacity bounds.
Reuse and allocation branches therefore retain their shared bound at a join;
all incoming paths must establish it. Stale length snapshots cannot restore a
bound invalidated by mutation. No general alias is inferred from equality.

### Literal uint shifts

Lua emission omits the shift-count helper for integer literals in `0..=31` and
does not capture the unused helper. Dynamic counts remain checked; a `uint`
type does not prove the upper bound. Right-shift flooring and left-shift uint
normalization remain in place.

## Soundness boundaries and next gaps

Unknown calls and callback-capable table accesses invalidate affected facts.
Heap record types do not prove plain storage or field presence; nullable-field
guards remain unless those properties are established.

- **Record-backed capacity and reuse:** repeated field/global/element reads
  are not equated. Alpha-mask dimensions cross an opaque allocator and record
  reads can invoke getters; a changing-width regression retains its check.
  Glyph-cache capacity joins are supported, but its record reads and finite row
  bounds remain unproved. The glyph fetch writes nine scratch floats; existing
  callback-free FFI effects preserve headers and the wrapper adds a global write.
- **Subtraction:** `n > 0` does not prove `n - 1 < n` for infinity or large
  rounded floating-point values. A rule needs finite exact-integer bounds and
  live operand snapshots.
- **Nullable fields:** no local proof establishes whether a heap-record field
  is present or whether clearing it can call a setter.
- **Interprocedural privacy and bounds:** a parameter type does not prove the
  caller supplied a private vector or preserve a capacity relation through a
  call. No caller-to-callee summary is implemented.

## Runtime measurements

x86 LuaJIT decides runtime trade-offs for this work. These measurements compare
compiler-emitted checked code with the optimized output. Each benchmark used LuaJIT with the JIT enabled, alternating
fresh processes, and matching result sinks. These are microbenchmarks, not
application results. Ranges and trace behavior show that some medians are
trace-sensitive.

| Proof | Machine / LuaJIT | Before -> after median | Sample range / caveat |
| --- | --- | --- | --- |
| Strided NativeBuffer read | Apple M4 Max / 2.1.1774896198 | 1.1930 -> 0.8062 ns/element | 8 pairs; ranges 1.1815-1.2249 -> 0.7940-0.8180; 0-2 trace aborts |
| Strided NativeBuffer read | Intel Core i9-14900K, Linux / 2.1.1785763465 | 1.6850 -> 1.2449 ns/element | 8 pairs; ranges 1.6775-1.7217 -> 0.3542-1.2549; 0-3 aborts and a fast outlier |
| Private vector push and collection | Apple M4 Max / 2.1.1774896198 | 4.1577 -> 3.1629, repeat 4.4662 -> 3.3445 ns/element | 6 pairs per batch; first/repeat ranges before -> after: 2.13-4.27 / 2.19-4.51 -> 3.13-3.43 / 3.32-3.41 |
| Private vector push and collection | Intel Core i9-14900K, Linux / 2.1.1785763465 | 5.4484 -> 4.6523, repeat 5.2100 -> 4.4473 ns/element | 6 pairs per batch; first/repeat ranges before -> after: 2.15-5.69 / 2.20-5.45 -> 4.37-4.78 / 4.33-4.65 |
| Constant uint shifts | Apple M4 Max / 2.1.1774896198 | 33.6624 -> 33.6220 ns/iteration | 8 pairs; runtime effectively unchanged; emitted chunk 145372 -> 145188 bytes, final machine code 1696-1788 -> 1572 bytes |
| Repeated-product byte fill | Apple M4 Max / 2.1.1774896198 | 0.4114 -> 0.4233; repeat 0.4077 -> 0.4295 ns/element | 8 pairs per batch; repeat ranges 0.3836-0.4372 -> 0.4200-0.4956; 3-5% slower medians |
| Repeated-product byte fill | Intel Core i9-14900K, Linux / 2.1.1785763465 | 0.4550 -> 0.3321 ns/element | 8-pair repeat; ranges 0.4536-0.4604 -> 0.3306-0.3374; about 27% faster |
| Capacity join, ten-lane reuse fill | Intel Core i9-14900K, Linux / 2.1.1785763465 | 1.6654 -> 1.1239; repeat 1.3831 -> 0.9250 ns/element | 8 pairs per batch; repeat ranges 1.2955-1.6219 -> 0.8608-1.1158; about 33% faster |

Private-vector timings include allocation and GC; faster baseline trace variants
limit the median claim. These probes do not establish application speedups or
whole-program compile cost. Constant shifts demonstrate smaller code only.

The parameterized `fill_and_collect` control retains its checks. Buffer probes
include allocation and GC. Repeated products change only the checked store;
M4's slower medians persisted despite smaller machine code (376 -> 208 bytes).
The capacity-join probe changes one store, preserves its known-effect helper,
and tests reuse and allocation paths. Allocation-path medians improved 19-44%
across the two batches, with overlapping ranges. This bounded probe is not a
measurement of the actual Stone glyph cache. Ten alternating M4 fresh-process
compile pairs measured `buffer_reuse` 15.743 -> 15.671 ms and `vec_store`
16.845 -> 16.711 ms, with overlapping ranges. Startup/output are included;
OS caches were not flushed.

## Reproduction

Compile the same benchmark with each compiler build and keep each emitted Lua
file for comparison. The runtime harnesses use the output directly:

```sh
rock compile benchmarks/guard_proofs.rock --output /tmp/guard-proofs.lua
luajit benchmarks/guard_proofs.lua /tmp/guard-proofs.lua proven_stride

rock compile benchmarks/vec_store.rock --output /tmp/vec-store.lua
luajit -e 'dofile("/tmp/vec-store.lua"); main()'

rock compile benchmarks/uint_shift.rock --output /tmp/uint-shift.lua
luajit benchmarks/uint_shift.lua /tmp/uint-shift.lua

rock compile benchmarks/buffer_fill.rock --output /tmp/buffer-fill.lua
luajit benchmarks/buffer_fill.lua /tmp/buffer-fill.lua
rock compile benchmarks/buffer_reuse.rock --output /tmp/buffer-reuse.lua
luajit benchmarks/buffer_reuse.lua /tmp/buffer-reuse.lua
```

Compare `proven_stride` for the affine proof and `fill_and_collect_private`
with `fill_and_collect` as the parameterized control for private pushes. The
shift and buffer drivers report result sinks and trace/machine-code sizes.
Check sinks match in each paired run. When comparing a stdlib change, set
`ROCK_STDLIB_DIR` to each build's matching sources; the CLI reads stdlib at runtime.
