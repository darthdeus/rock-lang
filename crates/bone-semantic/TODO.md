- sub-millisecond patching by recompiling just one fn
  - if it's within a fn body, recompile only the fn

- remove duplicateion in lower_struct_def/enum_def with generics? maybe for fns too

- remove `is_null` from refinements & refinement tests

- userland iterators via traits (base-iterator arc ACTIVE on `traits` branch -- implementation guide: docs/iterator-implementation-plan.md; only generic combinators stay deferred until trait bounds land)
  - `for x in coll` desugars to `let it = iter(coll); loop { let (ok, x) = next(it); if !ok break; body }`
  - two free fns `iter`/`next` (UFCS), `next -> (bool, E)` lowered to Lua MULTI-RETURN (zero-alloc); skip iters MUST be fetch-outside (else interpreter blacklist at sparse holes)
  - generic `map`/`filter` need an `Iterator<E>` trait bound (monomorphized, static); direct for-loops work magic-name w/o traits, so it can stage: range-for -> direct collection-for -> generic combinators
  - compatible with CFG rewrite: pure front-end desugar + stdlib, NO backend changes -- land after the simple compiler. Only backend prereqs are general (multi-return, struct mutation, destructuring)
  - full design + LuaJIT benchmarks: docs/iterator-desugaring-investigation.md

- there is resolve_ty_fueled in skull, why?

- get rid of "enumerate" hack and fix it properly
