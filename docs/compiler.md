# Compiler structure

The compiler accepts Rock source and emits Lua, JavaScript or GLSL. The host
provides files, build inputs and diagnostics; the compiler owns admission and
representation decisions. The command-line pipeline starts in
[`skull/src/pipeline.rs`](../crates/skull/src/pipeline.rs).

- `skull-parser` parses and expands source. Batch compilation rejects syntax
  errors even when the parser can recover a tree for editor use.
- `bone-core` defines HIR, types and the workspace. `bone-lower` builds HIR,
  `bone-inference` checks bodies, and `bone-semantic` coordinates the frontend.
- `bone-incremental` maintains a workspace across edits; `bone-query` and
  `rock-lsp` expose editor queries.
- `skull-graph` tracks reachable concrete instances. `skull-mir` lowers checked
  programs to MIR and owns shared representation decisions; `skull-transform`
  optimizes MIR, and the codegen crates emit each target. `skull` coordinates
  the backend.

## MIR and representations

MIR is a typed control-flow graph: mutable variables are cells, instructions
produce indexed results, and blocks end in terminators. Shared decisions belong
in MIR or its plans, so definitions, callers and backends agree. The structural
verifier can be enabled with `ROCK_MIR_CHECKS=1`; the CLI leaves it off by
default. See [testing](testing.md) for running compiler tests.

Function names are derived from signatures rather than declaration order, so
compatible reloads retain stable symbols. Entrypoints and explicitly unmangled
functions retain their declared names; raw target code that calls a Rock
function by that name needs that guarantee.

The ABI planner chooses one representation for each definition and its calls.
Ordinary values can be split into scalar lanes or boxed to fit Lua frame limits.
A boxed ordinary value stores its `nobox` children inline. A `nobox` value uses
scalar lanes in the Lua call ABI. Function values and opaque calls have their own
boundary plans. Value copy semantics and layouts hold across wire formats.
Immutable call and return boundaries may reuse existing boxed value children,
including an immutable local selected by `if` or `match` from proven immutable
bindings; mutable, borrowed, stored and foreign sources keep their snapshots.

Opaque `inline(lua)`, `inline(js)` and `extern` bodies cannot be inspected or
rewritten. Their concrete parameter and return representations are checked at
the boundary. `@repr(opaque)` waives the container-layout check for a parameter
whose body does not assume that layout; it does not waive no-box checks.

JavaScript multi-value calls return one leaf directly and place the others in a
scratch bank. A call and its bank reads must stay together, since another call
can overwrite the bank. An opaque JS body returning multiple lanes publishes
all of them through `__rock_publish` in its final top-level return.

An `@intrinsic` resolves to a backend mapping, then a portable Rock fallback,
or a spanned error when neither exists. `@native_required` removes the fallback
option. Lua mappings can live in the declaration; JavaScript and GLSL mappings
are backend tables. This keeps target-specific primitives behind one declaration.

`NativeBuffer<T>` holds a backend-native buffer and length: LuaJIT uses cdata,
JavaScript uses typed arrays. Value-struct element leaves share a buffer with
views derived from the common representation plan. Raw target code that
reseats a buffer pointer must refresh its views; the compiler cannot inspect
that code. Shader vertex and instance layouts use the C layout projection;
uniform blocks use std140, with the packer and declaration sharing a plan.
Texture bindings and uniform blocks are assigned by the compiler, rather than
requiring game code to duplicate names, offsets or binding numbers.
Each shader stage has its own uniform block; the engine block is shared by the
program and checked against the engine's expected type. Textures and uniform
arrays have separate binding roles. Buffer layout belongs to the binding rather
than the source type: forcing std140 on every value would change ordinary C
layout and still require packing.

## Optimizations and effects

Optimizations must preserve observable errors and callbacks as well as values.
For nilness, the proof used to remove a branch is stricter than the information
used to skip a redundant nil store: a wrong branch decision can remove live
code. A field store on a foreign Lua table can invoke a metatable, so guarding
it must not introduce a getter or suppress a setter.

Bounds checks on vectors and native buffers are removed only when dataflow
establishes a live bound. Mutations, unknown calls and callback-capable accesses
invalidate affected facts. The compiler infers effects transitively for Rock
bodies; annotations on opaque bodies are trusted promises. A private local
allocation can support stronger reasoning only while it remains unescaped.
See [purity](purity.md) for the trust boundary and
[LuaJIT performance](luajit-performance.md) for why emitted shapes matter.

`@preflight("prepare", "tail")` on a Lua direct call is a trusted callee
contract: the function must equal `tail(prepare(receiver), arguments)`,
including error and release order. The compiler runs `prepare` before
constructing value arguments only when it proves those arguments effect-free;
otherwise the call keeps its original order.

User iterators use an `iter`/`next` protocol which can be inlined and lowered
to a native counted loop. Range loops have direct numeric lowering. Bounds on
iterator types require a `(bool, E)` result; multi-value iterators can drive a
direct `for` but cannot satisfy that bound. Collection iteration captures its
length at entry; mutating the collection during iteration is undefined. A
hand-written `while` must retain its own semantics and re-read the length.
Mutable iterator structs can use `@consume` to mark their cursor values as
affine: operations that consume a cursor make the previous binding unusable,
while the source collection or range remains reusable.

## Compile-time inputs

Compile-time inputs are explicit: source, literal-path embedded files and build
variables declared in the package manifest. File content and variable values
participate in the cache keys of bodies that use them. Commands for build
variables run before an emitting build; type-check-only paths do not run them.
A secret variable is excluded from logs and cache metadata, but can still be
embedded in output and is not confidential once shipped. The user-facing
syntax is in the [language reference](language-reference.md#compile-time-inputs).

## Incremental compilation and reload

The editor must continue to answer queries on incomplete source. Lowering
represents failed types with an error type and reports diagnostics while keeping
a coherent workspace. After edits restore valid source, incremental results must
match a fresh workspace, without stale errors or definitions.

Body edits update the retained instance graph and concrete plans, including
reachability changes, then rebuild affected chunks. Interface or incompatible
layout changes take the appropriate full-build or restart path. Dependencies
include transitive effects and compile-time inputs, not just the edited source.

A compatible Lua overlay accumulates changes since the last full build and
publishes code slots together. If definition installation, constant
initialization or shader registration fails, the overlay restores the
compiler-owned global bindings it replaced and does not publish pending
CodeRefs. Static resets occur after initialization and registration, immediately
before slot publication. The applied initializer state belongs to the runtime,
so a failed attempt can retry a reset or removal in a later cumulative overlay
without discarding state from a previous successful load.

Rollback is limited to those bindings. Mutations inside existing tables,
program globals, I/O and foreign effects can survive a failed initializer;
code invoked during initialization can observe candidate definitions. Wire
hooks run after publication, so their failure does not undo published code.
`@wire(onchange=[...])` uses compiler dependency fingerprints to decide when a
hook runs after a compatible publication, rather than observing runtime writes.

## Splatting tuple boxes

A tuple carrying a `nobox` element is boxed in one layout inside Rock code:
the multi-return wire put in a table, leaves flat, with `tuple_box_shape` as
the oracle. Every box of such a tuple -- stored, loaded, returned, joined,
yielded -- has that layout, so a reader rebuilds the parts from the type
alone. The one exception is a tuple still being assembled from its parts
(`element_tuple`), which is read back element by element before it goes
anywhere; a name that binds a whole splatting tuple holds its slots, never
such a box. A tuple is immutable, so a box is shared rather than copied: an
unwritten name bound straight from a stored box hands that box on when it is
used whole, repacking slots read in full from one box yields the box (also
after inlining, from the box widths each body records), and a join whose
arms are all boxes meets as one box rather than as slots. `materialize(t)` of a splatting tuple is therefore `t` itself.
A body the compiler cannot see into (a user `inline` or `extern` callee,
`inspect`, interpolation) is handed the tuple as written instead: an
explicit `materialize(t)` argument there boxes one value per element, and an
implicit splatting tuple is rejected, because the flat box would read as a
wider tuple. The stdlib's opaque storage primitives hold the flat box without
reading it.
