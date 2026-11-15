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
- `cargo test -p rock-cli --lib failed_overlay -- --nocapture` passes two
  tests. They cover retained callers and CodeRefs, integer and false constants,
  no-box constant leaves, removal of a newly emitted generic specialization,
  and a successful cumulative retry after failure.
- Disabling the wrapper fails both tests. Omitting constant targets from the
  journal fails the no-CodeRef test. Both mutations were restored, and the
  restored focused tests pass.
- `cargo test -p skull` passed 109 unit and 1296 integration tests, with one
  existing ignored test. `cargo test -p rock-cli` passed 429 library, three
  binary, 126 integration and seven Stone-launch tests, with four ignored
  library tests. The child-lifetime subprocess test also passed.
- On the repaired no-CodeRef fixture, the emitted overlay grows from 688 to
  1610 bytes and saves nine globals. LuaJIT with the JIT enabled, parsed once,
  ran ten balanced batches of 20,000 reload executions per version. Mean
  batch averages were 0.094 microseconds without the journal and 0.696 with it.
  Median batch averages were 0.088 and 0.695 microseconds, and the maximum
  batch averages were 0.153 and 0.716 microseconds. This small fixture measures
  reload execution, with no steady-state performance claim.

## Reproduced follow-up and open questions

A separate static-retry defect remains. Start with this program and warm its
static by calling `observe()` after loading the full artifact.

```rock
fn value() -> int {
    static cell = 1
    cell
}
const LATE: int = 0
fn explode() -> int { error("late overlay failure") return 0 }
@nomangle fn observe() { print(value()) }
fn main() { observe() print(LATE) }
```

Emit an overlay changing `static cell = 1` to `static cell = 2` and `LATE`'s
initializer to `explode()`. Load it under a protected call and observe its
failure. Repair only `LATE` back to `0`, then emit and load the cumulative
overlay using the same compiler cache. `observe()` still prints `1`. A fresh
build of the repaired program prints `2`.

A temporary `static_retry_discrepancy_probe` in the existing runtime-test module
ran through `cargo test -p rock-cli --lib static_retry_discrepancy_probe --
--nocapture`. Its one discrepancy assertion passed and its temporary source was
removed. It also checked that the failed overlay contains
`__rock_static_prepare`, while the repaired overlay omits it. The compiler's
`advance_static_baseline` records the edited initializer at emission time,
before the failed load reaches its reset. The repaired overlay then treats the
initializer as already current. This change leaves that reset-baseline issue
for a separate repair.

Stronger isolation during initialization and rollback of external effects
would require an explicit host and language contract.
