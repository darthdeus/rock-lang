# Generic default and method declaration rejection evidence

Fourteen existing cases now assert their intended diagnostic: seven for generic
defaults and seven for method declarations. All original programs already
rejected for the stated reasons; no wrong-error fixture required isolation.

## Generic defaults

The two [arity cases](../tests/errors/default_type_params/arity.rock.err) check
a missing required argument and too many arguments when a trailing default is
available. `lower_resolved_type_name` and the nominal type-application branch in
[lowering](../crates/bone-lower/src/lib.rs) compute the accepted argument range
from total parameters and trailing defaults, then fill omitted trailing slots.

The five [declaration cases](../tests/errors/default_type_params/decl_shape.rock.err)
check non-trailing defaults, defaults on function generics, sibling parameter
references, an unsupported array-shaped default, and an unknown named default.
In the same lowerer, `reject_generic_defaults` enforces placement,
`validate_generic_defaults` checks order and supported shapes, and
`lower_generic_defaults_for_check` resolves names at the declaration even when
the type is unused.

The [runtime corpus](../tests/types/default_type_params.rock) checks partial and
complete explicit arguments, all-defaulted bare names, builtin and named
defaults, structs/enums/newtypes, parameter annotations and nullable uses. A new
control checks that an explicit array type argument is accepted and can pass
through an inferred function generic without a default. Unsupported default
shapes are not a general restriction on explicit type arguments.

## Method declarations

The seven [method cases](../tests/errors/methods/method_syntax.rock.err) check
annotated or missing receiver parameters, macro and extern receiver syntax,
duplicate generics across receiver and method lists, a second dotted receiver
segment, and an unknown receiver type. The first six are
[parser](../crates/skull-parser/src/parser.rs) checks in function declaration
parsing and `parse_params`. The last reaches type-name resolution in the lowerer.
The fixture header now reflects that distinction.

The second-dot restriction does not establish a ban on module-qualified
receivers: the parser separately handles `lib::Type.method`. This audit does not
check that path. The [method runtime corpus](../tests/functions/method_syntax.rock)
checks dot and bare calls, receiver and method generics, overloads by receiver,
an ordinary parameter named `self`, coroutine methods and inline annotations.

## Validation and limits

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/default_type_params/arity.rock.err --cases 2
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/default_type_params/decl_shape.rock.err --cases 5
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/methods/method_syntax.rock.err --cases 7
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/types/default_type_params.rock tests/functions/method_syntax.rock
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/types/default_type_params.rock tests/functions/method_syntax.rock --backend js
```

All 14 original cases pass. Wrong-diagnostic and valid-program mutations each
fail all 14 assertions. Runtime controls pass 18/18 tests on LuaJIT and 18/18 on
JS, with zero JS skips or not-for-JS cases. These examples do not cover every
default shape, declaration kind, module-qualified receiver or parser recovery
path. No production compiler code changed.

## Resolved: builtin default disagreement

The audit found that `trailing_generic_defaults` in
[semantic passes](../crates/bone-semantic/src/semantic_passes.rs) classified
`uint` and `f64` as builtin defaults, but the authoritative lowerer whitelist
omitted both. Checking `struct Defaulted<T = uint> { value: T }` followed by
`fn main() {}` rejected with `unsupported default`; the `f64` form did too.

The reference's builtin-default rule and normal type lowering agree with the
prepass: `uint` is unsigned and `f64` aliases `float`. The validator now admits
both spellings. `f32` remains restricted to field storage, and nullable,
compound, sibling-reference and function defaults retain their restrictions.

Two new [semantic regressions](../crates/bone-semantic/tests/default_type_params.rs)
failed on the original validator and pass after the repair. Each checks struct,
enum and newtype defaults, explicit result types and both module orders. Two
[runtime tests](../tests/types/numeric_default_type_params.rock) pass on LuaJIT
and JS, checking unsigned values above the signed 32-bit range and fractional
float results through all three declaration forms. The existing 14 diagnostic
assertions and mutation controls still pass. The full bone-lower and
bone-semantic suites pass (284 tests combined). This is a correctness repair;
no performance claim is made.
