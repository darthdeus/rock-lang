# Compiler structure

The command-line compiler orchestrates parsing, semantic checking and backend
emission. For the executable pipeline and its gates, start at
[`skull/src/pipeline.rs`](../crates/skull/src/pipeline.rs). The host supplies
filesystem, build inputs and diagnostic handling; the compiler decides whether
an input is accepted.

- `skull-parser` parses and expands source syntax. The command-line pipeline
  rejects recovered syntax errors; editor queries can use partial trees.
- `bone-core` defines the HIR, types and workspace. `bone-lower` lowers syntax
  into HIR, `bone-inference` checks bodies, and `bone-semantic` coordinates
  semantic passes and exposes the front-end API.
- `bone-incremental` maintains a `Session` across edits. Its update result
  distinguishes body, interface, scope and full-user changes, as well as
  unchanged input. `bone-query` uses that state for editor queries, and
  `rock-lsp` serves the protocol.
- `skull-mir` lowers checked programs into MIR and owns shared code identities
  and representation decisions. `skull-transform` runs MIR transforms;
  `skull-graph` constructs the concrete instance and artifact graphs.
  `skull-codegen-lua`, `skull-codegen-js` and `skull-codegen-glsl` emit their
  respective targets. `skull` coordinates these stages.

## LSP investigation

Completion ranking and its `sort_text` encoding share the field order in
[`rank_key.rs`](../crates/bone-query/src/rank_key.rs); the
[invariants](../crates/bone-query/src/invariants.rs) check that response order
agrees with it. Use the current key and ranking tests when investigating a
result, since client-side sorting can change the displayed order.

`rock lsp-survey <project> --format json` profiles generated body, signature
and broken edits through the workspace and reports their update tiers and
latencies. It is an in-process measurement, not a JSON-RPC round trip. The
Neovim plugin provides `:RockCapture [note]` for a local bug snapshot; see its
[README](../editors/nvim/README.md).

## MIR and emission

MIR is a typed control-flow graph. Mutable variables are cells read and written
by instructions; blocks end in terminators. Instruction indices identify their
results. See [`mir.rs`](../crates/skull-mir/src/mir.rs) for the actual IR and
its invariants. Shared lowering decisions belong in MIR or its plans rather
than being guessed independently by each emitter. Sequence indexing and keyed
lookup are distinct: a backend's sequence-index adjustment must not change map
keys.

Function mangling is shared by the emitters in
[`mangle.rs`](../crates/skull-mir/src/mangle.rs). Names depend on the function
signature rather than declaration order or the presence of other overloads;
collisions are rejected. This stability matters for code retained across a
compatible reload. `extern`, `@nomangle`, `@test`, `main` and `test_*`
functions keep their declared names. Raw `inline(lua)` code that calls a Rock
function by its declared name therefore needs that function to retain its name.

HIR may simplify an expression while its source span still contains the original
syntax. A source edit that classifies by HIR must account for the source text;
for example, inlining a folded expression can still require parentheses.

The incremental emitter can publish changed bodies as an overlay. The overlay
must retain changes accumulated since the full build and publish compatible
code slots transactionally, so a failed update does not expose a partial set of
bodies. Interface or incompatible layout changes require the appropriate full
build or restart path. See [`mir_emit.rs`](../crates/skull/src/mir_emit.rs) and
its incremental tests for the implementation and current boundaries.

For tests and fixture formats, see [Testing compiler changes](testing.md).

## Lua value ABI

The shared [ABI planner](../crates/skull-mir/src/lower/abi.rs) selects the
wire representation consistently for definitions and callers. For ordinary
values on scalar Rock-to-Rock calls, payloads above 48 leaves become one boxed
slot. A signature still above 96 lanes then boxes its widest eligible ordinary
values until it fits. The payload cut excludes a nullable presence flag; the
signature budget counts actual parameter and result lanes, including presence
flags. These are backend policy limits, not language size limits.

`nobox` values are never boxing candidates. An ordinary GC-table value whose
box would implicitly materialize a nested `nobox` child also stays scalar.
Keep this exemption until a concrete workload justifies another representation;
do not label an implicit child allocation explicit merely to bypass verification.
An explicit [`materialize`](language-reference.md#no-box-values) permits a box.
Function-value and opaque foreign/inline boundaries have their own ABI plans;
do not apply the direct-call cut independently at a call site.

A scalar signature can still exceed LuaJIT's parse limits. The Lua emitter's
[frame checks](../crates/skull-codegen-lua/src/frame_limits.rs) diagnose known
frame/local overflows; they do not make every no-box signature executable.
Parse limits and runtime trace pressure are different constraints. A smaller
scalar signature is not by itself evidence of faster execution or less
allocation. Use the [emitted-frame diagnostic](guide.md#backends) to investigate
call pressure, then measure the actual workload before changing representation.

The [wide-value ABI tests](../crates/skull/tests/wide_value_abi.rs) cover call
kinds and boxed value semantics. Representation changes must preserve value
copies and field layout, including uniform packing and nested no-box fields.

## Opaque body boundaries

An `inline(lua)` or `inline(js)` body is target code the compiler cannot
rewrite, and an `extern` has no Rock body to inspect. At a concrete call,
[boundary checks](../crates/skull-mir/src/lower/nobox.rs) reject declared
parameter and return representations that the opaque body cannot safely assume.
The container check uses the concrete declared slots, including an implicit
receiver, rather than the types of caller expressions. A slot declared `any`
does not assert a container layout. A parameter marked `@repr(opaque)` waives
only the container-layout check for that parameter: the body must treat its
value without assuming that layout. This does not waive the no-box checks.
Compiler-routed container operations are replaced before the opaque call check.

## JavaScript multi-value returns

The JS emitter returns the first leaf directly and publishes remaining leaves
in a program-sized scratch bank. Capturing a call and all its bank reads must
remain indivisible: another call between publication and capture would overwrite
the result. See [`multi_assign`](../crates/skull-codegen-js/src/emit.rs) and the
[multi-value tests](../crates/skull-codegen-js/tests/multivalue.rs).
Reference leaves can remain reachable until overwritten, bounded by the bank's
width. Clearing every slot on each call adds stores; retain the bounded bank
unless a measured retention problem warrants a different policy.

An `inline(js)` body with a multi-lane result must end with one top-level
`return __rock_publish(...)`, with one argument per ABI result lane. It must
not contain another return or access the scratch bank directly. A zero- or
single-lane result uses an ordinary return. The emitter validates this shape;
the body remains responsible for the meaning and order of its values.
