# `@repr(opaque)` rejection evidence

The [annotation registry](../crates/bone-core/src/annotations.rs) admits exactly
one unnamed bare identifier, `opaque`, at parameter position. The
[body check](../crates/bone-lower/src/lib.rs) restricts it to an inline or extern
boundary. [MIR lowering](../crates/skull-mir/src/lower/nobox.rs) skips the
column-container hazard check only for the annotated declared parameter; the
result and other parameters remain checked. This is a promise by the author of
the opaque body, not compiler verification that the target code honors it. It
does not waive no-box checks. Commit `ffccd026` introduced the contract; see
also [compiler.md](compiler.md#opaque-body-boundaries).

The [seven error cases](../tests/errors/annotations/repr_opaque.rock.err) pin
one specific diagnostic apiece: Rock body, function item, struct field, missing
argument, unknown kind, string kind, and extra arguments. They all use the
`Generic` error kind, which alone cannot distinguish the intended error from an
unrelated rejection. The test below also checks that changing every expected
message and replacing every case with a valid program fail all seven cases.

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/annotations/repr_opaque.rock.err --cases 7
# original: 7 passed; 0 failed; 0 todo
# wrong-diagnostic: 0 passed; 7 failed; 0 todo
# valid-programs: 0 passed; 7 failed; 0 todo

cargo test --locked -p skull repr_opaque_param
# integration: 11 passed, 0 failed, 0 ignored, 1288 filtered out
# library: 0 selected, 109 filtered out

cargo run --quiet --locked -p rock-cli --bin rock -- test tests/vectors/push_contract.rock --backend skull
# 5/5 passed, 0 failed, 1 file
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/vectors/push_contract.rock --backend js
# 5/5 passed, 0 failed, 0 skipped, 0 not for js, 1 file
```

The [Rust controls](../crates/skull/tests/repr_opaque_param.rs) include a Lua
inline payload admitted with the annotation, the same payload rejected without
it, a separate unannotated slot still rejected, and an equality check for the
annotated and unannotated no-box outcomes. The
[push controls](../tests/vectors/push_contract.rock) exercise column payloads,
column receivers, scalar/boxed values and nested vectors on both backends.
Compiler-routed operations can bypass the opaque call, so these results do not
prove arbitrary inline or extern code respects its promise, nor do they test
all extern boundaries or backend-specific inline bodies.
