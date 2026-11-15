# Iterator bound rejection evidence

The seven cases in [iterator_bound_violations.rock.err](../tests/errors/iterators/iterator_bound_violations.rock.err) now assert the intended diagnostic, rather than accepting any compiler error. The original seven diagnostics match their stated causes; no wrong-error case needed isolation. This audit does not cover the separate `bounded_call_in_nested_body` coroutine fixture.

- [Lowering](../crates/bone-lower/src/lib.rs) (`lower_generic_bounds`, `reject_generic_bounds`) accepts only `Iterator<E>` on function generics, with one type argument; an unknown bound and a bound on a struct reject.
- [Inference](../crates/bone-inference/src/iter.rs) (`check_iterator_bounds`, `bound_next_elem`) checks structural `next(C) -> (bool, E)` conformance at calls, including matching the element type. A missing `next`, mismatched element, and multi-value `next` each have distinct rejection reasons. A multi-value iterator can still drive a `for` directly.
- The same file's `resolve_for_iter_protocol` rejects iteration of an unbounded generic. [Expression inference](../crates/bone-inference/src/expr.rs) rejects a bounded function as a value because its bound calls are specialized at direct call sites.

The existing positive controls exercise direct bounded calls on vector and user iterators, combinators, forwarding through bounded functions, and a direct `for` over a three-result `next`: [combinators](../tests/iterators/combinators.rock), [bounded forwarding](../tests/iterators/bounded_forwarding.rock), and [userland](../tests/iterators/userland.rock).

Commands from the repository root:

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/iterators/iterator_bound_violations.rock.err --cases 7
cargo run --quiet --locked -p rock-cli --bin rock -- --backend skull test tests/iterators/combinators.rock tests/iterators/userland.rock tests/iterators/bounded_forwarding.rock
cargo run --quiet --locked -p rock-cli --bin rock -- --backend js test tests/iterators/combinators.rock tests/iterators/userland.rock tests/iterators/bounded_forwarding.rock
```

The rejection checker reported 7/7 original cases passed, 0/7 passed with deliberately wrong diagnostics, and 0/7 passed after replacing the programs with valid ones. Lua (skull) passed 58/58 tests across three files; the runner did not report a skip count. JavaScript passed 58/58 with 0 skipped and 0 not for JS.
