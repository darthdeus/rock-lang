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

A structural verifier ([`verify.rs`](../crates/skull-mir/src/verify.rs))
checks each body between passes, and lowering audits the concrete plans it
is handed. These checks only ever fail on a compiler bug, so a `rock` process
skips them; tests run them in debug builds. `ROCK_MIR_CHECKS=1` turns them on
for a compile under investigation, `ROCK_MIR_CHECKS=0` turns them off.

HIR may simplify an expression while its source span still contains the original
syntax. A source edit that classifies by HIR must account for the source text;
for example, inlining a folded expression can still require parentheses.

The incremental emitter can publish changed bodies as an overlay. The overlay
must retain changes accumulated since the full build and publish compatible
code slots transactionally, so a failed update does not expose a partial set of
bodies. Interface or incompatible layout changes require the appropriate full
build or restart path. See [`mir_emit/overlay.rs`](../crates/skull/src/mir_emit/overlay.rs) and
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

## Intrinsics

`@intrinsic` declares a primitive in the stdlib and resolves it through a
backend mapping, then a portable fallback, then a spanned error. The mapping
inventory lives in [`intrinsics.rs`](../crates/skull-mir/src/intrinsics.rs) and
the stdlib annotations; these are the constraints.

- A fn has one body, and that slot is the LuaJIT story. A Rock body is the
  portable fallback and the differential-test reference. An `inline(lua)` body
  is the LuaJIT mapping, declared in source; GLSL and JS mappings are Rust
  tables. A hard-error `inline(lua)` body still counts as the LuaJIT mapping,
  so a GPU-only intrinsic fails at run time on LuaJIT, not at compile time.
- Template placeholders are `$1..$n`.
- `@native_required` marks the small core every backend must map natively: no
  fallback rung, covered by hand-written semantic tests because differential
  mode cannot cover them. Nothing starts native-required by default; promote
  with evidence. A Rock-body fallback does not satisfy it.
- Per-type `NativeBuffer` declarations stay. A single generic get/set entry
  does not hold at the Rock level (phantom int contract, concrete-versus-generic
  overload ambiguity); each per-type declaration has its own mangled key and
  maps to the same one-line template per backend.
- A table key matching no `@intrinsic` declaration is a test-time error, so
  signature drift breaks loudly. A reachable call with no mapping and no
  fallback, or a `@native_required` fn with no mapping, is a spanned error.
- A misplaced `@intrinsic` on a non-fn item is not rejected, because HIR drops
  item-level annotations off non-fn items.

## NativeBuffer representation

`NativeBuffer<T> { ptr: Ptr<T>, len: int }` is a transparent struct and
`Ptr<T>` an empty phantom type; nothing about the buffer's shape is
LuaJIT-specific. `Ptr<T>` means a backend-native buffer handle, not a raw
machine address: a cdata pointer on LuaJIT, the TypedArray itself on JS,
absent on GLSL. A JS `Ptr<T>` always denotes the whole buffer, since offset
arithmetic exists only in `inline(lua)` and extern surfaces that reject on JS.
There is no field gate and no type whitelist: any reachable call to an
unmapped intrinsic rejects at emit with a span, and that is enough.

`get(b, i) -> T?` stays a single-hop `@intrinsic @native_required` rather than
portable Rock over `nb_get`: a wrapper frame is free only where the JIT traces
it away, and every call frame is paid on an interpreter-only LuaJIT.

Store edges: JS typed arrays wrap on store, matching C truncation for the
sized variants, and `NativeBuffer<int>` is pinned 32-bit. A NaN store into an
integer buffer is a programmer error, checked in debug mode by the JS
`__nb_checked_set` shim and backend-native in release. Rejected: an opaque
backend-intrinsic type plus a native-tier field gate, per-backend stdlib
files, and collapsing the sized variants.

`buf[i]` on a buffer whose `T` is an FFI-clean value struct addresses the
element's leaves through typed views (`ContainerKind::NativeBufferLeaf`,
`Inst::FfiViewCast`), never a struct slot, so the element never materializes
and a `nobox` type can live in one. Storage and the emitted cdef are
unchanged. One `ffi.cast` view per distinct leaf C type is made at buffer
construction and stored beside `ptr`; a leaf access is
`view[i * stride + off]` with compiler-computed constants, cross-checked once
at module init against `ffi.offsetof` along each leaf path. A second drift
axis is invisible to that check: leaves are matched to offsets positionally,
so if the leaf-path list and the layout walk disagreed on order every
`ffi.offsetof` assertion would pass while every leaf was written to the wrong
address. The `RepPlan` C-layout projection compares the two sequences and
rejects on disagreement, and the raw walker is private. The JS backend uses
one TypedArray per leaf C type over a single `ArrayBuffer` with the same plan;
an element whose C size is not a multiple of 4 rejects on JS.

Any hand-written `inline(lua)` that reseats `buf.ptr` leaves every view
pointing at the collected old array. It must call `__nb_refresh_views(buf)`
(`stdlib/unified.rock`, with the JS sibling in `stdlib/js.rock`). The
compiler cannot see into an `inline(lua)` body, so this is an unchecked
contract of the same family as `@nomangle`. The view fields are never
serialized by state dumps or hot reload.

## Iterators

`for` over user types and lazy iterator chains desugar through the
`iter`/`next` protocol that skull inlines, scalarizes and re-emits as a
native counted `for`. The design substrate is settled: a `(bool, E)` done
signal via `phantom()`, multi-return `next`, HIR-level desugar before the CFG,
and `@inline` scalarization.

- Do not reintroduce closure-based iterators or runtime `type()` dispatch.
- `for i in lo..hi` keeps its direct numeric lowering; never desugar it.
- Perf claims are proven by an artifact property (the golden Lua test
  `for_user_iter_inline_is_zero_alloc` in `skull-codegen-lua`) or a
  fixed-workload bench, never by allocation deltas in a running program.
- There is no blanket `iter<I>(it: I) -> I`: it would make every `iter(vec)`
  ambiguous. `next` and `iter` resolve by receiver type alone.
- Map keeps its lowering template: a pure-Rock `MapIter` cannot express Lua's
  stateful `next(tbl, key)`, and an `inline(lua)` `next` is an opaque call
  whose cursor cannot scalarize.
- Every fn whose return type is a tuple returns its elements as multiple Lua
  values; the ABI is keyed on the static callee type, so direct, indirect and
  lambda callees of one fn type agree. Nullable-tuple returns stay boxed.
  `inline(lua)` fns returning tuples must return multiple values, not a table.
- FORL recovery (`skull-transform/src/forl.rs`) rewrites tagged protocol loops
  into a counted `for`; LuaJIT emits a counted back-edge only for a
  source-level numeric `for`, and a goto loop is about 1.8x slower on a
  trivial body. It fires only on loops the desugar tagged: a user-written
  `while i < xs.len` must keep re-reading `len`. Snapshot semantics follow:
  `len` is read once at loop entry and mutating the collection mid-loop is
  undefined. Do not remove FORL recovery or the stdlib shapes it matches.
- Bounds require exactly `(bool, E)`. Multi-value iterators drive a `for` but
  cannot be wrapped by a combinator, so `enumerate` and `zip` are chain
  terminals. Bounded fns have no runtime identity: always force-inlined,
  rejected as fn values, and residual calls after inlining are a named error.
- Combinator struct fields are `__`-prefixed: a bare `inner` field breaks
  placeholder-lambda field inference in user code. `FlattenIter` holds its
  inner iterator in a real field plus a `__has` gate, because `next` on a
  nullable would mutate a throwaway copy.
- A mapped lambda rides the cursor as a fn value, one indirect call per
  element off-trace; write lambdas at the call site in hot loops.
- Rejected: `windows(n)` and `chunks(n)` with runtime N yielding `[T]`
  (allocates a Vec per step).

## Compile-time inputs

Tier 1 (`build_var`, `embed_string`, `[build.vars]`) is the user-facing
surface in the [language reference](language-reference.md#compile-time-inputs).
The decisions behind it:

- Every comptime input is declared: source text, a file named by a literal
  path (content-hashed into the cache key), or a build variable declared in
  `stick.toml` (value-hashed). Nothing else is observable at compile time.
  Compile-time execution does not break incrementality; untracked inputs do.
- No `@shell(..)` in expression position. Commands live in `[build.vars]` and
  run once per session build, before compilation; the compiler receives
  values. Type-check-only paths (lint, watch, LSP) stay unstamped and never
  run commands.
- Resolution is one chokepoint: every batch emission path stamps through
  `comptime::stamp_workspace` and every emitting session is built by
  `bone_backend::bone_session_for_emission`. Per-site assignments are how
  `rock test` and `rock watch` once shipped broken while `rock run` worked.
- Resolution order is declaration default, then channel-matched `cmd`, then
  explicit `--var`. `--channel` rejects when the project declares no
  channels. There is no ambient-environment fallback, and the release verifier
  must not reintroduce one.
- `secret = true` is metadata hygiene, not confidentiality: the value is
  allowed in the compiled payload and its staging intermediates and forbidden
  in logs, diagnostics, manifests and cache metadata, which carry only a
  fingerprint. A credential shipped in a client is recoverable by anyone
  holding it.
- A var's value and an embedded file's content hash join the cache key of
  every body that reads them, including global initializers and decorator
  definitions, and must reach every re-emit path. Byte identity of fresh
  builds cannot see staleness here; the gate is an incremental fixture.

A pure evaluator (tier 2) is designed but not started; do not begin it
without author sanction. If built, it interprets typechecked HIR (never the
AST, never a second VM, never LuaJIT), produces values only (no types as
values, no string-to-code eval), admits by a purity effect gate with spanned
rejections, and runs under a fuel budget. Explicitly out: comptime FFI, IO
and process exec, build-system-in-comptime, and AST or type introspection.

## Dependency-gated wires

`@wire(onchange=[CONST, ...])` is a qualified form of `@wire`, not a general
change-observation feature. It extends the compiler's single dependency and
invalidation authority and adds no wire-specific graph, runtime decorator
registry, polling loop or mutable signal system.

- An unqualified `@wire` runs at boot and after every successful reload. A
  gated wire runs once at boot, then after a compatible publication only when
  its body fingerprint or a declared dependency's canonical fingerprint
  (including the transitive const closure) changes.
- V1 admits only top-level immutable `const` declarations with representable
  canonical values. Runtime writes are not observed: no setters, write
  barriers, dirty flags or polling.
- Identities are logical keys, never diagnostic names, emitted names or
  source text. Adding or removing a gated wire, or changing its dependency
  list, is a structural transition that restarts or rejects; it never leaves
  stale host state.
- Publication order: stage the artifact and plans, select due hooks by
  comparing old and candidate entries, publish, then execute the scheduled
  hooks once. Failed compilation, materialization or publication runs no wire
  effect. A hook failure after publication is reported as a post-publication
  failure; opaque FFI effects are not claimed to roll back, so wire effects
  stay cheap, idempotent and setter-shaped.

## Error recovery in the editor session

An editor edits invalid code constantly, and the LSP must still answer hover,
definition, completion and diagnostics without a restart. After any sequence
of edits, once the source is valid the workspace must become correct with no
stale residue; the `incremental == fresh` oracle in
[`bone-incremental`](../crates/bone-incremental/tests/incremental.rs) proves
this and every change to recovery lands with it green.

Lowering is total: an unresolved type name in any position resolves to
`BuiltinTy::Error` with a diagnostic, and the pass returns a coherent,
possibly error-typed workspace. Fail-fast lowering would force keep-last-good,
which is coherent but stale and hides the dependent's real error. A body
lowering error leaves the function with its signature and no body. The
interface cascade is one level: the edited module, the modules whose
interface names a removed name, and the interface-error seed; a shape change
keeps the name, so dependents need only re-inference
(`shape_change_relowers_only_the_edited_module`).

`Error` is a distinct builtin poison type, not reused `never`: it absorbs in
`unify` and `coerce` in both directions and is identity in `join`, so a root
error produces no secondary errors, and it renders as `<error>` on hover.
Every binding form poisons on failure. Block-sibling binders bind their
pattern slots to `Error` and then return the root error; nested-body binders
(`for`, `while let`, `if let`) substitute `Error` for the scrutinee, still
infer the body so hover and completion work inside it, and defer the root
error. A failing block tail keeps its partial inference.

## Body-only incremental emission

The monomorphization pipeline made `InstanceGraph`, `MonoInterner`,
`ArtifactView` and concrete plans authoritative. A body-only edit scales with
changed declarations, their live concrete instances, the reachability delta
they cause and the chunks that contain the changed body or ABI, never with
total modules, owners, plans or chunks. There is one production path (session
body update, graph contribution delta, concrete-plan delta, artifact view
delta, affected chunk rebuild, overlay publication) and no legacy lowering
path, second graph, concrete table, reachability oracle, lint cache or
publication mechanism beside it. A shader or opaque-boundary root-contract
change requests a full build, not a widened body algorithm.

Contracts, each with a structural gate (timing is acceptance evidence, not a
unit-test oracle):

1. One logical identity: the retained graph and every consumer use the same
   monotonic `MonoInterner`.
2. One primary graph record: an owner's `Contribution` is authoritative, and
   cross-owner indices are derived, retractable projections. The old summary
   is captured at replacement time so every derived index can retract.
3. One concrete-plan answer: lowering, inlining, shader construction, JS, ABI
   planning and incremental emission consume the same per-instance entry.
4. Body edits retain interfaces: roots, mangling, backend sibling selection
   and signature shapes do not change on a body edit; a violation routes
   through the full path.
5. Reachability may change: a body edit can add or remove calls, generic
   instances, bounded witnesses, function-value targets and shader
   dependencies, and owner-local does not mean these cannot propagate.
6. Spans stay exact after same-line, length-changing and newline-changing
   edits, and span relocation never selects semantic walks.
7. No hot shared locks or whole-table copy-on-write; the single-threaded edit
   path mutates the uniquely owned store between emits, and an entry for
   generation N never serves N+1.
8. Determinism: fresh, sequential, parallel, incremental and opposite worker
   completion orders produce the same identities, diagnostics and bytes.
9. Counters carry denominators: every asserted zero needs a populated request
   or changed-body denominator, with no shared locks or atomics on the path.
10. Full builders are oracles, not fallbacks: re-adding `concrete_body` or
    `artifact::realize` to the body-edit path must turn
    `exact_body_edit_has_no_full_concrete_or_artifact_rebuild` red.

Graph pruning searches only the suspect region seeded by instances that lost
an in-edge; roots and always-emitted instances are anchors, an unsupported
seed expands so unanchored cycles of any length are pruned, and the result is
independent of branch order. Rejected: the one-hop predecessor liveness proxy,
the direct-root-only anchor predicate, and treating every external blocker as
permanently live. The artifact fingerprint version is bumped whenever the
value algorithm changes. `IncrementalLintCache` is the only reusable lint
derivation cache; unknown change is never treated as unchanged, a global
finding is never served stale, and fresh `lint_workspace` is its oracle.

Non-goals: interface-local reload optimization, changing mono identity or
representation semantics, persistent cross-process artifacts, and hiding cost
by skipping diagnostics, lints, shaders or ABI checks. Stop and report rather
than work around it if a plan update needs a second derivation mechanism, a
view cannot retract without rediscovering from HIR or MIR, an outer-map clone
or shared lock is required per edit, an incremental result differs from a
fresh one, or timing improves only because a correctness surface stopped
running. Verification for any change here: byte identity of all Lua artifacts
against the parent, fresh versus incremental versus overlay equality in
default and sequential builds, a mutation check for the property claimed, and
timing from fixed release binaries in balanced order with mean, p50 and p99.

## Shader binding and layout

A Rock struct is the binding unit and the compiler owns its layout, so no
GLSL identifier for a Rock-authored resource is typed twice, CPU data uploads
without per-field name lookups, and the emitter could target something other
than GL without game code changing. Names are a GL-only concept; the struct is
a binding group, field order is the layout. Only hand-authored GLSL and
caller-owned custom pipelines use the by-name lane.

- Each stage's `@uniform` is its own block at its own binding; one vertex fn
  may be consumed by several fragment stages with different uniform structs.
  `@engine` is the exception: one per program, both stages name the same
  type, because the engine fills it without a per-game layout. The compiler
  guarantees one engine block per program at binding 0 and the engine checks
  the type at registration against the shipped fingerprint.
- `@textures` is a separate role because textures cannot live in a uniform
  buffer in any API. Compiler-assigned units make a unit collision
  unrepresentable; callers never choose a name, offset or binding number.
- A buffer's wire format is a property of the buffer, not the struct. A value
  struct keeps its C representation everywhere and is packed at the binding
  boundary: vertex and instance buffers use the struct's C layout from the
  `RepPlan` projection (cross-checked by `ffi.offsetof` at chunk load),
  uniform blocks use std140 from `gpu_layout::uniform_block`. Within a
  context there is one layout function that both the packer and the shader
  declaration read; a disagreement is silent garbage, not a compile error.
- Rejected: making the cdata representation be std140. Value structs are
  scalarized so there is usually nothing to memcpy; a block `bool` is 32 bits
  while Rock's is one byte and padding cannot be asserted by `ffi.offsetof`;
  and it would make a storage decision global to the type.
- The driver's std140 implementation sees only active members, so production
  validation is partial by construction and `GL_UNIFORM_BLOCK_DATA_SIZE` must
  not be asserted equal to the plan size.
- GLES 3.0 guarantees only 16 KB per uniform block, enforced at compile time
  because the GLSL profile follows the runtime context and no desktop driver
  reports the floor. Loose array uniforms live in the default block, whose ES
  3.0 guarantee is far smaller, which is why array consumers use blocks.
- `@size(N)` array params are not fields of the `@uniform` struct. Each stage
  that declares them gets its own block at a reserved binding, so a per-draw
  scalar upload cannot invalidate live array contents and "a block is
  uploaded whole" stays the invariant. Do not add an array variant to
  `UniformBlockPlan.members`: its leaf paths must equal the struct's
  `value_leaf_paths` and the packer binds them by position. A distinct
  `UniformArrayBlockPlan` shares the std140 authority with its own member
  kind.
- Registration queries the block's size and member offsets and asserts them
  against the computed layout, eagerly and loudly, because hot-reload layout
  drift is silent garbage.
- `ShaderInterfacePlan` owns the logical interface and
  `skull-codegen-glsl::ShaderAbiPlan` the physical GL projection. GL is the
  only renderer; the interface IR keeps a second backend possible without a
  rewrite, and a second backend derives its own layout from its own plan
  rather than reusing the GL answer.
