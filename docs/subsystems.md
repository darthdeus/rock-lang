# Subsystem map and cleanup queue

Generate the local overview with `python3 perf/overview.py` and open
`target/inventory/overview.html`. The source of its reviewed descriptions,
ownership paths, entrypoint anchors and bounded cleanup queue is
[subsystems.json](subsystems.json). The detailed per-file inventory remains
available through `python3 perf/inventory.py`.

For the language-facing view, start with the
[compact contract](language-contract.md): selected rules for values/references,
nullability, generics and coroutines, with implementation and test links.

The map distinguishes three kinds of knowledge:

- Measured: tracked source lines, textual test markers, rejection assertions
  and declared normal Cargo dependencies.
- Reviewed: responsibilities, selected entrypoints and test locations from a
  bounded source read. Anchor checks detect drift, not semantic correctness.
- Checked: only the executable claims in [evidence.json](evidence.json).
  Results from another revision or a dirty tree are labelled stale.

This is a work map, not a complete language specification. Each file has one
measurement owner; test locations may be linked from several subsystems without
being counted repeatedly. Cross-cutting Rock corpora remain separate because
their directory names do not establish feature coverage. Upstream-derived code
and prebuilt bindings are excluded from project totals. Unmapped files remain
visible instead of being assigned a guessed responsibility.

Use the queue to assign independent scopes with observable acceptance checks.
Reproduced defects rank before speculative cleanups. Large files identify places
to read; moving their contents is not itself a simplification. Keep at most three
next actions, integrate completed fixes, then choose another bounded batch.

The reliable core is a set of checks that detect wrong behavior: fresh-build
comparisons, runtime results and diagnostic assertions with valid controls.
There is no prerequisite that every central implementation be tidy before work
can proceed elsewhere.
