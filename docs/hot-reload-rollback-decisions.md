# Failed hot-reload overlays

## Behavior contract

A compatible Lua overlay that fails during definition installation, constant
initialization or shader registration restores the compiler-owned global
bindings it replaced. Retained ordinary callers and retained CodeRefs therefore
continue to reach the previous bodies after the failed load returns. An absent
binding becomes absent again. Existing false values survive restoration.
Successful overlays keep the candidate definitions and constants.

The emitter inventories ordinary and synthetic definitions with the existing
Lua definition-header inventory. It adds rebound callee names, affected constant
assignment targets, including no-box leaves, and emitted enum-name tables. A
reload-local journal saves those bindings before execution. An `xpcall` aborts pending CodeRef publication, restores
them and rethrows a failure. The journal is
released after execution. Published function bodies keep their ordinary Lua
environments and direct-call instructions.

## Reproduction and decision

The previous late-initializer regression retained a registry CodeRef. Its slot
stayed on the old body while the changed global function definition had already
been installed. Adding a retained ordinary caller reproduced a failure after
slot abort. A mutually recursive edge keeps its call as a global lookup, so
callee capture cannot hide the problem. The reproducer now raises through a
real Rock initializer.

The chosen journal makes rollback proportional to globals emitted by the
overlay. Each saved binding uses one table so both nil and false are preserved.
Snapshot statements are partitioned across Lua protos to bound constant-table
pressure. Environment indirection would affect subsequent calls and the
behavior of closures created while staging. The journal confines its extra
lookup, allocation and protected-call work to reload execution.

## Limits and publication boundary

The journal restores bindings. Mutations inside a referenced table, writes to
mutable program globals, foreign effects, I/O and shader-driver effects can
survive a failed initializer. The no-CodeRef regression deliberately increments
a mutable global before raising and checks that this effect survives.
Candidate definitions may be observed by code invoked during initialization.
Full-program loading retains its existing behavior.

Static reset operations run after constant initialization and shader
registration, immediately before slot publication. Overlay generation stages
no DefinitionPlan. Its slot-begin operation clears pending definition
materializers, so this publication tail executes compiler-owned plain-table
static, slot and lifecycle-plan updates. It executes no user factory or wire
hook. This contract assumes intact runtime helpers and compiler-owned plain
tables. Runtime-helper replacement, storage metatable mutation and allocation
failure during publication require separate treatment. Wire hooks run after
publication, and their failures retain the published candidate.

## Evidence

- The strengthened retained-caller regression failed before the fix. Its
  CodeRef still ran old code, while its retained direct caller reached the new
  function and raised on an unavailable candidate constant.
- `cargo test -p rock-cli --lib failed_overlay -- --nocapture` passes four
  tests. They cover retained callers and CodeRefs, integer and false constants,
  no-box constant leaves, removal of a newly emitted generic specialization,
  a successful cumulative retry after failure, and static reset/removal retries.
- Disabling the wrapper fails both tests. Omitting constant targets from the
  journal fails the no-CodeRef test. Both mutations were restored, and the
  restored focused tests pass.
- `cargo test -p skull` passed 109 unit and 1298 integration tests, with one
  existing ignored test. `cargo test -p rock-cli` passed 431 library, three
  binary, 126 integration and seven Stone-launch tests, with four ignored
  library tests. The child-lifetime subprocess test also passed.
- On the repaired no-CodeRef fixture, the emitted overlay grows from 688 to
  1610 bytes and saves nine globals. LuaJIT with the JIT enabled, parsed once,
  ran ten balanced batches of 20,000 reload executions per version. Mean
  batch averages were 0.094 microseconds without the journal and 0.696 with it.
  Median batch averages were 0.088 and 0.695 microseconds, and the maximum
  batch averages were 0.153 and 0.716 microseconds. This small fixture measures
  reload execution, with no steady-state performance claim.

## Static reset retries

The previous emitter treated a generated overlay as accepted. It advanced the
compiler's static initializer baseline immediately, so a later initializer
failure could prevent the reset from running while making the next cumulative
overlay omit that reset. The permanent
`failed_overlay_static_initializer_retry_preserves_successive_live_state`
regression reproduced this before the fix. Its repaired overlay kept counting
from the old initializer, producing `5` where `101` was expected.

Runtime `__rock_static_init_keys` now supplies the applied initializer truth.
Every cumulative overlay carries preparation for current statics in its changed
bodies. `__rock_static_prepare` compares each requested key to the runtime key
and clears the warmed value only when they differ. Repeated successful loads
therefore preserve accumulated state, while a repaired load retries a
previously unexecuted reset. Reverting to an earlier initializer is also an
ordinary runtime key transition.

The compiler retains storage identities of emitted statics until the next full
build. Its `static_cells_since_full` inventory records candidate storage and
supplies drop requests for identities absent from current chunks. It retains
removed entries because a removal overlay can also fail or be skipped. Dropping
absent storage is idempotent. The permanent
`failed_overlay_static_removal_retry_drops_cells_and_can_reintroduce_them`
regression reproduced the previous omission of a retried drop, and now checks
removal, reintroduction and repeated successful application.

The tests also count generated transitions. In the one-static initializer
fixture, each of the failed, repaired, unrelated and reverted cumulative
overlays carries one prepare and zero drops. Previously the repaired and
unrelated overlays omitted that prepare. In the removal fixture, the failed
and repaired overlays each carry zero prepares and one drop. Reintroduction
carries one prepare and zero drops. Repeated application of those requests is
covered by the runtime assertions. These are statement counts for the fixtures,
with no speed claim.

Both tests failed before the fix and pass afterward. The initializer test also
checks that failed loading preserves the old warmed counter, repaired loading
resets it once, and subsequent reloads and unrelated edits preserve live state.
The existing successful static reload regressions remain part of the crate
suite.

Static storage remains one value/done pair keyed by the owning logical callable,
binding name and declaration ordinal. Its identity is independent of the
initializer and type. Prepare requests use the current type and initializer
hash, so a type transition clears the previous value in that same pair. Existing
CodeArtifact compatibility and mutable-global layout gates still decide
whether an edit may use the overlay path. The shared `StaticBaseline` record is
also consumed by JS, which still uses its initializer-key field. Lua reuses its
storage-slot fields and makes no acceptance decision from that recorded key.

No host acknowledgment API is needed for these static transitions. Their
acceptance state resides in the runtime table updated by the publication tail.
The retained compiler inventory and emitted prepare/drop statements can grow
with statics encountered between full builds. Preparation adds work during
reload execution. Static reads and calls keep their existing emitted code.
The publication-tail assumptions and external-effect limits above still apply.
This repair concerns Lua overlays.

## Open questions

Stronger isolation during initialization and rollback of external effects
would require an explicit host and language contract.
