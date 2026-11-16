# Duplicate declaration rejection evidence

Nine existing cases now assert their intended diagnostic: five member-name
collisions and four type-name collisions. All original programs already
rejected for those reasons. No unrelated rejection needed isolation, and no
production compiler code changed.

## Member names

The [member fixture](../tests/errors/symbols/duplicate_members.rock.err) checks:

| Case | Intended reason |
|---|---|
| Two struct fields named `x`, with different types | ``Duplicate field `x` in struct `Foo` `` |
| Two value-struct fields named `x` | ``Duplicate field `x` in struct `P` `` |
| Two unit variants named `Tick` | ``Duplicate variant `Tick` in enum `Ev` `` |
| Tuple and unit variants both named `Tick` | ``Duplicate variant `Tick` in enum `Ev` `` |
| Two record-payload fields named `w` | ``Duplicate field `w` in variant `Rect` of enum `Shape` `` |

Each assertion includes the exact field/variant and owner name, rather than
just the word `Duplicate`. In [lowering](../crates/bone-lower/src/lib.rs),
`lower_struct_fields` checks a field name against the fields already lowered.
Value structs use that path too. `lower_enum_def` separately checks record
payload fields against earlier fields in that payload and each variant name
against the earlier variants. These rules prevent ambiguous member selection
and construction; changing a field type or variant payload does not provide a
second selectable name.

The [new accepted controls](../tests/symbols/rejection_controls.rock) check
that distinct field names work in ordinary and value structs, the same field
name works in different owners, and distinct variants can mix unit, tuple and
record shapes. They also read a repeated record-field name in separate variants
and a repeated variant name in separate enums. Member uniqueness is scoped to
its owner.

## Type names

The [symbol fixture](../tests/errors/symbols/duplicate_symbols.rock.err) checks
a repeated enum, struct and newtype name within a source file, plus a repeated
struct name across two source files. The assertions distinguish
`duplicate enum definition`, `duplicate struct definition`,
`duplicate newtype definition` and `duplicate struct ... across modules`, each
with the colliding name.

In [semantic passes](../crates/bone-semantic/src/semantic_passes.rs),
`pass_register_forward_decls` detects a second declaration in each type-kind
registration map. Workspace name registration detects the cross-module
collision in the legacy global type namespace and retains the first
declaration's workspace slot while recording
an interface error on the later module. Rejecting repeated definitions avoids
making the meaning of a type depend on which declaration wins.

The controls use distinct struct, enum and newtype names, including
`SymbolId`/`SymbolIds`, and inspect their constructed values. The existing
[newtype corpus](../tests/newtypes/basics.rock) checks 47 accepted cases, while
the [record-variant corpus](../tests/patterns/enum_record_variants.rock) checks
six accepted cases.

## Validation and limits

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/symbols/duplicate_members.rock.err --cases 5
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/symbols/duplicate_symbols.rock.err --cases 4
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/symbols/rejection_controls.rock tests/patterns/enum_record_variants.rock tests/newtypes/basics.rock
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/symbols/rejection_controls.rock tests/patterns/enum_record_variants.rock tests/newtypes/basics.rock --backend js
```

All nine original rejection cases pass. Wrong-diagnostic and generic-valid
program mutations each fail all nine assertions (five and four respectively).
Inspecting the wrong-diagnostic output also confirmed that every case reports
its intended duplicate reason without an unrelated diagnostic.

The runtime files pass 56/56 tests on LuaJIT and 56/56 on JS: three new
controls, six record-variant tests and 47 newtype tests. JS reports zero skipped
and zero not-for-JS cases. The rejection inventory delta is nine fewer unpinned
cases, with no change to the total case count.

These are legacy-syntax cases. They do not establish the complete namespace
rules, collisions between different type kinds, duplicate globals/functions,
or v1 logical-module and package scoping. The sole cross-file rejection checks
structs; enum and newtype cross-file collisions have source guards but are not
executed by these rejection cases. Mutation checks establish assertion strength;
they do not disable the compiler's duplicate guards or prove every duplicate
shape is rejected.
