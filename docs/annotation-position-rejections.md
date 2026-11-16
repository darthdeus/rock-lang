# Annotation position rejection evidence

Thirteen existing cases now assert their intended registry diagnostic: four
[field cases](../tests/errors/annotations/field_position.rock.err), five
[item cases](../tests/errors/annotations/item_position.rock.err), and four
[parameter cases](../tests/errors/annotations/param_position.rock.err).
Every original case already rejected for the stated reason. Checking each
program with only its offending annotation removed admitted all thirteen, so
none needed competing-error isolation. No production compiler code changed.

## Rules and enforcement

The [annotation registry](../crates/bone-core/src/annotations.rs) centralizes
these decisions in `validate_annotations` and `validate_one`. `AnnoPos::describe`
names the positions in diagnostics. The `at` helper checks a single allowed
position, and `no_args` rejects arguments on nullary annotations.

| Position | Cases and intended reason | Registry branch |
|---|---|---|
| Field | Unknown name; `@nomangle` and `@attr` in the wrong position; argument on `@flat` | Unknown-name fallback; `nomangle`; `attr` role group; `flat` field group |
| Item | Unknown name; argument on `@ignore`; `@instance` and `@uniform` on a function; `@nomangle` on an enum | Unknown-name fallback; function-only nullary group; parameter-only groups; `nomangle` |
| Parameter | Unknown name; `@nomangle` in the wrong position; string argument to `@size`; argument on `@attr` | Unknown-name fallback; `nomangle`; `size` integer-shape check; parameter role group |

`@nomangle` accepts functions and structs, rather than functions alone. The
field and parameter fixture comments now describe that boundary accurately.
`@flat` accepts struct/value fields without arguments. `@attr`, `@uniform` and
`@instance` accept function parameters without arguments; `@size` requires
exactly one positional integer argument. These registry checks establish
annotation admission, not the downstream meaning of every accepted marker.

[Function lowering](../crates/bone-lower/src/lib.rs) validates `FnItem` and
`Param` in `build_function`, collecting registry errors in the lowering error
channel. `lower_struct_fields` validates `StructField`, covering both struct and
value declarations. The non-function annotation pass in
[semantic passes](../crates/bone-semantic/src/semantic_passes.rs), immediately
before `ws.log_time("annotations", gap_timer)`, validates `EnumItem` and other
non-function item positions. Thus the enum case reaches a separate entry path
from the four function-item cases while sharing the registry rule.

## Accepted controls

The new [nearby controls](../tests/annotations/rejection_controls.rock) add
three runtime tests: `@nomangle` on an ordinary function and struct, nullary
`@flat` on a value field, and nullary parameter roles alongside integer
`@size(32)`. Existing [registration tests](../tests/annotations/registration.rock)
exercise six user-decorator cases, distinguishing an unknown annotation from a
declared `deco init`. Existing [ignore tests](../tests/annotations/ignore.rock)
run two ordinary tests and exclude two `@ignore` tests that would fail if run.

These files pass 11/11 executed tests on LuaJIT and 11/11 on JS across three
files. JS reports zero skips and zero not-for-JS tests. The two ignored tests
are excluded from both executed totals; they are not JS emitter skips. The new
file supplies three tests per backend. CPU admission and execution of `@attr`,
`@uniform`, `@instance` and `@size` do not verify shader semantics. These
controls do not validate GLSL role/layout semantics, all annotation names, enum
record-variant fields, decorator parameters, or every non-function item kind.

## Runnable checks

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/annotations/field_position.rock.err --cases 4
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/annotations/item_position.rock.err --cases 5
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/annotations/param_position.rock.err --cases 4
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/annotations/rejection_controls.rock tests/annotations/registration.rock tests/annotations/ignore.rock
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/annotations/rejection_controls.rock tests/annotations/registration.rock tests/annotations/ignore.rock --backend js
```

The originals pass 4, 5 and 4 cases respectively, with no failures or TODOs.
Wrong-diagnostic and valid-program mutations each fail all 13 assertions. This
strengthens 13 existing cases and adds no rejection cases: the total inventory
is unchanged and the unpinned count falls by 13. The checks establish the
listed boundaries; they do not certify the complete registry or parser recovery.
