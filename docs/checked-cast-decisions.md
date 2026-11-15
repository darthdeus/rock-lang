# Checked `as` rejection evidence

`as` checks an already inferred operand against a written target. The relation
admits assignment-compatible pairs and explicit numeric conversions; `::` supplies
context to its operand instead. In particular, extracting a nominal value from
`any` uses `cast<T>`, not `as`. These rules come from `check_cast` in
[inference](../crates/bone-inference/src/expr.rs) and the existing
[accepted examples](../tests/types/checked_ascription.rock).

[The rejection fixture](../tests/errors/checked_cast.rock.err) checks three boundaries:

- Five incompatible pairs: string/int in both directions, bool/int, struct/int,
  and any/struct.
- Four operands that cannot infer independently: an empty array, bare null, an
  identity lambda, and a lambda with a deferred field access. The cast target
  must not supply their missing types.
- Three target holes: an entire hole under incompatible and identity contexts,
  and a nested array element hole under an incompatible context. A target hole
  must not launder a conversion via the surrounding expectation.

Each rejection asserts the relevant conversion diagnostic and types. Removing the
calls through the rejected lambda values avoids extra non-function call errors;
the deferred field example can still additionally report an uninferred parameter.
The test checks that the desired diagnostic appears, not that it is the only one.

The accepted controls in `tests/types/checked_ascription.rock` cover resolved
array and numeric identity casts, numeric conversions, resolved nullable
widening, contextual lambda/null alternatives, and `cast<T>`
from `any`. These controls do not prove every possible assignment-compatible
pair or backend behavior. The suite's 22 tests passed on both `skull` (Lua) and
`js` when this evidence was recorded. Rerun with
`cargo run --quiet --locked -p rock-cli --bin rock -- test tests/types/checked_ascription.rock`,
then the same command with `--backend js`.

The rejection runner and its sensitivity check were run with:

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/checked_cast.rock.err --cases 12
```

The original produced 12 passes; replacing every expected diagnostic with a
wrong one produced 12 failures; replacing each program with a generic valid
`main` produced 12 failures. The latter establishes rejection sensitivity, not
that every nearby valid program is accepted; the separate accepted controls
above provide that evidence.

## Open questions

None raised by this bounded audit. The tests pin the current intended relation;
they do not establish that the relation is complete for all types.
