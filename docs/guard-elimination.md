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

Unknown calls and writes to the buffer length invalidate the relevant facts.
This is a bounded, local proof for the access shape, not general affine
arithmetic. An `int` annotation, a negative-value rejection branch, or a bound
proved in another function is not enough.

### Private Lua vector pushes

For a fresh boxed vector that remains local and private, the pass tracks its
allocation and aliases through local loads and stores. Appending to that vector
cannot invoke a foreign table setter. A private push invalidates facts tied to
current length and element places, while captured upper bounds can survive
because length only grows. Unsupported escapes revoke the allocation's private
status.

This exception is Lua-specific: JavaScript arrays can inherit indexed setters.
The JS path remains conservative. Opaque calls still invalidate ordinary
memory-bound facts, even when they do not receive the private vector. Vector
parameters, container contents, and caller-to-callee privacy are not inferred.

### Fresh descriptor lengths

A `MakeStruct` constructor establishes its `len` initializer as a bound on that
new descriptor. Local assignments copy the relation; reassignment, length
writes, opaque calls and repeated producers invalidate stale facts. This proves
fills of fresh NativeBuffers when allocation and loop share the same count.
The sized primitive constructor wrappers are inline so the pass sees their
headers. No pointer-ownership or general finite-integer assumption is added.
Alias, mutation, branch and old-instance loop regressions cover the boundary.

### Literal uint shifts

Lua emission omits the shift-count helper for integer literals in `0..=31` and
does not capture the unused helper. Dynamic counts remain checked; a `uint`
type does not prove the upper bound. Right-shift flooring and left-shift uint
normalization remain in place.

## Soundness boundaries and next gaps

Unknown calls may mutate any reachable length or field, and boxed vector, map,
or heap-record accesses can invoke Lua metamethods. The analysis drops facts
where those effects can invalidate a proof. Nullable-field presence is also
not inferred from a record type: reads and writes may invoke `__index` or
`__newindex`. Keep these guards unless a plain-table and stable-presence proof
is available.

The current follow-up gaps are:

- **Recomputed capacity and reuse:** independently evaluated size expressions
  are not equated. The alpha-mask fills still need this bridge; glyph-cache
  reuse also needs equality facts across joins and helper-call effects.
- **Subtraction:** `n > 0` does not prove `n - 1 < n` for infinity or large
  rounded floating-point values. A rule needs finite exact-integer bounds and
  live operand snapshots.
- **Nullable fields:** no local proof establishes whether a heap-record field
  is present or whether clearing it can call a setter.
- **Interprocedural privacy and bounds:** a parameter type does not prove the
  caller supplied a private vector or preserve a capacity relation through a
  call. No caller-to-callee summary is implemented.

Other guards for conversions, defaults, coroutine state, and native layout or
reload compatibility remain where dynamic values or external state affect
behavior.

## Runtime measurements

The measurements below compare compiler-emitted checked code with the new
compiler output. Each benchmark used LuaJIT with the JIT enabled, alternating
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
| Fresh integer/byte buffer fills | Apple M4 Max / 2.1.1774896198 | int 0.4918 -> 0.5011; byte 0.4625 -> 0.4505 ns/element | 8 pairs; int ranges 0.4623-0.5886 -> 0.4606-0.5380; byte 0.4214-0.4670 -> 0.4338-0.4917 |
| Fresh integer/byte buffer fills | Intel Core i9-14900K, Linux / 2.1.1785763465 | int 0.9049 -> 0.8077; byte 0.4730 -> 0.3418 ns/element | 8 pairs; int ranges 0.8722-1.0725 -> 0.7594-0.9829; byte 0.4664-0.4843 -> 0.3402-0.3460 |

The strided-read ranges are trace-sensitive, especially on x86; they support a
microbenchmark result, not an application speedup. The private-vector timings
include allocation and GC, and the faster baseline trace variants make the
median direction less stable. The shift result supports smaller emitted code,
not a runtime claim. No whole-program compile-cost conclusion is supported by
the small standalone compile samples.

The parameterized `fill_and_collect` control retained its checks. Buffer-fill
measurements include allocation and GC; the byte case includes constructor
inlining. M4 timing changes are small with overlapping ranges; x86 byte fills
improved about 28%. Final buffer-fill machine code shrank on both architectures.
Ten alternating M4 fresh-process compile pairs for this change measured
`buffer_fill` 16.760 -> 16.550 ms and `vec_store` 17.672 -> 17.765 ms, with
overlapping ranges. OS caches were not flushed; startup/output are included.

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
```

Compare `proven_stride` for the affine proof and `fill_and_collect_private`
with `fill_and_collect` as the parameterized control for private pushes. The
shift and buffer drivers report result sinks and trace/machine-code sizes.
Check sinks match in each paired run. When comparing a stdlib change, set
`ROCK_STDLIB_DIR` to each build's matching sources; the CLI reads stdlib at runtime.
`benchmarks/guard_proofs.py` performs a manual valid-input-only guard ablation;
it is not compiler output and cannot establish proof soundness.
