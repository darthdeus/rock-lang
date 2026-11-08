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
