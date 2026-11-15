# Root-hole refresh and concrete-instance counts

## Decision

Share the repeated root-hole calculations while preserving the callers' work
scopes. A fresh build enumerates the union of root-hole names and indexed
failure names. Incremental derivation passes its dirty-name set. Both use one
owner-row aggregation and deletion rule. Full and local instance refreshes
share canonical declaration deduplication and instance-count summation.

The full instance pass now iterates its hole map directly, removing its
temporary cloned name vector. This removes source duplication and allocation
sites. No compile-time speedup is claimed.

## Separate correctness repair

`RootHole.instances` counts concrete instances reached at other call sites,
while the hole's obligation survives. Commit `b50e838d` introduced this meaning
and counted instances by their mangled names. The later name-index lookup used
an emitted name such as `gen_id_T` against `Workspace.functions_by_name`, whose
key is `gen_id`. A concrete `gen_id<int>` consequently reported zero.

The new `mangled_root_holes_count_concrete_instances` regression first failed
with one concrete instance present and a reported count of zero. Failure
contributions now retain their resolved `FuncRef` beside their emitted name and
diagnostic. The existing owner-indexed failure rows retain that identity too.
Counting unions those callees with matching bare declarations and deduplicates
canonical backend representatives. It scans the selected names' rows and
declarations. It requires no workspace scan or decoding of emitted names.

Changes to a declaration's live instance population also dirty its existing
hole. The test adds and removes a second specialization through real body-only
session updates with an explicit empty root delta. Counts follow `1 -> 2 -> 1`,
and each updated graph matches a fresh canonical snapshot.

Generic-root holes come only from `mangle::is_exempt` declarations, enforced by
`generic_root_obligation`. Their bare names remain lookup-compatible. Existing
root-count, backend-variant, root-addition/removal and moved-root refusal tests
remain relevant. Retained failure callees have the same declaration lifetime as
the contribution's existing call edges. Body-only updates retain declaration
identity, owner replacement removes old indexed rows, and interface changes
require a fresh graph.

The reported count is visible in `graph_snapshot::canonical`. Closure admission
uses hole membership, and diagnostic/span census uses failure sites. This repair
changes the reported count without discharging holes or changing their errors.

## Deterministic evidence

`root_hole_refresh_retains_other_owners_and_counts_backend_siblings_once`
starts with two failed owners of one backend-variant function, one other hole,
and a concrete specialization. It removes the first two owners in sequence.
The generic root remains, the other hole survives, and every update agrees with
a fresh build.

| Stage | Names | Failure rows | Declaration rows |
|---|---|---|---|
| Original and sharing-only full build | 8 | 3 | 6 |
| Original and sharing-only first owner removal | 2 | 2 | 2 |
| Original and sharing-only last owner removal | 2 | 1 | 2 |
| Corrected full build | 8 | 3 | 9 |
| Corrected first owner removal | 2 | 2 | 3 |
| Corrected last owner removal | 2 | 1 | 2 |

Declaration rows include each retained failure callee examined by the repair.
Failure rows include retraction and surviving-row aggregation. The fresh fixture
also contains two stdlib generic-root holes. The separate mangled-count test's
two local specialization edits each examine two names, one failure row and one
declaration row. These counters describe performed work, and bound this fixture.
They establish no universal incremental-complexity claim.

The baseline ran five `skull-graph` tests, 41 `graph_update` tests and four
`graph_snapshot` tests. The sharing-only implementation also passed all 57
`instance_graph` tests. The final test commands are

```sh
cargo test -p skull-graph
cargo test -p skull --test skull_tests graph_update::
cargo test -p skull --test skull_tests graph_snapshot::
cargo test -p skull --test skull_tests instance_graph::
cargo test -p skull
```

Final validation passed five `skull-graph` tests, 109 skull unit tests and 1297
skull integration tests, with one existing ignored integration test. Deliberately
replacing canonical declaration deduplication with a vector made the
owner/backend test fail with four reported instances against one expected.
Removing the touched-declaration dirty-name update made the specialization test
fail with one reported instance against two expected. Both mutations were
restored before the final full-suite pass. Formatting used the installed
Cargo 1.99.1 formatter through `cargo fmt --all`; this environment has no rustup
toolchain selector.

## Open questions

Coordinate relocation has a separate existing gap. In the owner-retention test,
replace the equal-length `mon_id` name throughout that test with `mono_id` and
run its exact filter. The first edit remains body-only, but the canonical
fresh comparison reports surviving `gen_id` and `other_id_T` failure spans one
byte behind their fresh positions. Contribution spans move while the retained
root-hole row copies remain stale. The committed test uses equal-length edits
to isolate owner retention and count refresh. Span-copy relocation needs its own
repair and regression.

No cold-compile or runtime timing claim is made. Concurrent compiler work would
make wall-time comparisons unsuitable for measuring this small change.
