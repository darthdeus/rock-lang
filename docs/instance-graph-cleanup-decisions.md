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

Count-repair validation passed five `skull-graph` tests, 109 skull unit tests and 1297
skull integration tests, with one existing ignored integration test. Deliberately
replacing canonical declaration deduplication with a vector made the
owner/backend test fail with four reported instances against one expected.
Removing the touched-declaration dirty-name update made the specialization test
fail with one reported instance against two expected. Both mutations were
restored before the final full-suite pass. Formatting used the installed
Cargo 1.99.1 formatter through `cargo fmt --all`; this environment has no rustup
toolchain selector.

## Span relocation repair

The subsequent `root_hole_spans_follow_insertions_and_deletions` regression
reproduced stale failure positions after inserting or deleting a byte and after
inserting or deleting two lines. In each case a later same-name hole and a
different-name hole retained their contributions while their public failure
spans kept the old coordinates. The canonical fresh-build comparison detected
every mismatch. The test also retains an unaffected hole in another file and
multiple failures with different names in one owner.

`root_hole_sites` now stores owner-local row indices into the contribution's
`always_emitted_holes`, removing its cloned failure records. Relocation moves
the contribution spans once and dirties names whose spans moved. The existing
name-scoped refresh republishes their failures from those authoritative rows.
Owner replacement removes the old row indices before installing replacements,
and owner removal removes its indexed rows.

Coordinate passes skip edits whose byte and line deltas are both zero.
`shift_span` otherwise returns true for eligible spans even when neither
coordinate changes, which unnecessarily dirtied later holes and widened the
existing equal-length fixture's census from two examined names to four.

The test checks both offset and line relocation and full canonical equality.
Neither retained owner is rewalked or retracted. Each update examines four
names across the two refresh passes, against five hole names in the graph.
It examines four failure rows, including any retracted row, and three
declaration rows after insertion or four after deletion. The other file's hole
stays outside the refreshed names. These are deterministic fixture work counts.

Run the regression with

```sh
cargo test -p skull --test skull_tests graph_update::root_hole_spans_follow_insertions_and_deletions -- --exact
```

Final `cargo test -p skull-graph -p skull --quiet` validation passed five graph
tests, 109 skull unit tests and 1298 skull integration tests, with one existing
ignored integration test. Formatting and whitespace checks also passed.

No cold-compile or runtime timing claim is made. Concurrent compiler work would
make wall-time comparisons unsuitable for measuring this small change.
