# Reserved runtime field rejections

The intended boundary is exactly `__fn`, `__ty`, and `__variant`, not all
`__` names. [The lowering predicate](../crates/bone-lower/src/lib.rs) applies
it to struct fields (including `value` structs) and named enum variant fields.
The check was introduced in commit `a5652c4f`. It protects runtime marker
keys: for example, [the Lua struct constructor](../stdlib/unified.rock)
assigns `__ty`, and [the JS enum constructor](../crates/skull-codegen-js/src/lib.rs)
uses `__variant` and `__ty` for boxes. The common predicate is deliberately
broader than any one representation.

[The rejection fixture](../tests/errors/reserved_field_names.rock.err) pins
all three names for each struct form and for enum records. Each assertion
identifies both the offending name and its declaration, so an unrelated error
does not satisfy it. It does not cover tuple payloads, which have no user-named
fields.

[The accepted control](../tests/types/reserved_field_names_allowed.rock)
constructs and reads neighbouring `_fn`, `_ty`, `_variant`, `__inner`, and
`__pred` fields across struct, value, and enum records. It passed on both Lua
and JS (one test each). The rejection evidence command below passed all nine
original cases; replacing every expectation with a wrong diagnostic and
replacing every case with a valid program each yielded nine failures. These
are batch mutations, not independent per-case compiler mutations.

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/reserved_field_names.rock.err --cases 9
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/types/reserved_field_names_allowed.rock
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/types/reserved_field_names_allowed.rock --backend js
```
