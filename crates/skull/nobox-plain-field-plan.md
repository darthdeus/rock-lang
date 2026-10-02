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

Next checkpoint: fix the shared embedded leaf layout/projection and make both
runtime oracles green. Baseline compiler is retained outside Git. MAG emission
succeeded. `compile tests/` rejects this non-package directory; comparison
therefore emits each `.rock` fixture separately and reconciles rejection names.
