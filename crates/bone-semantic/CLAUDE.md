# Bone typechecker

Semantic fixtures live in `tests/fixtures/`; structural checks live in
`tests/structural.rs`. Keep typechecker tests here rather than in the
`bone-semantic` or `bone-inference` source trees.

Bone is the only typechecker. A CLI probe observes current behavior; the
language reference defines intended behavior. Resolve disagreements explicitly.

Use the existing re-inference and re-lowering work counters for incremental
reuse assertions, rather than timing thresholds that vary with machine load.

Compound types contain arena-local child indices. Copying them into another
arena requires `reify`, which recursively remaps those indices; a shallow copy
can leave dangling child references. Fix malformed types where they are built,
rather than guarding later readers of the broken type.
