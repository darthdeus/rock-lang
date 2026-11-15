# Control-flow rejection boundaries

The eight cases in [if_subexpressions.rock.err](../tests/errors/control_flow/if_subexpressions.rock.err)
now assert their diagnostic text, rather than accepting any rejection. All eight
already failed for the intended reason; none needed an unrelated error removed.

| Cases | Intended rule and diagnostic |
| --- | --- |
| First three: `continue`, `break`, `return` in a tuple RHS element | ``control flow (return/break/continue) is not allowed in an `if` used as a tuple element of a destructuring assignment`` |
| Next three: value `if` without `else` | `` `if` without a final `else` cannot be used as a value `` |
| Last two: no enclosing loop | `` `break`/`continue` is only valid inside a loop body `` |

The [`reject_cf_in_destructure_rhs`](../crates/bone-inference/src/purity.rs) applies to an
`if` directly inside the tuple-literal RHS of destructuring. It runs before
inference of that RHS; the first two cases have enclosing loops so loop-context
errors cannot substitute for this rule. This guard and the missing-else rule
were already present in [the initial compiler import](https://github.com/darthdeus/rock-lang/blob/78b22639/bone-inference/src/lib.rs#L437).

The [position rule](../crates/bone-inference/src/expr.rs) requires a final
`else` when an `if` produces a value, including a binding initializer. Discarded
statement `if`s allow no `else` and do not join their branch values. The
[semantic fixture](../crates/bone-semantic/tests/fixtures/if_statement_vs_value.rock)
records this as an intentional language decision, adopted 2026-06-10, rather
than accidental parity with the retired checker. The
[loop-context checks](../crates/bone-inference/src/expr.rs) reject
`break`/`continue` outside a loop, in statement and expression forms.

[Eight runtime controls](../tests/control_flow/if_rejection_controls.rock) pair
these boundaries: move exits before tuple destructuring, use missing-else `if`s
as statements, and put closed `if`s containing loop exits inside loops. Both
exit and fallthrough paths are exercised. They passed **8/8 on Lua and 8/8 on
JS**, with **0 JS skips and 0 backend exclusions**. A temporary copy with
`assert(false)` inserted into every test produced **0/8 passed (8 failed) on each backend**, with
zero JS skips; the passing counts therefore represent executed tests.

Run from the checkout root:

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/control_flow/if_subexpressions.rock.err --cases 8
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/control_flow/if_rejection_controls.rock
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/control_flow/if_rejection_controls.rock --backend js
```

The rejection evidence reports original **8 passed**, wrong-diagnostic mutation
**8 failed**, and valid-program mutation **8 failed**, all with zero TODOs.

## Open questions

This is not evidence of a blanket ban on control flow in every subexpression.
For example, `let x = if cond { continue } else { 1 }` (or `break`) inside a
loop currently fails with `type mismatch: expected int, found ()` (type names
are backtick-quoted in the diagnostic). The
[block inference](../crates/bone-inference/src/expr.rs) recognizes a final
statement `return` as diverging, but treats final statement `break`/`continue`
as unit, although their expression forms infer `never`. This adjacent behavior
is recorded for review, not pinned as an intended rejection or changed here.
