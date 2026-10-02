# Nobox field containing a plain value

## Plan

1. Replace the Lua key-spelling sentinel with a runtime oracle and add mirror
   shapes in `tests/nobox_plain_value_field.rock`.
2. Make projection and assignment use the embedded child's existing leaf
   layout, including plain value descendants. Keep ordinary storage unchanged.
3. Prove the runtime cases, mutate one consumer, compare baseline/current Lua
   for MAG and every Rock fixture, and run the dispatched quick gates.

## RED checkpoint

Baseline: `a0d305da92409adb25d035daa59f9d43fa6ea431`.
`nice cargo test -p skull`: 89/89 unit and 1016/1017 integration cases passed,
with `mir_execution_oracle::corpus_execution_oracle` ignored.
The RED unit run passed 88/89; its only expected failure is
`tests::nobox_value_holding_a_plain_value_reads_the_stored_value`, with
`attempt to index local 'r0' (a nil value)`. Every formerly passing unit case
except the replaced sentinel retains its name and passes. The separate RED
integration run passed the same 1016 cases, with the same ignored case.
The new Rock fixture passes 4/5 on baseline; its expected failure is
`nobox_plain_nullable_field_reads_presence_and_leaves` (expected 6.5, got -1).
The runtime read fixture observes the nil key; some Rock mirrors are optimized
through scalar groups and already pass, so the mutation must exercise them.

The RED checkpoint is superseded by the GREEN checkpoint below. The baseline
compiler is retained outside Git. MAG emission succeeded. `compile tests/`
rejects this non-package directory; comparison
therefore emits each `.rock` fixture separately and reconciles rejection names.

## GREEN checkpoint

The defect had two paths: parameter construction already stored primitive
leaves, while literal construction kept the plain child's box. Projection
could stop at that box's logical key even when parameter construction had never
stored it. `embed_leaf_paths` now selects the existing value-shape leaf walk for
nobox fields containing plain values. Construction, null clearing, physical
projection, whole-value reads, and whole-child assignment consume this layout.
Ordinary embeds retain their declared-field layout. No new key encoding exists.

Production consumers: `embed_leaf_values` (literal and parameter constructors,
whole-field stores), `embed_leaf_paths` (nullable clearing and layout lookup),
`physical_field` (scalar place and expression consumers), and
`physical_field_layout` (value reads in `resolve.rs`, assignments in `expr.rs`).
Whole-value reads rebuild from the same `leaf_names` order that writes use.

Final quick gates: 89/89 Skull unit tests; 1016/1017 integration cases pass,
with only `mir_execution_oracle::corpus_execution_oracle` ignored; 5/5 Rock
fixture cases pass. No failures. All 1105 retained baseline test names and
statuses match, including the ignored case. The execution oracle prints 9.5.

Adversarial checks (both exact probe hunks removed, rebuilt and green):
- Constructor-side dotted keys prefixed with `broken.`: execution oracle 0/1;
  Rock 1/5. Failing Rock names: `nobox_plain_field_reads_stored_leaves`,
  `nobox_plain_field_reads_deep_plain_leaves`,
  `nobox_plain_nullable_field_reads_presence_and_leaves`, and
  `nobox_plain_field_reads_a_vec_element`.
- Assignment-side keys prefixed with `broken.`: Rock 4/5, with only
  `nobox_plain_field_reads_after_whole_child_write` failing (12.5 expected,
  old 1.5 observed). This isolates the write witness from constructor corruption.

The emission receipts below close validation. The new Vec fixture adds one
unpackaged container site; baseline and candidate census outputs match.

## Emission receipts and handoff

Both compilers use this worktree's absolute source paths and stdlib. The
baseline binary was built at `a0d305da92` before production edits; the final
binary was rebuilt after undoing both mutations. Both emitted MAG with
`--var BUILD_UNIX=0 --var BUILD_DATE=2000-01-01` in the same repository state,
so source-revision and volatile build inputs match.

Artifacts and logs: `/home/developer/tmp/rock-cb-nobox-read/`.
Commands, with the dispatched `ROCK_DIR` and `CARGO_TARGET_DIR` exported:

```sh
/home/developer/tmp/rock-cb-nobox-read/rock-baseline compile mag --var BUILD_UNIX=0 --var BUILD_DATE=2000-01-01 /home/developer/tmp/rock-cb-nobox-read/baseline-mag.lua
nice cargo run --bin rock -- compile mag --var BUILD_UNIX=0 --var BUILD_DATE=2000-01-01 /home/developer/tmp/rock-cb-nobox-read/final-mag.lua
diff -u /home/developer/tmp/rock-cb-nobox-read/baseline-mag.lua /home/developer/tmp/rock-cb-nobox-read/final-mag.lua
python3 /home/developer/tmp/rock-cb-nobox-read/emit-tests.py baseline /home/developer/tmp/rock-cb-nobox-read/rock-baseline
python3 /home/developer/tmp/rock-cb-nobox-read/emit-tests.py final /home/developer/tmp/rock-cb-nobox-read/rock-final
```

The MAG diff exits 0 and contains zero lines: **not reachable: mag emits no
changed lines**. No changed hot functions or native performance pair is needed.
Compiler timing is not measured by this worker's dispatched quick gates.

The fixture emit script enumerates all 718 `.rock` paths with `rg --files`,
uses a 60-second bound per compile under an outer 900-second bound, and records
all exit statuses. Both compilers emit 711/718; 710 pre-existing successful
artifacts are byte-identical. Identical rejection names:

- `tests/modules/chess-scoped/lib.rock`
- `tests/modules/witness/witness-legacy/main.rock`
- `tests/modules/witness/witness-lib/lib.rock`
- `tests/modules/witness/witness-lib/secret.rock`
- `tests/modules/witness/witness-v1/main.rock`
- `tests/newtype_generic_unification_reject_map_conflict.rock`
- `tests/newtype_generic_unification_reject_wrong_shape.rock`

Only `tests/nobox_plain_value_field.rock` changes: 17 added / 42 removed Lua
lines, 128 lines of unified diff including headers/context. The changed
functions are `field_holder_float_ret_FieldHolder`,
`field_x_FieldHolder_ret_float`, `field_y_FieldHolder_ret_float`,
`deep_field_x_DeepFieldHolder_ret_float`,
`deep_field_y_DeepFieldHolder_ret_float`,
`__rock_mk_nobox_plain_field_reads_deep_plain_leaves`,
`nobox_plain_nullable_field_reads_presence_and_leaves`, and
`__rock_mk_nobox_plain_field_reads_after_whole_child_write`. The deep test's
now-unused `FieldPoint_ct` constant hoist disappears. Every hunk concerns the
requested shape's construction/read/write; nullable box/branch removal and
register changes follow from optimization of the corrected leaf construction.
There are no changed lines in an unrelated shape's artifact.

The baseline and final identity census are byte-identical on the new fixture
population. Against the committed pin, only unpackaged container sites rise
132 -> 133 (total 355 -> 356), due to the Vec witness. Re-pin this in a separate
final commit; do not fold it into the Lua fix.

Implementation and evidence are complete. Next action: lead integration and
its broader gates; the separately owned SpacePoint workaround remains intact.
